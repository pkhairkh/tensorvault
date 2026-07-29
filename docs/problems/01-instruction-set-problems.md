# Instruction Set Problems

> Problems related to the kernel table: hand-tuned instruction sequences per
> (CPU vendor, CPU generation, memory tier) tuple. The kernel table is the
> engine's competitive moat — getting these right is what makes the
> instruction-first architecture pay off.
>
> **Research source**: `docs/cpu_energy_kb.md` (per-instruction energy, latency,
> throughput for Intel Ice Lake/Sapphire Rapids/Emerald Rapids, AMD Zen 3/4/5).

---

## P-01-01: Per-tier scan kernel differentiation 🔴

**Layer**: Instruction sets
**Status**: 🔴 open (🟡 partial — L3 kernel exists, DDR5/CXL kernels need validation)
**Math**: none
**Effort**: M (1–3 months)
**Impact**: high

### Problem

The same operator (`scan_eq`, `scan_range`, `hash_probe`) needs different
kernels for different memory tiers because the optimal prefetch distance,
batch size, and SIMD width depend on the tier's latency and bandwidth:

| Tier | Latency | Optimal prefetch | Optimal batch |
|------|---------|-----------------|---------------|
| L3 | ~15 ns | 1 page | 8 cells (1 ZMM) |
| DDR5 | ~90 ns | 4 pages | 8 cells, pipelined |
| CXL | ~250 ns | 8 pages | 8 cells, deep pipeline |
| NVMe | ~20 µs | async I/O | 4 KB (page-granular) |

The current codebase has L3, DDR5, and CXL variants of `scan_eq`, but they
are unvalidated against real tier-resident data (we don't have a CXL device
in the test environment).

### Open questions

- Does the 8-page prefetch for CXL actually hide the ~250 ns latency, or do
  we need 16 pages?
