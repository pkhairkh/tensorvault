# Kernel Audit — Wave 12

**Date:** Wave 12 baseline audit
**Host:** AMD EPYC-Turin (Zen 5), AVX-512F + AVX-512_VPOPCNTDQ
**Commit audited:** `9040832` (prior to Wave 12)
**Auditor:** subagent-w12

This document inventories every kernel in `src/kernel/`, measures its raw
throughput on a 1 M-cell synthetic column, and determines whether it is
reachable from the TPC-H execution path (`engine::QueryEngine` →
`engine::tpch::TpchExec` → `engine::executor::*`).

---

## 1. Inventory

The `src/kernel/` module exposes 19 `Kernel` trait implementations across 5
files. A 6th file (`cpu.rs`) provides CPU detection only and defines no
kernels. The `KernelTable` (`mod.rs`) registers all 19 at startup so they are
introspectable, but only **one** is selected by the TPC-H executor (see §3).

### 1.1 `scan.rs` — predicate scans (9 kernels)

| Kernel | Operator | CPU target | Tier | SIMD instructions | I/O |
|---|---|---|---|---|---|
| `ScanEqScalar` | `ScanEqU64` | Scalar | L3 | (none — branchless scalar) | `&[u64] → KernelResult{count,mask}` |
| `ScanRangeScalar` | `ScanRangeU64` | Scalar | L3 | (none) | `&[u64] → count` |
| `ScanMultiPredicateScalar` | `ScanMultiPredicate` | Scalar | L3 | (none) | `&[u64] → count` |
| `ScanEqAvx2` | `ScanEqU64` | X86Avx2 | L3 | `_mm256_set1_epi64x`, `_mm256_loadu_si256`, `_mm256_cmpeq_epi64`, `_mm256_movemask_epi8`, `popcnt` | `&[u64] → count` |
| `ScanEqAvx512L3` | `ScanEqU64` | X86Avx512 | L3 | `_mm512_set1_epi64`, `_mm512_loadu_epi64`, `_mm512_cmpeq_epi64_mask`, `popcnt` | `&[u64] → count+mask` |
| `ScanEqAvx512Ddr5` | `ScanEqU64` | X86Avx512 | Ddr5 | same as L3 + `_mm_prefetch` (4-page T0 hint) | `&[u64] → count` |
| `ScanEqAvx512Cxl` | `ScanEqU64` | X86Avx512 | Cxl | same as L3 + `_mm_prefetch` (8-page T0 hint) | `&[u64] → count` |
| `ScanRangeAvx512L3` | `ScanRangeU64` | X86Avx512 | L3 | `_mm512_cmpge_epi64_mask`, `_mm512_cmple_epi64_mask`, `popcnt` | `&[u64] → count` |
| `ScanMultiPredicateAvx512` | `ScanMultiPredicate` | X86Avx512 | L3 | `_mm512_cmpeq/cmpgt/cmplt_epi64_mask`, `_mm512_ternarylogic_epi64` (imm=0x80, fused AND), `popcnt` | `&[u64] → count` |

### 1.2 `aggregate.rs` — sum & count-distinct (4 kernels)

| Kernel | Operator | CPU target | Tier | SIMD instructions | I/O |
|---|---|---|---|---|---|
| `SumF64Scalar` | `AggregateSumF64` | Scalar | L3 | (none) | `&[u64 bits] → sum:f64` |
| `SumF64Avx2` | `AggregateSumF64` | X86Avx2 | L3 | `_mm256_setzero_pd`, `_mm256_loadu_si256`, `_mm256_castsi256_pd`, `_mm256_add_pd`, `_mm256_storeu_pd` | `&[u64 bits] → sum:f64` |
| `SumF64Avx512` | `AggregateSumF64` | X86Avx512 | L3 | `_mm512_setzero_pd`, `_mm512_loadu_epi64`, `_mm512_castsi512_pd`, `_mm512_add_pd`, `_mm512_storeu_pd` | `&[u64 bits] → sum:f64` |
| `CountDistinctScalar` | `AggregateCountDistinct` | Scalar | Ddr5 | (none — `HashSet`-backed) | `&[u64] → count` |

