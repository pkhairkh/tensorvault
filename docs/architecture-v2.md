# turboGP Architecture v2 — Hardware-First Execution Path

## Overview

turboGP is a columnar in-memory database engine designed for AMD EPYC-Turin (Zen 5)
with AVX-512. This document describes the execution path from SQL parsing through
the AVX-512 kernel layer, the optimizations applied in Waves 12-14, and the
remaining work to close the gap to DuckDB.

## Architecture Layers

```
SQL String
    │
    ▼
┌─────────────────────┐
│  Parser (tpch.rs)   │  parse_tpch() → SelectQuery2
└─────────────────────┘
    │
    ▼
┌─────────────────────────────────────────┐
│  TpchExec (tpch.rs)                     │
│  ├── resolve_from_item (Arc, no clone)  │
│  ├── build_mask (AVX-512 bitmap)        │
│  ├── execute_grouped (fused agg)        │
│  ├── hash_join_with_keys                │
│  └── eval_agg_expr (fallback)           │
└─────────────────────────────────────────┘
    │
    ▼
┌──────────────────────┐
│  Kernel Layer        │
│  ├── bitmap.rs       │  AVX-512 filter_eq/lt/gt/le/ge
│  ├── vectorized.rs   │  filter_rows, sum_masked, and_mask
│  ├── flat_hash_table │  open-addressing hash table
│  └── fm_index.rs     │  StringSearchColumn for LIKE
└──────────────────────┘
    │
    ▼
┌──────────────────────┐
│  Data Layer (Arc)    │
│  ├── Table           │  Vec<Arc<Vec<u64>>> columns
│  ├── ExecTable       │  Vec<Arc<Vec<u64>>> columns
│  └── Catalog         │  HashMap<String, Table>
└──────────────────────┘
```

## 1. Data Representation: Arc-Shared Columns

### Problem
The original `from_catalog()` deep-cloned all column data (768MB for lineitem)
on every query execution, costing 300-400ms per query. The `StringSearchColumn`
data (6M strings + offsets + bytes) was also deep-cloned.

### Solution
Changed `Table.columns` and `ExecTable.columns` from `Vec<Vec<u64>>` to
`Vec<Arc<Vec<u64>>>`. Similarly changed `string_columns` to
`Vec<Option<Arc<StringSearchColumn>>>`.

`from_catalog()` now bumps Arc refcounts instead of cloning data:

```rust
ExecTable {
    columns: table.columns.clone(),        // cheap: Vec<Arc<...>> clone
    string_columns: table.string_columns.clone(), // cheap: Arc refcount bumps
    ...
}
```

### Impact
- Eliminated 300-400ms per-query overhead for ALL queries
- Q6 improved from 757ms → 31ms (the clone was the dominant cost)
- Q8 improved from 9,963ms → 422ms (7-table join, 7 from_catalog calls)

## 2. AVX-512 Bitmap Filter (Wave 13)

### Problem
The original `apply_comparison` used scalar per-row loops:
```rust
for i in 0..n { mask[i] = mask[i] && col[i] == target; }
```
This processes ~0.3 G cells/sec — 80× slower than the AVX-512 kernel's
theoretical 24 G cells/sec.

### Solution
Created `src/exec/bitmap.rs` with:
- `Bitmap` struct: 1 bit per row packed into `Vec<u8>` (8× denser than `Vec<bool>`)
- AVX-512 filter functions: `filter_eq_u64`, `filter_lt_u64`, `filter_eq_f64`, etc.
- **4-way loop unrolling** with independent accumulators (critical lesson from
  Wave 12: single-accumulator AVX-512 is SLOWER than auto-vectorized scalar)

