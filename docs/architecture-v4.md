# turboGP Architecture v4 — TQFT-Inspired Optimization

## Overview

This document covers the TQFT-inspired optimizations implemented in Waves
28-33, building on the v3 architecture. The work applies concepts from
**Topological Quantum Field Theory** — specifically the Atiyah-Segal axioms
and Frobenius algebra structure — to database query execution.

## TQFT → Database Concept Mapping

The full mapping is in `docs/tqft-mapping.md`. Key applications:

| TQFT Concept | Database Technique | Wave | Impact |
|---|---|---|---|
| Wilson loop (holonomy pre-check) | Bloom filter semi-join | W29 | Q5: 8.3× |
| Frobenius ε (counit/trace) | Float epsilon comparison | W32 | Q15 fix (partial) |
| Topological invariance | Zone maps data skipping | W33 | Module ready |
| Cobordism gluing | Morsel pipeline | W30 | Future |
| Frobenius μ (multiplication) | Hash join + bloom | W29 | Implemented |

## Wave 29: Wilson-Loop Bloom Filter (Biggest Win)

### TQFT Basis
The Wilson loop is a topological observable that tests whether a path is
contractible (trivial holonomy) **without computing the full holonomy**.
It's a "topological pre-check" that exploits structure to avoid expensive
computation.

### Database Application
In a hash join, the "full holonomy" is the hash table probe — a random
memory access costing ~200-400 cycles. The "Wilson loop" is a **bloom
filter** that tests whether a join key is definitely absent from the build
side in ~5 instructions (L1 cache resident).

### Implementation (`src/exec/bloom_filter.rs`)
```rust
pub struct BloomFilter {
    bits: Vec<u64>,        // bit array, power-of-2 size
    num_hashes: usize,     // 3-5 double-hashing positions
}

// Insert: CRC32 double-hashing (h1 = crc32(key), h2 = h1 >> 32)
// Set bits at h1, h1+h2, h1+2*h2, ...
pub fn insert(&mut self, key: u64);

// Check: returns false = definitely absent, true = might be present
// Hot path: <5 instructions, L1-resident
#[inline]
pub fn might_contain(&self, key: u64) -> bool;
```

### Integration in `hash_join_with_keys`
```rust
// Build phase: construct both hash table AND bloom filter
let mut build_hash = JoinHashTable::new(build_side.row_count);
let mut bloom = BloomFilter::new(build_side.row_count);
for r in 0..build_side.row_count {
    let k = build_side.columns[bk0][r];
    build_hash.insert(k, r as u32);
    bloom.insert(k);  // Wilson loop construction
}

// Probe phase: check bloom BEFORE hash table
for p in 0..probe_side.row_count {
    let probe_key = ...;
    if !bloom.might_contain(probe_key) {
        continue;  // Wilson loop says absent — skip hash table probe
    }
    // Only probe hash table for ~10% of keys (selective joins)
    build_hash.probe_all(probe_key, &mut matched_rows);
}
```

### Impact
- **Q5: 4,200ms → 505ms (8.3×)** — region filter narrows to 1 nation,
  so 90%+ of probe keys are absent. Bloom skips them before touching
  the hash table directory.
- **Q10: 880ms → 625ms (1.4×)** — moderate selectivity
- **Q3: 740ms → 732ms (marginal)** — 2-table join, less selective

### Total Q5 Improvement
- Mission start: 18,330ms
- After W13 (Arc refactor): 14,733ms
- After W15 (cardinality join): 4,200ms
- After W29 (bloom filter): **505ms**
- **Total: 36.3× improvement**

## Wave 32: Float Epsilon Comparison (TQFT Counit)

### TQFT Basis
The counit ε: A→k is the "trace" or "dimension" map. In TQFT, this should
be **topologically invariant** — the exact representation shouldn't matter,
only the value.

### Database Application
Float sums computed via different code paths (fused aggregation vs scalar
`eval_agg_expr`) may differ by 1-2 ULPs due to different summation orders.
Q15's `total_revenue = (SELECT max(total_revenue) FROM ...)` compares a
column value to a subquery result — exact bit comparison fails.

### Fix
Added `filter_eq_f64_epsilon` with relative tolerance 1e-6:
```rust
pub fn filter_eq_f64_epsilon(col: &[u64], val: f64) -> Bitmap {
    let abs_tol = 1e-6 * val.abs().max(1.0);
    for (i, &c) in col.iter().enumerate() {
        if (f64::from_bits(c) - val).abs() <= abs_tol { bm.set(i); }
    }
}
```

### Status
The epsilon comparison is correct, but Q15 still returns 0 rows due to a
**separate join bug**: the `s_suppkey = supplier_no` join produces 0 rows
because the derived table alias `supplier_no` isn't resolved correctly as
a join key. This is a deeper parser/executor issue.

## Wave 33: Zone Maps (Topological Invariance)

### TQFT Basis
A topological invariant is **metric-independent** — it doesn't depend on
the specific geometry, only the topology. Zone maps let the scan skip
entire pages of data that cannot possibly match, regardless of individual
row values.

### Implementation (`src/exec/zone_map.rs`)
```rust
pub struct ZoneMap {
    mins: Vec<u64>,  // per-page minimum (1024 rows/page)
    maxs: Vec<u64>,  // per-page maximum
}

// O(1) page overlap check
pub fn page_might_contain_range(&self, page_idx: usize, lo: u64, hi: u64) -> bool {
    self.maxs[page_idx] >= lo && self.mins[page_idx] <= hi
}
```

### Projected Impact (not yet integrated)
For Q6 (6M rows, 1-year date range filter):
- Without zone maps: scan all 6M rows, filter each
- With zone maps: check 5,860 page ranges, skip ~90% → scan ~600K rows
- **Projected: Q6 32ms → ~10ms (3×)**

Integration requires storing zone maps in `ExecTable` and modifying
`build_mask` to check zone maps before scanning each page.

## Current Performance (W33)

| Query | Mission Start | After W12-15 | After W28-33 | Total Improvement |
|-------|:-:|:-:|:-:|:-:|
| Q1 | 3,514ms | 132ms | 126ms | 28× |
| Q3 | 3,885ms | 1,111ms | 732ms | 5.3× |
| **Q5** | **18,330ms** | **4,467ms** | **505ms** | **36.3×** |
| Q6 | 1,435ms | 32ms | 32ms | 45× |
| Q8 | 9,963ms | 283ms | 289ms | 34× |
| Q10 | 929ms | 880ms | 625ms | 1.5× |
| Q12 | 2,040ms (0 rows) | 1,163ms (2 rows) | 1,163ms | 1.8× + fixed |
| **TPC-H total** | **81,947ms** | **17,260ms** | **~13,000ms** | **~6.3×** |
| Tests | 772 | 801 | 823 | +51 |

## Repository
- **GitHub**: `pkhairkh/tensorvault` (branch: `main`)
- **Latest commit**: `23f2c23`
- **Total commits this session**: 5 (W28-W33)
- **Test count**: 823 (up from 811 at W25 start)
- **Documentation**: `docs/tqft-mapping.md`, `docs/architecture-v4.md`

## Remaining Work
1. **Zone map integration** into `build_mask` (store in ExecTable)
2. **Q15 join bug** — derived table alias resolution
3. **Q4/Q11/Q19/Q20/Q21** — semi-join, anti-join, OR-split (Frobenius Δ)
4. **VNNI/BF16 deep integration** into fused aggregation (W31)
5. **Morsel-driven pipeline** (W30 — cobordism gluing axiom)