### 1.3 `hash.rs` — hash build & probe (3 kernels + `HashTable` struct)

| Kernel | Operator | CPU target | Tier | SIMD instructions | I/O |
|---|---|---|---|---|---|
| `HashBuildScalar` | `HashBuild` | Scalar | Ddr5 | (none — `std::HashMap`) | `&[u64] → *mut HashTable` (written to `output`) |
| `HashProbeScalar` | `HashProbe` | Scalar | L3 | (none — `HashMap::get`) | `[ptr][probe_keys] → count` |
| `HashProbeAvx512` | `HashProbe` | X86Avx512 | L3 | (none — **delegates to scalar**; AVX-512 SwissTable path is a stub) | same as scalar |

Note: `AlignedSlot` (64-byte cache-line-aligned struct) is defined in `hash.rs`
but **not yet used** by any kernel — it is preparation for a future SwissTable.

### 1.4 `similarity.rs` — Hamming distance (2 kernels)

| Kernel | Operator | CPU target | Tier | SIMD instructions | I/O |
|---|---|---|---|---|---|
| `HammingScalar` | `SimilarityHamming` | Scalar | L3 | `u64::count_ones` (POPCNT) | `&[u64] → count` |
| `HammingAvx512` | `SimilarityHamming` | X86Avx512 | L3 | `_mm512_set1_epi64`, `_mm512_loadu_epi64`, `_mm512_xor_epi64`, **`_mm512_popcnt_epi64`** (AVX-512_VPOPCNTDQ), `_mm512_cmple_epi64_mask`, `popcnt` | `&[u64] → count` |

### 1.5 `leapfrog.rs` — worst-case-optimal join (1 kernel + `LeapfrogJoin` struct)

| Kernel | Operator | CPU target | Tier | SIMD instructions | I/O |
|---|---|---|---|---|---|
| `LeapfrogScalar` | `LeapfrogJoin` | Scalar | L3 | (none — binary-search leapfrog) | `[left u64][right u64] → count` |

Note: `LeapfrogJoin` (the standalone struct that supports N-way intersection)
is **not** a `Kernel` trait impl — it's a higher-level API for the executor to
call directly when the planner emits a worst-case-optimal join node. The
`LeapfrogScalar` kernel is a 2-way wrapper for symmetry with the kernel table.

### 1.6 `cpu.rs` — CPU detection (no kernels)

Defines `CpuTarget` enum (`Scalar`, `X86Avx2`, `X86Avx512`, `ArmNeon`, `ArmSve`)
and `detect_cpu()` which probes CPUID at startup. On this AMD EPYC-Turin host
both `avx512f` and `avx512vpopcntdq` are detected, so `detect_cpu()` returns
`CpuTarget::X86Avx512`.

---

## 2. Raw Throughput Measurements

Benchmark: `examples/bench_kernels_raw.rs`. 1 M-cell synthetic column
(`i % 100`), best-of-5 iterations, release profile (`opt-level=3`,
`lto=fat`, `codegen-units=1`).

**Peak reference:** 8 lanes × 2.0 GHz = **16.0 G cells/sec** for both u64 scans
and f64 aggregates (one cell per lane per cycle for these ops).