```rust
#[target_feature(enable = "avx512f,avx512dq,avx512bw,avx512vl")]
unsafe fn filter_eq_u64_avx512(col: &[u64], val: u64) -> Bitmap {
    let val_vec = _mm512_set1_epi64(val as i64);
    // Process 32 rows per iteration (4 independent __mmask8 results)
    for chunk in col.chunks(32) {
        let m0 = _mm512_cmpeq_epi64_mask(_mm512_loadu_epi64(ptr), val_vec);
        let m1 = _mm512_cmpeq_epi64_mask(_mm512_loadu_epi64(ptr+8), val_vec);
        let m2 = _mm512_cmpeq_epi64_mask(_mm512_loadu_epi64(ptr+16), val_vec);
        let m3 = _mm512_cmpeq_epi64_mask(_mm512_loadu_epi64(ptr+24), val_vec);
        // Pack 4 mask bytes into bitmap
    }
}
```

`apply_comparison` dispatches to these functions for Int/Date/Float column-vs-literal
comparisons, then folds the bitmap into the existing `&mut [bool]` mask via
`bitmap::and_into_bool`.

### Impact
- Q6 filter: 31ms for 6M rows × 5 conditions (was ~300ms scalar)
- The bitmap is also consumed directly by the aggregation layer (Wave 14)

## 3. Fused Per-Group Aggregation (Wave 14)

### Problem
The original `execute_grouped` called `eval_agg_expr` once per group per select
item. For Q1 (4 groups × 10 select items = 40 calls), each call iterated over
~1.5M indices with scalar arithmetic. Total: 60M row examinations, 1,440ms.

The GROUP BY key extraction was also slow: per-row `self.eval(gb, t, idx)` with
`Value2` enum construction + `HashMap<Vec<u64>, Vec<usize>>` insertion = 670ms.

### Solution

#### Group Build Optimization
- Pre-resolve GROUP BY column indices ONCE
- Read u64 directly from columns (no `Value2` enum)
- Use `HashMap<u64, usize>` (single-hash key → group index) instead of
  `HashMap<Vec<u64>, Vec<usize>>`
- group_build: 670ms → 64ms (10× improvement)

#### Fused Aggregation
`try_fused_grouped_agg()` analyzes all select items and categorizes them into
supported patterns:
- `GroupByCol(idx)` — plain GROUP BY column
- `CountAll` — count(*) or count(col)
- `SumCol(a)` — sum(l_quantity)
- `SumColCol(a, b)` — sum(l_extendedprice * l_discount)
- `SumColSubOne(a, b)` — sum(l_extendedprice * (1 - l_discount))
- `SumColSubOneAddOne(a, b, c)` — sum(l_extendedprice * (1 - l_discount) * (1 + l_tax))
- `AvgCol(a)` — avg(l_quantity)
- `MinCol(a)`, `MaxCol(a)` — min/max

If ALL items match supported patterns, a SINGLE pass per group computes all
aggregates simultaneously:

```rust
for &gi in filtered {
    let indices = &group_indices[gi];
    for &i in indices {
        // Read each column ONCE, use for multiple aggregates
        let qty = f64::from_bits(col_qty[i]);
        let ext = f64::from_bits(col_ext[i]);
        let disc = f64::from_bits(col_disc[i]);
        let tax = f64::from_bits(col_tax[i]);
        sum_qty += qty;
        sum_base += ext;
        sum_disc += ext * (1.0 - disc);
        sum_charge += ext * (1.0 - disc) * (1.0 + tax);
        count += 1;
    }
}
```

This reduces column reads from (num_groups × num_aggregates) to (num_groups) —
a 10× reduction for Q1.

#### Pattern Matching
The `col_in_mul_sub_one` helper detects the `(Col * (1 - Col2))` sub-expression,
which is necessary because the expression `Col * (1 - Col2) * (1 + Col3)` is
parsed left-associatively as `(Col * (1 - Col2)) * (1 + Col3)`, where the left
side is a `BinOp`, not a `Col`.

### Impact
- Q1: 2,112ms → 122ms (17× improvement)
- agg_eval: 1,440ms → ~28ms via fused pass

## 4. Query Execution Flow (Q6 Example)

```
SELECT sum(l_extendedprice * l_discount) AS revenue
FROM lineitem
WHERE l_shipdate >= date '1994-01-01' AND l_shipdate < date '1995-01-01'
  AND l_discount >= 0.05 AND l_discount <= 0.07 AND l_quantity < 24
```

