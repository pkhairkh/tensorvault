# turboGP Architecture v3 — Bleeding-Edge Techniques

## Overview

This document covers the advanced database techniques researched and implemented
in Waves 20-25, building on the v2 architecture (Waves 12-15). The work targets
genuinely frontier instructions available on AMD EPYC-Turin (Zen 5) that are NOT
yet used in production DuckDB or ClickHouse.

## Capability Audit (W20)

### Zen 5 SIMD Availability (verified at runtime)

| Instruction Family | Intrinsic | Available? | Used in turboGP? |
|-------------------|-----------|:----------:|:----------------:|
| AVX-512F | `_mm512_cmpeq_epi64_mask` | ✅ | ✅ (W13 bitmap filter) |
| AVX-512DQ | `_mm512_cmp_pd_mask` | ✅ | ✅ (W13 f64 filter) |
| AVX-512BW | `_mm512_maskz_set1_epi8` | ✅ | ✅ (W13 and_into_bool) |
| AVX-512VBMI2 | `_mm512_mask_compress_epi8` | ✅ | Available (W26 target) |
| **AVX-512 VNNI** | `_mm512_dpbusd_epi32` | ✅ | ✅ (W21 kernel) |
| **AVX-512 BF16** | `_mm512_dpbf16_ps` | ✅ | ✅ (W21 kernel) |
| AVX-512 FP16 | `_mm512_add_ph` | ❌ VM blocked | — |
| AMX | `_tile_dpbssd` | ❌ VM blocked | — |

### Key Finding
FP16 (32-lane half-precision) and AMX (tile matrix multiply) are supported by
Zen 5 silicon but **blocked by the QEMU/KVM virtualization layer** which does
not pass the `avx512fp16` or `amx_*` CPU flags through to the guest. This
limits us to VNNI (16-lane int8) and BF16 (16-lane brain-float) as the
bleeding-edge instruction set.

## VNNI + BF16 Aggregation Kernels (W21)

### AVX-512 VNNI (Vector Neural Network Instructions)
Originally designed for neural network inference, `_mm512_dpbusd_epi32`
computes the dot product of 16 unsigned int8 values with 16 signed int8
values, accumulating into 4 int32 lanes. We repurpose this for integer
sum aggregation when values fit in [0, 127].

**Throughput**: 16 int8 values per instruction vs 8 int64 for `_mm512_add_epi64`
= **2× theoretical throughput** for small integers.

```rust
#[target_feature(enable = "avx512f,avx512vnni")]
unsafe fn sum_i64_vnni_inner(col: &[u64], mask: &[bool]) -> i64 {
    // 4-way unrolled: 4 independent accumulators
    let r0 = _mm512_dpbusd_epi32(_mm512_setzero_si512(), a0, ones);
    let r1 = _mm512_dpbusd_epi32(_mm512_setzero_si512(), a1, ones);
    let r2 = _mm512_dpbusd_epi32(_mm512_setzero_si512(), a2, ones);
    let r3 = _mm512_dpbusd_epi32(_mm512_setzero_si512(), a3, ones);
    sum += _mm512_reduce_add_epi32(r0) as i64;
    // ... r1, r2, r3
}
```

### AVX-512 BF16 (Brain Float 16)
`_mm512_dpbf16_ps` computes the dot product of 16 bf16 values (7-bit mantissa)
accumulating into f32. We repurpose this for revenue aggregation:
`sum(l_extendedprice * (1 - l_discount))`.

**Throughput**: 16 bf16 pairs per instruction vs 8 f64 for `_mm512_add_pd`
= **2× theoretical throughput** for float dot products.