| # | Kernel | Target | Tier | Cells/sec | Peak | % Peak | Notes |
|---|---|---|---|---|---|---|---|
| 1 | `scan_eq_scalar` | scalar | L3 | 2.149e9 | 16.0e9 | 13.4% | auto-vectorized by rustc |
| 2 | `scan_eq_avx2_l3` | x86-avx2 | L3 | 3.656e8 | 16.0e9 | 2.3% | hand-written intrinsics slower than auto-vec |
| 3 | `scan_eq_avx512_l3` | x86-avx512 | L3 | 8.606e8 | 16.0e9 | 5.4% | inner loop is sound, see §4 |
| 4 | `scan_eq_avx512_ddr5` | x86-avx512 | Ddr5 | 8.567e8 | 16.0e9 | 5.4% | 4-page SW prefetch (no DRAM residency — runs from L3) |
| 5 | `scan_eq_avx512_cxl` | x86-avx512 | Cxl | 8.569e8 | 16.0e9 | 5.4% | 8-page SW prefetch (same caveat) |
| 6 | `scan_range_scalar` | scalar | L3 | 2.603e9 | 16.0e9 | 16.3% | auto-vectorized |
| 7 | `scan_range_avx512_l3` | x86-avx512 | L3 | 4.987e8 | 16.0e9 | 3.1% | two mask cmps + AND per batch |
| 8 | `scan_multi_predicate_scalar` | scalar | L3 | 1.162e9 | 16.0e9 | 7.3% | 3 predicates AND-combined |
| 9 | `scan_multi_predicate_avx512_l3` | x86-avx512 | L3 | 4.906e9 | 16.0e9 | 30.7% | `VPTERNLOGQ` fusion — best AVX-512 scan |
| 10 | `sum_f64_scalar` | scalar | L3 | 2.160e9 | 16.0e9 | 13.5% | auto-vectorized |
| 11 | `sum_f64_avx2` | x86-avx2 | L3 | 4.719e8 | 16.0e9 | 2.9% | single accumulator chain |
| 12 | `sum_f64_avx512` | x86-avx512 | L3 | 8.391e8 | 16.0e9 | 5.2% | single accumulator chain |
| 13 | `hamming_scalar` | scalar | L3 | 1.429e9 | 16.0e9 | 8.9% | scalar POPCNT per cell |
| 14 | `hamming_avx512_l3` | x86-avx512 | L3 | 9.579e9 | 16.0e9 | **59.9%** | `VPOPCNTDQ` — **best % peak** |
| 15 | `hash_build_scalar` | scalar | Ddr5 | 7.940e7 | 16.0e9 | 0.5% | `std::HashMap` insert — not SIMD |
| 16 | `hash_probe_scalar` | scalar | L3 | 1.775e8 | 16.0e9 | 1.1% | `HashMap::get` — random-access latency bound |
| 17 | `hash_probe_avx512_l3` | x86-avx512 | L3 | 1.774e8 | 16.0e9 | 1.1% | delegates to scalar (no SIMD path) |
| 18 | `count_distinct_scalar` | scalar | Ddr5 | 1.765e8 | 16.0e9 | 1.1% | `HashSet` insert — random-access bound |
| 19 | `leapfrog_scalar` | scalar | L3 | 3.915e7 | 16.0e9 | 0.2% | 2-way leapfrog, binary-search bound |

### 2.1 Key observations

1. **`hamming_avx512_l3` is the only kernel near peak (59.9%).** `VPOPCNTDQ`
   is the standout: 8 lanes of popcount per cycle, fully utilized. This is the
   one kernel that justifies its SIMD inner loop.
2. **`scan_multi_predicate_avx512_l3` is the second-best (30.7%).** The
   `VPTERNLOGQ` fusion of three predicate masks into one AND is a real win
   over the scalar 3-comparison loop.
3. **All other AVX-512 kernels under-perform the scalar auto-vectorized
   baseline.** `scan_eq_avx512_l3` (5.4%) is slower than `scan_eq_scalar`
   (13.4%). Root cause: the hand-written AVX-512 inner loops use a single
   accumulator (`count += mask.count_ones()`) which serializes POPCNT, and
   they don't unroll across multiple ZMM registers. The scalar loop is
   auto-vectorized by rustc with `-Copt-level=3` and gets better scheduling.
   **This is a tuning opportunity, not a correctness bug.**
4. **`sum_f64_avx512` (5.2%) is much slower than `sum_f64_scalar` (13.5%).**
   The kernel uses a single `__m512d` accumulator, creating a 6-cycle
   dependency chain on `VADDPD`. The fix is 4-8 independent accumulators
   (one per ZMM) with a final horizontal sum — standard FMA-loop unrolling.