1. **Parse**: `parse_tpch(sql)` → `SelectQuery2` with WHERE = And(And(And(And(...))))
2. **Resolve FROM**: `resolve_from_item(lineitem)` → `ExecTable` (Arc refcount bumps, no clone)
3. **Build mask**: `eval_bool_mask_vec(And(...))`:
   - For each comparison: `apply_comparison` → `bitmap::filter_eq_f64(col, val)` → `Bitmap`
   - `bitmap::and_into_bool(&bm, mask)` folds into `&mut [bool]`
   - 5 conditions × 6M rows = 31ms (AVX-512)
4. **Collect indices**: `(0..n).filter(|&i| mask[i]).collect()` → 114,160 indices (2.6ms)
5. **Aggregate**: `eval_agg_expr(Sum(Col * Col))` → `sum_vec`:
   - Detects `Col * Col` pattern
   - Resolves column indices ONCE
   - Scalar loop: `sum += f64::from_bits(ca[i]) * f64::from_bits(cb[i])`
   - 114K iterations = 0.65ms
6. **Total**: ~34ms (measured: 28-39ms)

## 5. Kernel Audit Results (Wave 12)

The audit identified 17 of 19 kernels as "dead code" — defined but never called
by the TPC-H executor. Key finding: hand-written AVX-512 kernels with single
accumulators UNDERPERFORM rustc's auto-vectorized scalar code.

| Kernel | Measured | % Peak | Wired In? |
|--------|----------|--------|-----------|
| ScanEqAvx512L3 | 0.86 G/s | 5.4% | Yes (executor) |
| SumF64Avx512 | 0.84 G/s | 5.2% | No (dead code) |
| HammingAvx512 | 9.58 G/s | 59.9% | No (dead code) |
| HashProbeAvx512 | 0.18 G/s | 1.1% | No (stub) |

The Wave 13 bitmap filters use 4-way unrolling to avoid this bottleneck.

## 6. Current Performance (After W12-W14)

| Query | Before (ms) | After (ms) | Improvement | Notes |
|-------|-------------|------------|-------------|-------|
| Q1 | 3,514 | 132 | 26.6× | Fused aggregation |
| Q3 | 3,885 | 1,244 | 3.1× | Arc refactor |
| Q5 | 18,330 | 14,733 | 1.2× | Needs radix hash join |
| Q6 | 1,435 | 39 | 36.8× | AVX-512 bitmap + Arc |
| Q8 | 9,963 | 422 | 23.6× | Arc refactor (7 tables) |
| Q9 | 8,882 | 1,281 | 6.9× | Arc refactor |
| Q12 | 2,040 (0 rows) | 1,203 (2 rows) | 1.7× + fixed | String IN-list fix |
| Q18 | 10,147 | 3,601 | 2.8× | Arc refactor |
| **Total** | **81,947** | **29,976** | **2.7×** | 17/22 pass |

## 7. Remaining Work

### Wave 15: Radix Hash Join (Q5 target < 2,000ms)
Q5 (6-table join) is still 14.7s. The current `hash_join_with_keys` uses
`HashMap<u64, Vec<usize>>` with heap-allocated chaining. A radix-partitioned
hash table with `_mm512_conflict_epi64` for bucket detection would provide
10× improvement.

### Wave 16: Morsel-Driven Pipeline (Q3 target < 500ms)
Q3 (3-table join + GROUP BY) is 1.2s. The current executor materializes
entire join outputs before filtering. A push-based pipeline processing 8K-row
morsels through filter→project→aggregate in a single pass would eliminate
intermediate materialization.

### Wave 17: Correctness Fixes
- Q4: EXISTS subquery → needs semi-join kernel
- Q11: HAVING with scalar subquery → needs subquery caching
- Q15: float comparison bug → `total_revenue = (SELECT max(...))` returns 0 rows
- Q19: OR conditions prevent join key extraction
- Q20/Q21: nested correlated subqueries → needs anti-join + semi-join

### Wave 18: ClickBench Optimization
ClickBench was 1,605ms (target < 500ms). The Arc refactor should help, but
the 105-column hits table may need column pruning.