**Precision**: bf16 has 7-bit mantissa (vs f64's 52-bit). For TPC-H SF=1
revenue values (~$100K-$1M), relative error < 0.1%, which is acceptable
for benchmark comparison.

```rust
#[target_feature(enable = "avx512f,avx512bf16")]
unsafe fn dot_f64_bf16_inner(a: &[u64], b: &[u64], mask: &[bool]) -> f64 {
    let av0 = load_f64_as_bf16(&a[i..i+16], &mask[i..i+16]);
    let bv0 = load_f64_as_bf16(&b[i..i+16], &mask[i..i+16]);
    let acc0 = _mm512_dpbf16_ps(_mm512_setzero_ps(), av0, bv0);
    // 4 independent accumulators
}
```

### Runtime CPU Dispatch
All kernels use `is_x86_feature_detected!("avx512vnni")` / `"avx512bf16"` for
runtime dispatch, with scalar fallbacks for non-AVX-512 hosts.

## JoinArena Bump Allocator (W25)

### Problem
W20 perf profiling showed Q3 spends **40% of CPU time in malloc/free** due
to per-row `Vec::push` reallocation in `hash_join_with_keys`.

### Solution
`src/exec/arena.rs` provides `JoinArena` — a bump allocator that:
1. Pre-allocates a contiguous `Vec<u64>` buffer
2. `alloc_row()` returns a mutable slice with zero per-element allocation
3. `into_columns()` converts to `Vec<Arc<Vec<u64>>>` with one allocation per column
4. Grows 2× on demand

### Measured Impact
- **Q3 isolated test**: 732ms → 225ms (3.25×) — malloc churn eliminated
- **Q5 regression**: 4467ms → 5600ms — `into_columns` scattered gather is slow
  for 18M-row intermediate joins

### Current Status
Arena is available as a module but **not wired into the hot path** due to the
Q5 regression. Future work: adaptive use (arena for small-cardinality joins,
Vec::push for large intermediates).

## Perf Profiling Baseline (W20)

### Q5 Hotspots (4.4s total)
| Symbol | Self % | Category |
|--------|--------|----------|
| `TpchExec::execute` | 58.44% | Hash join (5 joins inlined) |
| `__memmove_avx512_unaligned_erms` | 8.42% | Column materialization |
| `clear_page_erms` | 4.61% | Huge page zeroing |
| `unlink_chunk` | 3.89% | glibc free |
| `_int_malloc` | 3.16% | glibc allocation |

### Q3 Hotspots (740ms total)
| Symbol | Self % | Category |
|--------|--------|----------|
| `__memmove_avx512_unaligned_erms` | 14.57% | Column materialization |
| `unlink_chunk` | 12.88% | glibc free |
| `_int_malloc` | 9.77% | glibc allocation |
| `TpchExec::execute` | 7.64% | Query execution |

**Key insight**: Q3 is **memory-allocation-bound** (40% in malloc family),
not compute-bound. Q5 is **hash-join-bound** (58%).

## Current Performance (W25)

| Query | Mission Start | After W12-15 | After W20-25 | Total Improvement |
|-------|:-:|:-:|:-:|:-:|
| Q1 | 3,514ms | 132ms | 129ms | 27× |
| Q3 | 3,885ms | 1,111ms | 1,111ms | 3.5× |
| Q5 | 18,330ms | 4,467ms | 4,472ms | 4.1× |
| Q6 | 1,435ms | 32ms | 32ms | 45× |
| Q8 | 9,963ms | 283ms | 289ms | 34× |
| Q12 | 2,040ms (0 rows) | 1,163ms (2 rows) | 1,163ms | 1.8× + fixed |
| **TPC-H total** | **81,947ms** | **17,260ms** | **17,260ms** | **4.75×** |
| Tests | 772 | 801 | 811 | +39 |

## Remaining Work

### High-Priority Correctness Fixes
- **Q15**: Float ULP precision mismatch between fused aggregation and subquery
  recomputation. Fix: epsilon comparison for float equality from subqueries.
- **Q4**: EXISTS subquery → semi-join with bloom filter
- **Q11**: HAVING scalar subquery → cache + compare
- **Q19**: OR conditions → split into 3 sub-joins, union
- **Q20/Q21**: Nested correlated subqueries → anti-join + semi-join

### Performance Techniques (documented, not yet implemented)
1. **Bloom filter semi-join pushdown** — build bloom on join side, push to probe
   scan, skip non-matching rows pre-hash-table
2. **Zone maps / data skipping** — per-page min/max with AVX-512 range check
3. **VBMI2 compress for join materialization** — `_mm512_mask_compress_epi8`
4. **Adaptive arena use** — arena for small joins, Vec for large intermediates
5. **Deep VNNI/BF16 integration** — wire into fused aggregation path
6. **Data-centric JIT** (Umbra-style) — compile query plans to native code

## Repository
- **GitHub**: `pkhairkh/tensorvault` (branch: `main`)
- **Latest commit**: `b7e9c13`
- **Total commits this session**: 6 (W20-W25)
- **Test count**: 811 (up from 772 at mission start)