5. **Hash and count-distinct kernels are random-access-latency-bound, not
   lane-bound.** The 0.5–1.1% of peak is expected: `std::HashMap` and
   `std::HashSet` do pointer chasing. The "AVX-512" `HashProbeAvx512` is a
   stub that delegates to scalar — a real SwissTable with `VPCMPEQB` metadata
   scan is future work (Wave 4, per `hash.rs` doc comment).
6. **`leapfrog_scalar` at 0.2% of peak** is the slowest. It does binary
   search per seek on two slices — `O(AGM)` work, but each step is a
   high-latency dependent branch. This is acceptable for a worst-case-optimal
   join (it's asymptotically better than hash join on cyclic queries), but
   the constant factor is large.

---

## 3. Wired-In Audit — Dead Code Inventory

### 3.1 Definition

A kernel is **"wired in"** if it is selected by the TPC-H execution path:

```
engine::QueryEngine::execute(sql)
  → engine::tpch::TpchExec::execute(query)
    → engine::executor::execute_count(...)        // only kernel call site
      → kernel_table.select(Operator::ScanEqU64, MemoryTier::L3)
      → kernel.execute(...)
```

This is the **only** `.select(Operator::*, tier)` call site in
`src/engine/`. (The alternative executors `src/executor/{eddy,pipeline,
scheduler,worker}.rs` also call `kernel_table.select(...)` but they are
**not** invoked by `engine::QueryEngine` — they are exercised only by
benches and unit tests. The planner's `Lowerer::pick_best_tier` calls
`select` for cost estimation but does not call `execute`.)

### 3.2 Reachability matrix

| Kernel | Registered in table | Selected by TPC-H executor? | Status |
|---|---|---|---|
| `ScanEqScalar` | yes (`ScanEqU64`, Scalar, L3) | yes — fallback when `detect_cpu() != X86Avx512` | **LIVE (fallback)** |
| `ScanEqAvx512L3` | yes (`ScanEqU64`, X86Avx512, L3) | **YES — primary on this Zen 5 host** | **LIVE (primary)** |
| `ScanEqAvx2` | yes (`ScanEqU64`, X86Avx2, L3) | no — never selected (executor hardcodes L3, and `detect_cpu()` returns `X86Avx512` here) | **DEAD CODE** |
| `ScanEqAvx512Ddr5` | yes (`ScanEqU64`, X86Avx512, Ddr5) | no — no executor ever requests `MemoryTier::Ddr5` | **DEAD CODE** |
| `ScanEqAvx512Cxl` | yes (`ScanEqU64`, X86Avx512, Cxl) | no — no executor ever requests `MemoryTier::Cxl` | **DEAD CODE** |
| `ScanRangeScalar` | yes (`ScanRangeU64`, Scalar, L3) | no — executor never selects `ScanRangeU64` | **DEAD CODE** |
| `ScanRangeAvx512L3` | yes (`ScanRangeU64`, X86Avx512, L3) | no — same | **DEAD CODE** |
| `ScanMultiPredicateScalar` | yes (`ScanMultiPredicate`, Scalar, L3) | no — executor never selects `ScanMultiPredicate` | **DEAD CODE** |
| `ScanMultiPredicateAvx512` | yes (`ScanMultiPredicate`, X86Avx512, L3) | no — same | **DEAD CODE** |
| `SumF64Scalar` | yes (`AggregateSumF64`, Scalar, L3) | no — TPC-H sum path uses row-by-row scalar (`execute_sum`), not the kernel | **DEAD CODE** |
| `SumF64Avx2` | yes (`AggregateSumF64`, X86Avx2, L3) | no — same | **DEAD CODE** |
| `SumF64Avx512` | yes (`AggregateSumF64`, X86Avx512, L3) | no — same | **DEAD CODE** |
| `HammingScalar` | yes (`SimilarityHamming`, Scalar, L3) | no — TPC-H has no Hamming-similarity query | **DEAD CODE** |
| `HammingAvx512` | yes (`SimilarityHamming`, X86Avx512, L3) | no — same | **DEAD CODE** |
| `HashBuildScalar` | yes (`HashBuild`, Scalar, Ddr5) | no — TPC-H joins use nested-loop, not hash | **DEAD CODE** |
| `HashProbeScalar` | yes (`HashProbe`, Scalar, L3) | no — same | **DEAD CODE** |
| `HashProbeAvx512` | yes (`HashProbe`, X86Avx512, L3) | no — same (and it's a stub anyway) | **DEAD CODE** |
| `CountDistinctScalar` | yes (`AggregateCountDistinct`, Scalar, Ddr5) | no — TPC-H `COUNT(DISTINCT ...)` uses `Vec::dedup` in `tpch.rs:1992`, not the kernel | **DEAD CODE** |
| `LeapfrogScalar` | yes (`LeapfrogJoin`, Scalar, L3) | no — TPC-H joins are nested-loop | **DEAD CODE** |

### 3.3 Summary

- **LIVE (reachable from TPC-H executor):** 2 kernels — `ScanEqAvx512L3`
  (primary on Zen 5) and `ScanEqScalar` (fallback on non-AVX-512 hosts).
- **DEAD CODE (defined & registered but never invoked by `tpch.rs` or
  `dispatch.rs`):** 17 kernels.

The dead-code list is not a defect — many of these kernels are intended for
future query shapes (range scans, multi-predicate filters, hash joins,
worst-case-optimal joins, similarity search) that the TPC-H executor does
not yet emit. The audit establishes the **baseline**: as of commit
`9040832`, only the equality-scan kernel is wired into the TPC-H path, and
even it is under-tuned (5.4% of peak — see §2.1 point 3).

### 3.4 TPC-H `COUNT(DISTINCT ...)` confirmation

The `tpch.rs` aggregate path at line 1992 reads:
```rust
AggFunc::CountDistinct => {
    let mut values: Vec<u64> = ...;
    values.sort_unstable();
    values.dedup();
    // count = values.len()
}
```
This is a sort-then-dedup, **not** the `CountDistinctScalar` kernel (which
uses `HashSet`). So `CountDistinctScalar` is confirmed dead on the TPC-H
path.

---

## 4. Tuning opportunities (informational, not Wave 12 scope)

These are not action items for Wave 12 (audit-only wave) but are recorded
here so future waves can pick them up:

1. **`ScanEqAvx512L3` / `ScanRangeAvx512L3` / `SumF64Avx512` — multiple
   accumulators.** The single-accumulator inner loops serialize on the
   `VADDPD` / `POPCNT` latency. Use 4-8 independent ZMM accumulators and
   horizontally reduce at the end. Expected speedup: 4-8×, bringing these
   kernels to 30–60% of peak (matching `HammingAvx512`).
2. **`HashProbeAvx512` — implement the SwissTable.** The current impl
   delegates to scalar. The `AlignedSlot` struct (64-byte cache-line-aligned)
   is already defined for this purpose. A real SwissTable with 1-byte
   metadata and `VPCMPEQB` probing would close the gap.
3. **`ScanEqAvx512Ddr5` / `ScanEqAvx512Cxl` — measure on real DRAM/CXL
   data.** The current bench runs from L3 (1 M cells = 8 MB, fits in L3).
   To validate the prefetch distances, the bench needs a column larger than
   L3 (≥ 64 MB) backed by the appropriate tier.
4. **Wire `ScanRangeU64`, `ScanMultiPredicate`, `AggregateSumF64` into the
   TPC-H executor.** These are the highest-leverage dead-code kernels —
   `ScanMultiPredicateAvx512` already hits 30.7% of peak and would
   accelerate TPC-H Q19 (OR-of-AND predicates) and Q5 (multi-predicate
   join keys).

---

## 5. Reproduction

```sh
cd /root/turbogp
cargo run --release --example bench_kernels_raw
cargo test --lib    # must report 772+ passing
```

The bench prints the table from §2 to stdout. The audit doc lives at
`docs/kernel-audit.md` (this file).