- Should the NVMe kernel use `io_uring` or `SPDK` for zero-copy page reads?
- How do we benchmark a CXL kernel without CXL hardware? (Fallback: emulate
  CXL latency with `memsleep` — a mmap'd file with artificial delay.)

### Success criteria

- Each tier-specific kernel achieves ≥ 80% of the memory bandwidth for that tier.
- The kernel selection is automatic: `KernelTable::select(op, tier)` returns
  the right kernel based on the detected data placement.

---

## P-01-02: BMI2 PEXT/PDEP landmine on AMD Zen/Zen2 🔴

**Layer**: Instruction sets
**Status**: 🔴 open
**Math**: none
**Effort**: S (< 1 month)
**Impact**: high

### Problem

`PEXT` and `PDEP` (BMI2 instructions) are 3-cycle, 1/cycle throughput on
Intel and AMD Zen 3+. But on AMD Zen/Zen+/Zen 2, they are microcoded at
~18-cycle latency, ~250× worse throughput.

If we use `PEXT` for bit extraction (e.g., in the bit-sliced index or in
NaN-box tag extraction) without guarding, we silently lose 250× performance
on Zen/Zen2 CPUs.

### Open questions

- Should we ship a software fallback for Zen/Zen2, or just refuse to use BMI2
  on those CPUs?
- How do we benchmark this? Need a Zen 2 machine (or a Zen 2 core in a cloud
  instance).

### Success criteria

- `KernelTable::new()` detects Zen/Zen2 and disables BMI2 kernels.
- A software `pext_u64` fallback is registered for those CPUs.

---

## P-01-03: AVX-512 frequency throttling on older Intel 🟡

**Layer**: Instruction sets
**Status**: 🟡 partial (we detect AVX-512 but don't account for throttling)
**Math**: none
**Effort**: S
**Impact**: medium

### Problem

On Skylake-X / Cascade Lake (pre-Ice Lake), AVX-512 512-bit instructions
trigger a **license-level downclock** of 300–500 MHz. The AVX-512 kernel is
faster per-instruction but the whole core runs slower — net win is unclear.

Sapphire Rapids reduced the offset to ~100 MHz. Zen 4 and Zen 5 have zero
penalty.

### Open questions

- Should we use 256-bit AVX2 kernels on Skylake-X instead of 512-bit AVX-512?
- How do we model the frequency hit in the cost model?

### Success criteria

- The CPU detection module (`src/kernel/cpu.rs`) distinguishes
  Skylake-X/Cascade Lake from Ice Lake+ and picks the right SIMD width.

---

## P-01-04: ARM NEON / SVE kernel port 🟡

**Layer**: Instruction sets
**Status**: 🟡 partial (CPU detection exists, no ARM kernels)
**Math**: none
**Effort**: L (3–6 months)
**Impact**: high

### Problem

The kernel table currently has x86-64 kernels (AVX-512, AVX2) and a scalar
fallback. ARM (NEON, SVE) is detected but has no kernels — we fall back to
scalar on ARM, losing 4–8× throughput.

ARM is relevant for:
- AWS Graviton 3/4 (Neoverse N1/V2)
- Apple Silicon (M2/M3/M4 — UMA architecture, 800 GB/s memory bandwidth)
- Ampere Altra / One

### Open questions

- SVE2 has variable-length vectors — how do we write kernels that adapt?
- Apple Silicon's UMA means no NUMA; the memory manager needs a different
  default placement policy.

### Success criteria

- `scan_eq`, `sum_f64`, `hash_probe`, `similarity_hamming` all have NEON
  kernels.
- SVE2 kernels for Graviton 4.
- The smoke test runs on Apple Silicon.

---

## P-01-05: VPTERNLOGQ for multi-predicate fusion 🔴

**Layer**: Instruction sets
**Status**: 🔴 open
**Math**: none
**Effort**: M
**Impact**: high

### Problem

`VPTERNLOGQ` (AVX-512) fuses any 3-input bitwise truth table into one
instruction (latency 1, throughput 0.5 on Intel). It's the cheapest-per-joule
instruction in the knowledgebase.

A query like `WHERE x > 100 AND y < 50 AND z != 0` compiles to three
predicates that currently run as three separate scan kernels. With
`VPTERNLOGQ`, we could fuse them into a single pass:

```
vpcmpleq k1, zmm_x, zmm_100    ; x > 100
vpcmpgtq  k2, zmm_y, zmm_50    ; y < 50
vpcmpeqq  k3, zmm_z, zmm_0     ; z != 0
vpternlogq k4, k1, k2, k3, 0xE8 ; (k1 AND k2) AND k3
```

### Open questions

- How does the planner know when to fuse predicates?
- What's the breakeven: 2 predicates vs 3 vs 4?

### Success criteria

- A `scan_multi_predicate` kernel that takes a predicate DAG and emits
  fused `VPTERNLOGQ` instructions.
- Benchmark: 3-predicate scan at ≥ 1.5× the throughput of 3 separate scans.

---

## P-01-06: VPOPCNTDQ for vectorized Hamming distance 🟡

**Layer**: Instruction sets
**Status**: 🟡 partial (kernel exists, unvalidated on real VPOPCNTDQ hardware)
**Math**: III (probability — for similarity join confidence bounds)
**Effort**: S
**Impact**: medium

### Problem

`VPOPCNTDQ` (AVX-512_VPOPCNTDQ, Ice Lake+ / Zen 5) does vectorized
population count across 8×64-bit lanes per cycle. The `HammingAvx512` kernel
exists in `src/kernel/similarity.rs` but falls back to scalar on CPUs without
VPOPCNTDQ.

### Open questions

- Is the fallback path correct on Zen 4 (which has AVX-512F but not
  VPOPCNTDQ)?
- Should we use the `VCNT` NEON instruction for the ARM equivalent?

### Success criteria

- `is_x86_feature_detected!("avx512vpopcntdq")` gates the kernel correctly.
- Benchmark on Ice Lake or Zen 5 shows ~8 G cells/sec.

---

## P-01-07: Cross-vendor kernel benchmarking harness 🔴

**Layer**: Instruction sets
**Status**: 🔴 open
**Math**: none
**Effort**: M
**Impact**: high

### Problem

The kernel table claims throughputs (19 G cells/sec for `scan_eq` on L3,
etc.) but these are theoretical, not measured. We need a benchmark harness
that:

1. Runs each kernel on each supported CPU (Intel Ice Lake, Sapphire Rapids,
   Zen 4, Zen 5, Apple M4, Graviton 4).
2. Measures actual throughput, energy (via RAPL on x86), and tail latency.
3. Updates the kernel table's metadata with measured numbers.

### Open questions

- Can we use cloud instances (AWS, Azure, GCP) for the benchmark matrix?
- How do we get reproducible energy measurements? (AMD RAPL is a model, not
  a measurement — see `cpu_energy_kb.md` §2.3.)

### Success criteria

- A `benches/kernel_matrix.rs` that runs all kernels × all tiers × all CPUs.
- A CSV output with measured throughput and energy.
- The kernel table's `name()` method includes the measured throughput.

---

## P-01-08: Instruction cache pressure for large kernel tables 🔴

**Layer**: Instruction sets
**Status**: 🔴 open
**Math**: none
**Effort**: M
**Impact**: medium

### Problem

As the kernel table grows (16 kernels today, 50+ planned), the total
instruction footprint may exceed the L1 instruction cache (32 KB). If the
scheduler dispatches different kernels in quick succession, we get I-cache
misses that dominate the hot loop.

### Open questions

- How many kernels can we have before I-cache pressure matters?
- Should we group kernels by operator (all `scan_eq` variants together) to
  improve locality?

### Success criteria

- A benchmark that measures I-cache miss rate as the kernel count grows.
- A kernel layout strategy (e.g., function-sections + linker script) that
  keeps hot kernels in the same I-cache region.

---

## P-01-09: Branchless hot loops — eliminate mispredicts 🔴

**Layer**: Instruction sets
**Status**: 🔴 open
**Math**: none
**Effort**: M
**Impact**: high

### Problem

A mispredicted branch costs 15–21 cycles + ~2–4 nJ (see `cpu_energy_kb.md`
§1.9). In the current scan kernels, the tail loop (processing remaining cells
after the SIMD chunk) has branches that mispredict on the last iteration.

```rust
while i < cells.len() {
    if cells[i] == target { count += 1; }  // branch on every cell
    i += 1;
}
```

### Open questions

- Can we eliminate all branches in the hot loop using CMOV + mask accumulatic?
- What's the measured mispredict rate on real workloads?

### Success criteria

- The scan and aggregate kernels are verified branchless via `perf stat`
  (branches / branch-misses).
- Measured mispredict rate < 0.1% on uniform data.

---

## P-01-10: REP MOVSB for bulk page copy 🟢

**Layer**: Instruction sets
**Status**: 🟢 solved (documented in `cpu_energy_kb.md` §1.7)
**Math**: none
**Effort**: —
**Impact**: low

### Problem (solved)

Bulk page copy (e.g., during region migration) should use `REP MOVSB` with
ERMS (Fast Short REP MOV), which achieves ~1 byte/cycle and is
hardware-prefetched.

### Resolution

Documented in the knowledgebase. The memory manager's `migrate_to()` should
use `memcpy` (which compilers lower to `REP MOVSB` on x86 with ERMS).

---

## P-01-11: Split LOCK avoidance 🔴

**Layer**: Instruction sets
**Status**: 🔴 open
**Math**: none
**Effort**: S
**Impact**: critical

### Problem

A split LOCK (atomic access crossing a cache line boundary) costs 3,000–10,000
cycles + ~50–200 nJ on Ice Lake+ (see `cpu_energy_kb.md` §1.8). This is the
single most expensive operation on modern x86.

If any kernel or data structure has an unaligned atomic, we silently hit
this. The hash table's `LOCK CMPXCHG` for slot insertion is the likely
culprit.

### Open questions

- How do we statically guarantee all atomics are cache-line-aligned?
- Should we use `#[repr(align(64))]` on all atomic-containing structs?

### Success criteria

- `cargo build` with `-C debug-assertions=on` catches unaligned atomics.
- A test that runs the hash-join kernel under `perf stat` shows zero
  split-lock events.

---

## P-01-12: SIMD-amortized batch size tuning 🟡

**Layer**: Instruction sets
**Status**: 🟡 partial (we use 1024–4096 rows per batch, industry standard)
**Math**: none
**Effort**: S
**Impact**: low

### Problem

SIMD setup cost (load constants, align, loop tail handling) is ~5–15 cycles.
Break-even is at ~64–128 elements for AVX-512 (see `cpu_energy_kb.md` §8.2).

We currently process 4 KB pages (504 cells) — well past break-even. But for
the executor's cross-operator pipeline, the batch size between operators
(e.g., scan → filter → aggregate) affects cache residency.

### Open questions

- Should the batch size be adaptive (smaller for L1-resident pipelines,
  larger for DRAM-resident scans)?
- How does this interact with the trace JIT's specialization?

### Success criteria

- The executor exposes a configurable batch size.
- Benchmarks show < 5% throughput difference between 512 and 4096 cell batches.

---

## P-01-13: Crypto offload (AES-NI, SHA-NI) for encrypted columns 🔴

**Layer**: Instruction sets
**Status**: 🔴 open
**Math**: none
**Effort**: M
**Impact**: medium

### Problem

If columns are encrypted at rest (a common requirement), the scan kernel
must decrypt before scanning. `AESENC` (4 cycles, 0.5 throughput on Intel)
is cheap, but the key schedule setup and CBC/CTR mode chaining add overhead.

### Open questions

- Should decryption happen at the page level (decrypt a 4 KB page into L1,
  then scan) or at the cell level (stream-decrypt during scan)?
- Can we use `PCLMULQDQ` for GHASH in AES-GCM for authenticated encryption?

### Success criteria

- An `EncryptedPage` type that transparently decrypts on access.
- Benchmark: encrypted scan at < 2× the cost of plaintext scan.

---

## P-01-14: RISC-V kernel port (future) 🔴

**Layer**: Instruction sets
**Status**: 🔴 open
**Math**: none
**Effort**: XL (6+ months)
**Impact**: low (today), medium (future)

### Problem

RISC-V (SiFive P550, Ventana Veyron V1, T-Head C910) is emerging for
datacenter use. The vector extension (RVV) is variable-length like ARM SVE.

No production RISC-V server hardware is widely available yet, but porting
early would give us a head start.

### Open questions

- Is RVV stable enough to target?
- Should we use the `riscv-rvv` crate or inline assembly?

### Success criteria

- A `scan_eq` kernel in RVV assembly.
- Smoke test runs on a RISC-V emulator (QEMU).

---

## Summary

| # | Problem | Status | Effort | Impact |
|---|---------|--------|--------|--------|
| 01 | Per-tier scan kernel differentiation | 🟡 | M | high |
| 02 | BMI2 PEXT/PDEP landmine on Zen/Zen2 | 🔴 | S | high |
| 03 | AVX-512 frequency throttling on older Intel | 🟡 | S | medium |
| 04 | ARM NEON / SVE kernel port | 🟡 | L | high |
| 05 | VPTERNLOGQ for multi-predicate fusion | 🔴 | M | high |
| 06 | VPOPCNTDQ for vectorized Hamming distance | 🟡 | S | medium |
| 07 | Cross-vendor kernel benchmarking harness | 🔴 | M | high |
| 08 | Instruction cache pressure for large kernel tables | 🔴 | M | medium |
| 09 | Branchless hot loops — eliminate mispredicts | 🔴 | M | high |
| 10 | REP MOVSB for bulk page copy | 🟢 | — | low |
| 11 | Split LOCK avoidance | 🔴 | S | critical |
| 12 | SIMD-amortized batch size tuning | 🟡 | S | low |
| 13 | Crypto offload for encrypted columns | 🔴 | M | medium |
| 14 | RISC-V kernel port (future) | 🔴 | XL | low |
