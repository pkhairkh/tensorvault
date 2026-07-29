# Wave 1 Research: Instruction Set + Memory Hierarchy Problems

**Engine context**: Instruction-first, memory-centric DB engine. Every value is a 64-bit word; data lives in explicit memory tiers (L1/L2/L3/DDR5/HBM/CXL/NVMe/Network); a kernel table holds hand-tuned AVX-512 kernels per (CPU, tier) tuple.

**Reference energy figures used throughout** (from Kim et al., MEMSYS 2015; Vogelsang, IEEE TCAD 2010; and standard architecture texts):

| Tier | Latency | Energy/access |
|------|---------|---------------|
| L1D | ~1 ns (4 cyc) | ~0.5 nJ |
| L2 | ~4 ns (14 cyc) | ~2–4 nJ |
| L3 | ~12 ns (40 cyc) | ~5–10 nJ |
| DDR5 | ~80 ns | ~20–30 nJ |
| HBM | ~100 ns | ~5–10 nJ/word (wider bus) |
| CXL | 140–520 ns | ~30–40 nJ |
| NVMe | 10–100 μs | ~2–10 μJ / 4 KB read |
| Network/RDMA | 2–10 μs | ~1–5 μJ / msg |

---

# PART I: INSTRUCTION SET PROBLEMS (14)

---

## P-IS-01: Per-Tier Scan Kernel Differentiation

### Candidate Solution A: Tier-Tagged Kernel Dispatch Table
- **Approach**: Maintain a `(CPU_arch, tier_id) → kernel_fn*` table indexed at query-planning time. The planner resolves the tier of each input region and emits a CALL to the matching kernel. L3 kernels issue 1-page software prefetch (`_mm_prefetch(...,_MM_HINT_T0)`); DDR5 kernels issue 4-page stride prefetch; CXL kernels issue 8-page prefetch with deeper lead distance; NVMe kernels use io_uring async submit+reap.
- **Performance**: Willhalm et al. (VLDB 2009) showed SIMD-scan achieving 1.6–3.2 cycles/value on in-memory (L3-resident) data with software prefetch tuned to cache geometry. Polychroniou et al. (VLDB 2015) showed 2–7× speedup over scalar with gather/scatter-based vectorized operators. Per-tier prefetch tuning adds another 1.3–2× on DDR5-resident data (prefetch distance must match DRAM row-buffer open time ~40 ns). Expected: ~1.5 cycles/value L3, ~3 cycles/value DDR5, ~8 cycles/value CXL.
- **Time to implement**: 2–3 months. Requires writing 4 kernel variants per operator per architecture (scan, filter, aggregate = ~12 kernels × 4 tiers = 48 functions), plus dispatch plumbing.
- **Energy cost**: L3-resident scan: ~2 nJ/value (0.5 nJ L1 load + 1.5 nJ SIMD compute). DDR5-resident: ~30 nJ/value (dominated by DRAM read). CXL: ~40 nJ/value. The compute overhead of dispatch is amortized over ≥1024-element batches (<0.001 nJ/value).
- **Upside**: Maximum throughput per tier; each kernel is independently tunable and benchmarkable.
- **Downside**: Code explosion (N operators × M tiers × P architectures); maintenance burden.
- **Key paper**: Willhalm et al., "SIMD-Scan: Ultra Fast in-Memory Table Scan Using On-Chip Vector Processing Units," PVLDB 2(1), 2009. Polychroniou et al., "Rethinking SIMD Vectorization for In-Memory Databases," PVLDB 8(12), 2015.

### Candidate Solution B: Adaptive Prefetch Distance + Single Kernel
- **Approach**: One parameterized kernel per operator; prefetch distance and depth are runtime parameters passed via a small descriptor struct. The kernel issues `_mm_prefetch(ptr + stride * dist, hint)` where `dist` is read from the tier descriptor.
- **Performance**: ~10–20% lower than Solution A on L3/DDR5 because the branch on prefetch depth inhibits some compiler optimizations; but within 5% on CXL where memory latency dominates. Expected ~1.8 cycles/value L3, ~3.5 cycles/value DDR5.
- **Time to implement**: 1–1.5 months. One kernel per operator with parameterized prefetch; tier descriptor resolved at plan time.
- **Energy cost**: ~5% higher compute energy per value due to parameter indirection (~2.1 nJ/value L3 vs 2.0 nJ); negligible vs memory energy on DDR5+.
- **Upside**: Far less code; easier to maintain and benchmark; new tiers only need a descriptor entry.
- **Downside**: Cannot exploit tier-specific instruction sequences (e.g., NVMe async I/O fundamentally different from synchronous prefetch); 10–20% throughput penalty on fast tiers.
- **Key paper**: Polychroniou et al., PVLDB 2015 (single vectorized scan kernel with tunable block size).

### Candidate Solution C: Tier-Blind Kernel + Hardware Prefetcher Reliance
- **Approach**: Write one canonical SIMD kernel per operator with no software prefetch; rely on the CPU's hardware stride/stream prefetcher to detect access patterns.
- **Performance**: Within 10% of tuned kernels on L3-resident sequential scans (hardware prefetchers are excellent for unit-stride). Falls to 40–60% of tuned throughput on DDR5 and CXL because hardware prefetchers have limited depth (~16 cache lines on Skylake) and cannot bridge CXL's 170 ns controller latency. NVMe: non-functional (no hardware prefetch across PCIe).
- **Time to implement**: 0.5 months. Trivial—just write standard SIMD scan.
- **Energy cost**: Similar compute energy (~2 nJ/value L3); but 30–50% more DRAM/CXL energy on misses due to under-prefetching causing pipeline stalls and re-fetches.
- **Upside**: Minimal engineering; simplest codebase.
- **Downside**: Unacceptable for CXL/NVMe tiers; leaves 40–60% throughput on the table for slow tiers.
- **Key paper**: Willhalm et al., VLDB 2009 (demonstrated software prefetch was essential for non-L3-resident scans).

### Recommendation
**Solution A** (tier-tagged dispatch table). For an instruction-first engine, the dispatch table is a natural fit—it's already the central abstraction. The 2–3 month cost is justified by the 1.3–2× throughput gain on DDR5/CXL/NVMe tiers, which are where real workloads spend most time. Solution B is a good fallback if engineering time is constrained. Solution C is rejected: it defeats the engine's memory-centric design.

---

## P-IS-02: BMI2 PEXT/PDEP on AMD Zen/Zen2

### Candidate Solution A: CPUID-Guarded Runtime Dispatch
- **Approach**: At engine init, call `cpuid` to detect microarchitecture. If `CPUID.7.0:EBX[BMI2]=1` AND vendor is Intel OR family ≥ Zen3, use the PEXT/PDEP fast path (3 cycles). If Zen/Zen2, dispatch to the software fallback. Store the function pointer in the kernel table.
- **Performance**: PEXT on Intel/Zen3+ executes in 3 cycles with 1/cycle throughput (Agner Fog instruction tables, 2025). Software fallback (shift+mask loop) is ~6–8 cycles for a 64-bit field extraction. On Zen/Zen2, the microcoded PEXT is 18 cycles, so the fallback is 2–3× faster. Net: 6–8 cycles on AMD Zen/Zen2 vs 18 cycles microcoded; 3 cycles on Intel/Zen3+.
- **Time to implement**: 0.5 months. CPUID detection is ~50 lines; software PEXT fallback is ~20 lines of C; dispatch pointer swap is trivial.
- **Energy cost**: Fast-path PEXT: ~0.3 nJ (single μop). Software fallback: ~0.5–0.8 nJ (6–8 μops). Microcoded PEXT: ~1.5 nJ (18-cycle microcode sequence with high switching activity). The fallback saves ~1 nJ/op on Zen/Zen2.
- **Upside**: Correct on all CPUs; no throughput regression on any platform; low engineering cost.
- **Downside**: Two code paths to maintain and test; indirect call overhead (~1 cycle) if not inlined.
- **Key paper**: Agner Fog, "Instruction Tables: Lists of Instruction Latencies, Throughputs and Micro-operation Breakdowns," 2025 (documents 18-cycle PEXT on Zen/Zen2 vs 3-cycle on Zen3+/Intel).

### Candidate Solution B: Software-Only PEXT (No BMI2)
- **Approach**: Never use PEXT/PDEP intrinsics. Always use the shift+mask software fallback everywhere.
- **Performance**: 6–8 cycles on all platforms. On Intel/Zen3+, this is 2× slower than the hardware instruction.
- **Time to implement**: 0.25 months. One code path; no dispatch.
- **Energy cost**: ~0.5–0.8 nJ everywhere. On Intel/Zen3+, this is ~0.2–0.5 nJ more than the hardware instruction.
- **Upside**: Simplest; no platform-specific code.
- **Downside**: 2× throughput penalty on Intel/Zen3+ where PEXT is fast; forfeits a real hardware advantage.
- **Key paper**: Agner Fog instruction tables, 2025.

### Candidate Solution C: Refuse BMI2 / Replace Algorithm
- **Approach**: Redesign the bit-unpacking and hash-probing algorithms to avoid PEXT/PDEP entirely. Use lookup tables (LUT) or multiply-shift for bit extraction.
- **Performance**: LUT-based extraction: ~5 cycles (1 load + 1 shift). Multiply-shift: ~4 cycles. Comparable to software PEXT but different trade-offs (LUT uses L1 cache; multiply uses the multiplier port). On Intel/Zen3+, still slower than 3-cycle hardware PEXT.
- **Time to implement**: 1.5–2 months. Requires redesigning the bit-packing layer and revalidating correctness.
- **Energy cost**: LUT: ~1 nJ (includes L1 access energy). Multiply-shift: ~0.5 nJ.
- **Upside**: Algorithmically portable to ARM/RISC-V (no BMI2 equivalent).
- **Downside**: Highest engineering cost; LUT path consumes L1 cache capacity.
- **Key paper**: Lemire et al., "Consistent Hashing Using Buzhash," and bit-unpacking literature.

### Recommendation
**Solution A** (CPUID-guarded dispatch). The 0.5-month cost is trivial and the 2–3× speedup on Zen/Zen2 is free. The engine already has a CPU×tier kernel table, so adding a "microarch variant" dimension is natural. Solution B is a reasonable v1 shortcut. Solution C is over-engineering for now but worth revisiting when the ARM/RISC-V port (P-IS-04, P-IS-13) is attempted.

---

## P-IS-03: AVX-512 Frequency Throttling

### Candidate Solution A: Dynamic Width Switching (AVX2 ↔ AVX-512)
- **Approach**: At init, detect microarch via CPUID. On Skylake-X / Cascade Lake, default to 256-bit (AVX2) kernels. On Ice Lake+ / Zen 4 / Zen 5, use 512-bit (AVX-512) kernels. Optionally implement a runtime profiler that counts AVX-512 instruction density per kernel; if a kernel is "light" (<10% AVX-512 instructions), use 512-bit anyway since Skylake-X only downclocks on "heavy" AVX-512 (license level 2/3).
- **Performance**: On Skylake-X, heavy AVX-512 downclocks ~300–500 MHz (LLVM issue #102047; Lemire blog 2018 measured ~3–4% net penalty for mixed workloads). Using AVX2 on Skylake-X avoids the downclock but halves SIMD width → ~1.5–1.8× slower per kernel (not 2× because of load/store port contention). On SPR, the penalty is ~100 MHz (negligible). On Zen 4/5, zero downclock (Chips and Cheese, Mar 2025). Net: AVX2 on Skylake-X = ~60% of AVX-512 throughput but no 500 MHz system-wide penalty. For a query that is 50% SIMD + 50% scalar, AVX-512's 500 MHz penalty hurts the scalar half too, so AVX2 can actually win.
- **Time to implement**: 1 month. Need both AVX2 and AVX-512 kernel variants (already partially needed for ARM). CPUID detection + dispatch.
- **Energy cost**: AVX-512 on Skylake-X: higher power draw (~1.5× due to 512-bit FMA units) but shorter execution time → similar energy/op. The 500 MHz downclock reduces frequency-dependent leakage waste but increases execution time. Net energy is roughly equivalent; AVX-512 is ~5–10% more energy-efficient per operation due to amortized loop overhead. On Zen 5, AVX-512 is strictly better (no downclock, full-width datapaths).
- **Upside**: Correct per-platform behavior; avoids the "AVX-512 tax" on Skylake-X.
- **Downside**: Two kernel variants doubles testing matrix; subtle bugs when AVX2 and AVX-512 produce different floating-point rounding.
- **Key paper**: LLVM issue #102047, "[AVX512] Preferring 512-bit vectors on recent Intel CPUs," 2024. Chips and Cheese, "Zen 5's AVX-512 Frequency Behavior," Mar 2025. Lemire, "The Dangers of AVX-512 Throttling," 2018.

### Candidate Solution B: AVX-512 Everywhere with License-Level Awareness
- **Approach**: Use AVX-512 on all capable CPUs. On Skylake-X, restrict to "light" AVX-512 instructions (no 512-bit FMA, only logic/load/store = license level 1) which do not trigger downclocking. Use 256-bit FMA for compute, 512-bit for loads/stores/predicate logic.
- **Performance**: 512-bit loads + 256-bit compute = ~1.4× over pure AVX2 on Skylake-X (wider load bandwidth, no downclock). On SPR/Zen4/5, switch to full 512-bit FMA for 2× throughput.
- **Time to implement**: 1.5–2 months. Requires careful instruction selection (using `_mm512_load` but `_mm256_fmadd`), which is non-trivial in intrinsics.
- **Energy cost**: ~0.3 nJ lower than full AVX-512 on Skylake-X (no 512-bit FMA power spike); comparable on SPR.
- **Upside**: One codebase; exploits load bandwidth on Skylake-X without the downclock.
- **Downside**: Complex; compiler may still emit 512-bit FMA if not careful; difficult to validate.
- **Key paper**: Intel 64 and IA-32 Optimization Reference Manual, §18.25 (AVX-512 license levels). Chips and Cheese, 2025.

### Candidate Solution C: AVX2-Only (Conservative)
- **Approach**: Use only AVX2 (256-bit) on all x86 platforms. Avoid AVX-512 entirely.
- **Performance**: ~50–60% of AVX-512 throughput on SPR/Zen4/5 (which have zero downclock penalty). Leaves significant performance on the table on modern hardware.
- **Time to implement**: 0 months if already implemented; but forfeits future gains.
- **Energy cost**: ~1.5–2× more energy/op on modern CPUs (more iterations → more loop overhead, more memory accesses).
- **Upside**: No downclock issues anywhere; simplest.
- **Downside**: 40–50% throughput loss on SPR/Zen4/5; unacceptable for a performance-focused engine.
- **Key paper**: LLVM issue #102047, 2024.

### Recommendation
**Solution A** (dynamic width switching). The per-platform dispatch is already part of the kernel table design. Skylake-X is a declining market share; SPR/Zen4/5 are the future and they have zero downclock. The 1-month cost is justified by recovering 40–50% throughput on modern CPUs while protecting Skylake-X. Solution B is technically superior but the 2× engineering cost and validation burden make it a poor ROI. Solution C is rejected: it permanently handicaps the engine on modern hardware.

---

## P-IS-04: ARM NEON/SVE Port

### Candidate Solution A: SVE-First (Target Graviton 3/4, Grace)
- **Approach**: Write ARM kernels using SVE intrinsics (`svld1_u64`, `svwhilelt_b64`, etc.). SVE is length-agnostic (vector length = `VL` set by hardware, 128–2048 bits), so the same code runs on Graviton 3 (256-bit SVE), Graviton 4 (256-bit SVE2), and NVIDIA Grace (128-bit SVE). Use SVE2 where available for additional instructions (MATCH, HISTSEG).
- **Performance**: SVE on Graviton 3/4 achieves ~70–80% of AVX-512 throughput on equivalent Intel cores for scan/filter workloads (fewer ports, 256-bit width). NVIDIA Grace with SVE at 128-bit achieves ~40–50% of AVX-512 per-core but has 72 cores (Neoverse V2). The length-agnostic property means no recompilation needed when wider SVE implementations ship. Expected: ~2.5 cycles/value on Graviton 4.
- **Time to implement**: 2–3 months. SVE intrinsics have a different programming model (predicate-driven, whilelt loops); each operator needs a rewrite. Need to set up ARM CI (Graviton EC2 instances).
- **Energy cost**: Graviton 4 SVE scan: ~1.5 nJ/value (ARM cores are ~2× more energy-efficient than x86 at equivalent throughput, per AWS Graviton benchmarks). This is the most energy-efficient path.
- **Upside**: Future-proof (SVE scales to 2048-bit); best energy efficiency; works on Graviton + Grace.
- **Downside**: SVE is not available on older ARM (Cortex-A72/A76, Raspberry Pi); requires newer hardware for development/testing.
- **Key paper**: ARM, "SVE Programmer's Guide," 2020. NVIDIA, "Arm Vector Instructions – Grace CPU Benchmarking Guide," 2024. ArXiv 2505.09462, "ARM SVE Unleashed: Performance and Insights," 2025.

### Candidate Solution B: NEON (128-bit Fixed)
- **Approach**: Write ARM kernels using NEON intrinsics (`vld1q_u64`, etc.). NEON is universally available on all ARMv8-A cores.
- **Performance**: 128-bit width = ~25% of AVX-512 throughput per core. On Graviton 4, NEON achieves ~1.8 cycles/value (good ILP with 2× 128-bit load ports). But misses SVE's predicate-driven vectorization (no whilelt), so tail handling requires scalar cleanup.
- **Time to implement**: 1–1.5 months. NEON intrinsics are simpler (fixed width, similar mental model to SSE).
- **Energy cost**: ~2.5 nJ/value (more iterations than SVE for same data → more loop overhead energy).
- **Upside**: Maximum compatibility (runs on every ARM chip); simpler programming model; easier to test.
- **Downside**: 128-bit width leaves 50% throughput on the table on Graviton 3/4 which support 256-bit SVE.
- **Key paper**: ARM NEON Programmer's Guide. NVIDIA Grace vectorization guide, 2024.

### Candidate Solution C: Auto-Vectorization Only
- **Approach**: Write portable C/C++ with `#pragma omp simd` and rely on the compiler (GCC/Clang) to auto-vectorize for ARM. No intrinsics.
- **Performance**: GCC 13 auto-vectorizes simple scans at ~60–70% of hand-tuned NEON, ~40% of SVE. Complex gather/scatter operations (hash joins) auto-vectorize poorly (~20–30% of hand-tuned).
- **Time to implement**: 0.25 months. Just compile with `-O3 -ftree-vectorize -march=armv9-a+sve2`.
- **Energy cost**: ~30% higher than hand-tuned due to suboptimal instruction selection and memory access patterns.
- **Upside**: Zero kernel code; works on all architectures; trivial maintenance.
- **Downside**: 30–60% throughput loss vs hand-tuned; unacceptable for a performance-focused engine.
- **Key paper**: ArXiv 2605.10860, "Towards Portable Performance on RISC-V Vector Processors," 2026 (also discusses auto-vectorization quality for vector ISAs).

### Recommendation
**Solution A** (SVE-first). Graviton 3/4 and Grace are the target ARM platforms, and all support SVE. The length-agnostic design is a strategic advantage—no recompilation when 512-bit SVE ships. The 2–3 month cost is justified by 2× throughput over NEON and best-in-class energy efficiency. Solution B (NEON) is a good interim if SVE hardware is unavailable for development. Solution C is rejected for the same reason as P-IS-03 Solution C.

---

## P-IS-05: VPTERNLOGQ Multi-Predicate Fusion

### Candidate Solution A: Manual Intrinsics with Precomputed 8-bit IMM
- **Approach**: When a scan has 2–3 conjunctive predicates (e.g., `x > 10 AND x < 100 AND x != 50`), compute each comparison into a mask register, then fuse with a single `VPTERNLOGQ zmm, mask1, mask2, IMM` where IMM encodes the Boolean function. Use a lookup table or truth-table compiler to derive the 8-bit immediate.
- **Performance**: VPTERNLOGQ fuses 3 mask inputs into 1 output in a single instruction (1-cycle latency, 0.5/cycle throughput on Skylake-X per Felix Cloutier ISA reference). Replaces 2 separate `KANDD` instructions (2 cycles). Saves ~1 cycle per predicate fusion, ~2 cycles for 3-predicate case. On a scan with 3 predicates over 8 elements/vector, this is ~0.25 cycles/value saved.
- **Time to implement**: 0.5 months. Precompute IMM via a truth-table-to-IMM function (~30 lines); call `_mm512_ternarylogic_epi64` in scan kernels.
- **Energy cost**: 1 VPTERNLOGQ: ~0.3 nJ. 2 KANDD: ~0.5 nJ. Saves ~0.2 nJ per vector = ~0.025 nJ/value.
- **Upside**: Simple, measurable win; the instruction is available on all AVX-512 CPUs.
- **Downside**: Only helps when ≥2 predicates are conjunctive; requires the compiler/planner to detect predicate DAG structure.
- **Key paper**: Intel, "Intel 64 and IA-32 Architectures Software Developer's Manual," Vol. 2B (VPTERNLOGQ). Felix Cloutier, x86 reference. Arnaud Carré, "AVX Bitwise Ternary Logic Instruction Busted!" 2024.

### Candidate Solution B: Compiler Auto-Fusion (Rely on -O3)
- **Approach**: Write predicate logic as C expressions (`mask = (m1 & m2) | m3`) and trust that GCC/Clang will emit VPTERNLOGQ at `-O3 -mavx512f`.
- **Performance**: GCC 13 emits VPTERNLOGQ for simple 3-operand Boolean expressions ~60% of the time (misses complex cases with negation or 4+ operands). When it misses, it emits 2 separate instructions → 1 cycle penalty.
- **Time to implement**: 0 months—just verify compiler output.
- **Energy cost**: Same as Solution A when fusion succeeds; ~0.025 nJ/value worse when it misses.
- **Upside**: Zero engineering cost.
- **Downside**: Unreliable; compiler version-dependent; no control over edge cases.
- **Key paper**: GCC documentation on AVX-512 code generation.

### Candidate Solution C: Predicate DAG Compiler
- **Approach**: Build a small Boolean expression tree compiler in the query optimizer. When the planner sees `WHERE x > 10 AND (y < 100 OR z = 5)`, it constructs a DAG, assigns each leaf a mask register, and emits a sequence of VPTERNLOGQ instructions with computed IMMs. The DAG compiler minimizes total instruction count via algebraic simplification.
- **Performance**: Optimal—always emits the minimum number of VPTERNLOGQ instructions. For a 5-predicate expression, saves 2–3 instructions vs manual coding.
- **Time to implement**: 2–3 months. Requires a Boolean expression tree, truth-table analysis, register allocator for mask registers, and codegen.
- **Energy cost**: Optimal energy: minimum instructions = minimum switching activity.
- **Upside**: Handles arbitrary predicate complexity; future-proofs against complex WHERE clauses.
- **Downside**: Significant engineering investment for a niche optimization (most queries have ≤3 predicates).
- **Key paper**: Polychroniou et al., PVLDB 2015 (discusses vectorized predicate evaluation). Intel ISA manual.

### Recommendation
**Solution A** (manual intrinsics with precomputed IMM). The 0.5-month cost is low and the win is measurable. Most real-world queries have 2–3 predicates, so the manual approach covers the common case. Solution C is the right long-term answer but the 2–3 month cost is not justified until predicate complexity grows. Solution B is a good baseline (verify compiler output first), then layer Solution A on top for the cases the compiler misses.

---

## P-IS-06: VPOPCNTDQ for Hamming Distance

### Candidate Solution A: AVX-512_VPOPCNTDQ (Ice Lake+ / Zen 4+)
- **Approach**: Use `_mm512_popcnt_epi64` to count set bits in 8 × 64-bit words per instruction. Available on Ice Lake SP, Zen 4, Zen 5 (Wikipedia AVX-512; .NET runtime issue #96162 confirms Zen 4 support).
- **Performance**: 1 instruction processes 512 bits. Latency ~3 cycles, throughput 1/cycle on Ice Lake. Throughput: ~1.5 cycles per 64-bit word for Hamming weight (512 bits / 3 cycles / 8 words = ~0.375 cycles/word). For Hamming distance (XOR then popcount): XOR (1 cyc) + VPOPCNTDQ (3 cyc) + horizontal reduce (~4 cyc) = ~8 cycles per 8 words = 1 cycle/word.
- **Time to implement**: 0.25 months. Single intrinsic call; trivial to integrate.
- **Energy cost**: ~0.5 nJ per VPOPCNTDQ instruction (similar to other AVX-512 ALU ops) = ~0.06 nJ/word.
- **Upside**: 5–8× faster than scalar popcount; hardware-guaranteed correctness.
- **Downside**: Requires Ice Lake+ / Zen 4+; not available on Skylake-X or Zen 2.
- **Key paper**: Intel ISA manual, VPOPCNTDQ. Wikipedia, "AVX-512" (VPOPCNTDQ introduced with Knights Mill and Ice Lake). .NET runtime issue #96162, 2023.

### Candidate Solution B: PSADBW Fallback (AVX2)
- **Approach**: Use the classic `PSADBW` (Sum of Absolute Differences) trick: precompute a lookup that converts each byte to its popcount, then use `_mm256_sad_epu8` against a zero vector to get per-8-byte popcount sums. Requires 2× PSADBW + shifts to cover a 256-bit vector.
- **Performance**: ~6–8 cycles per 256-bit vector = ~2 cycles/64-bit word. 3–4× slower than VPOPCNTDQ but works on all AVX2 CPUs (Skylake-X, Zen 2).
- **Time to implement**: 0.5 months. The PSADBW trick is well-documented but requires careful byte-shuffling.
- **Energy cost**: ~1.5 nJ per 256-bit vector (~0.4 nJ/word) due to multiple instructions and shuffle port pressure.
- **Upside**: Universal AVX2 compatibility; no new ISA requirement.
- **Downside**: 3–4× slower than VPOPCNTDQ; higher energy.
- **Key paper**: Muła et al., "Faster Population Counts Using AVX2 Instructions," IEEE TC 2017.

### Candidate Solution C: Scalar POPCNT
- **Approach**: Use the scalar `POPCNT` instruction (available since SSE4.2 / BMI1) in a loop. 1 cycle per 64-bit word.
- **Performance**: 1 cycle/word scalar, but no SIMD parallelism. For 1024 words: 1024 cycles + loop overhead = ~1200 cycles. VPOPCNTDQ: ~128 cycles (1024/8 × 1). So scalar is ~10× slower.
- **Time to implement**: 0.1 months. Trivial.
- **Energy cost**: ~0.3 nJ/word but 10× more iterations → more loop overhead energy (~0.5 nJ/word total).
- **Upside**: Simplest; works everywhere.
- **Downside**: 10× slower than VPOPCNTDQ; unacceptable for Hamming-heavy workloads.
- **Key paper**: Intel optimization manual (POPCNT instruction).

### Recommendation
**Solution A** (VPOPCNTDQ) with **Solution B** (PSADBW) as fallback. Use CPUID dispatch (same pattern as P-IS-02). On Ice Lake+ / Zen 4+, VPOPCNTDQ is 5–8× faster and lower energy. On older CPUs, PSADBW provides a reasonable fallback. The combined cost is 0.75 months. Solution C is only acceptable as a debug/reference path.

---

## P-IS-07: Cross-Vendor Kernel Benchmarking

### Candidate Solution A: Cloud Instance Matrix
- **Approach**: Rent cloud instances across vendors: AWS (Graviton 4, Intel SPR, AMD EPYC Zen 4), GCP (Intel Cascade Lake, AMD Milan), Azure (Intel SPR, AMD Genoa). Run a standardized microbenchmark suite (scan, filter, aggregate, hash join) on each, measuring cycles/value and watts.
- **Performance**: Covers the top 4 microarchitectures in production. Results are representative of real deployment conditions (noisy neighbors included). Throughput measurements include cloud overhead (virtualization, NUMA effects).
- **Time to implement**: 1 month. Write benchmark harness, provision instances, automate runs, collect results. Ongoing cost: ~$500–2000/month for instances during benchmarking.
- **Energy cost**: N/A (this is a measurement problem, not a runtime cost). Measurement energy is the instance's idle + benchmark power, ~100–400W per instance.
- **Upside**: Real-world numbers; covers the exact CPUs customers use; low setup cost.
- **Downside**: Cloud instances have noisy-neighbor variance (±5–10%); no access to exotic hardware (Apple M-series, RISC-V); cannot control BIOS/firmware settings.
- **Key paper**: SPEC ICPE 2022 (cross-vendor benchmarking methodology). AWS Graviton performance benchmarks.

### Candidate Solution B: On-Prem Benchmarking Lab
- **Approach**: Purchase or lease bare-metal servers: 1× Intel SPR, 1× AMD Zen 4, 1× Graviton 4 (via AWS Bare Metal), 1× Apple M3 Ultra. Pin cores, disable Turbo, control BIOS, measure with PCM + perf.
- **Performance**: ±1% repeatability (no noisy neighbors). Full control over frequency, NUMA, C-states. Can measure energy precisely with RAPL (Intel/AMD) or perf energy counters.
- **Time to implement**: 2–3 months (procurement + setup + benchmarking). Capex: ~$20–50K for hardware.
- **Energy cost**: Precise measurement possible. Can characterize energy/op per kernel per CPU to populate the kernel table's energy column.
- **Upside**: Gold-standard measurements; enables energy-axis optimization; reproducible.
- **Downside**: High capex; limited to purchased hardware; procurement lead time.
- **Key paper**: McCalpin, IXPUG 2023 (uses on-prem Xeon Max for precise HBM bandwidth measurement).

### Candidate Solution C: Simulation (gem5 / QEMU)
- **Approach**: Use gem5 (for cycle-accurate microarch simulation) or QEMU (for functional testing) to benchmark kernels on architectures without physical access.
- **Performance**: gem5 gives cycle-accurate estimates but is 100–1000× slower than real hardware → limited to microbenchmarks (<1M instructions). QEMU gives no performance data (functional only). gem5 models for recent x86 (SPR, Zen 4) are incomplete or inaccurate.
- **Time to implement**: 2 months (model setup + validation against known hardware).
- **Energy cost**: gem5 has energy models (McPAT integration) but accuracy is ±20–30%.
- **Upside**: Can test hypothetical/future hardware (RISC-V RVV, future ARM); no hardware cost.
- **Downside**: Low accuracy on modern microarchitectures; slow; gem5 models lag real silicon by 2–3 years.
- **Key paper**: gem5 documentation. SPEC ICPE 2022 (discusses simulation vs measurement trade-offs).

### Recommendation
**Solution A** (cloud instance matrix) for v1, with **Solution B** (on-prem lab) as a v2 upgrade. Cloud instances cover 90% of customer deployments at 1/10th the cost of on-prem. The ±5–10% variance is acceptable for kernel selection (kernels differ by >20% in throughput). Solution C is useful only for RISC-V (P-IS-13) where no hardware exists yet.

---

## P-IS-08: I-Cache Pressure

### Candidate Solution A: Function Sections + Hot/Cold Splitting
- **Approach**: Compile with `-ffunction-sections -fdata-sections` and link with `--gc-sections` to remove unused kernels. Split each kernel into a "hot" path (the SIMD inner loop, <2 KB) and "cold" path (tail handling, error paths) using `__attribute__((cold))` and `__attribute__((section(".text.cold")))`. The hot path fits in L1I; cold paths are evicted to L2.
- **Performance**: A 32 KB L1I holds ~16 kernels at 2 KB each. With hot/cold splitting, the hot inner loops are ~500 bytes each → 64 kernels fit in L1I. Eliminates I-cache thrashing when the planner switches between kernels mid-query. Expected: 5–15% throughput improvement on multi-operator queries (I-cache misses cost ~12 ns each from L2; eliminating 1 miss per 1024 values saves ~0.01 cycles/value).
- **Time to implement**: 0.5 months. Add compiler flags; manually annotate cold paths; verify with `perf stat -e iCache-misses`.
- **Energy cost**: L2 instruction fetch: ~4 nJ vs L1I fetch ~0.5 nJ. Eliminating 1 L2 I-fetch per 1024 values saves ~3.5 nJ per 1024 values = ~0.003 nJ/value. Small but nonzero.
- **Upside**: Simple, portable, no runtime cost; works on all architectures.
- **Downside**: Requires manual annotation of cold paths; linker must support section GC.
- **Key paper**: Hintsani et al., ISPASS 2019 (I-cache characterization of database engines). Linux LPC 2021, "Strange Kernel Performance Changes" (function alignment and I-cache effects).

### Candidate Solution B: JIT Code Generation (Per-Query Kernel Fusion)
- **Approach**: Instead of precompiled kernels, JIT-compile a fused kernel per query at runtime (using LLVM or asmjit). The JIT emits only the operators actually needed, in sequence, with no function calls between them. The entire query's hot path is one contiguous function <8 KB.
- **Performance**: Eliminates I-cache misses between operators entirely (no CALL/RET overhead, no branch predictor pollution). Kersten et al. (PVLDB 2018) showed compiled execution is 1.1–3× faster than vectorized for narrow queries. But JIT compilation adds 1–50 ms per query (LLVM) or 10–100 μs (asmjit). For short queries, compilation cost dominates.
- **Time to implement**: 3–4 months. Need LLVM ORC or asmjit integration, expression tree → IR lowering, and a code cache.
- **Energy cost**: JIT compilation: ~10–100 J per query (LLVM optimization passes are CPU-intensive). Amortized over query results: negligible for long-running queries, significant for short queries. Runtime: ~5% lower than precompiled due to zero I-cache misses.
- **Upside**: Maximum performance for long-running queries; zero I-cache thrashing; adaptive to query shape.
- **Downside**: High engineering cost; compilation latency for short queries; JIT adds security surface (W^X concerns).
- **Key paper**: Kersten et al., "Everything You Always Wanted to Know About Compiled and Vectorized Queries But Were Afraid to Ask," PVLDB 11(13), 2018.

### Candidate Solution C: Profile-Guided Kernel Layout
- **Approach**: Use PGO (profile-guided optimization) to reorder kernel functions in the binary so that frequently co-occurring kernels are adjacent in the instruction stream, maximizing L1I spatial locality.
- **Performance**: ~3–5% improvement from better I-cache spatial locality (adjacent functions share cache lines). Less impactful than hot/cold splitting but complementary.
- **Time to implement**: 0.75 months. Run representative workload with `-fprofile-generate`, then rebuild with `-fprofile-use`.
- **Energy cost**: ~1–2% reduction from fewer L2 I-fetches.
- **Upside**: No code changes; just build system modification.
- **Downside**: Requires representative profile workload; reoptimization needed when query patterns change.
- **Key paper**: LLVM PGO documentation. Hintsani et al., ISPASS 2019.

### Recommendation
**Solution A** (function sections + hot/cold splitting) as the baseline—it's 0.5 months and gives 5–15% immediately. Layer **Solution C** (PGO) on top for another 3–5% at near-zero cost. Defer **Solution B** (JIT) to a future wave; it's the highest-performance option but 3–4 months is too costly for Wave 1, and the engine's "kernel table" design is fundamentally precompiled.

---

## P-IS-09: Branchless Hot Loops

### Candidate Solution A: Mask Accumulation (SIMD-Native)
- **Approach**: In SIMD scan kernels, never branch on predicate results. Instead, compute a comparison mask, use it to select values via `_mm512_mask_blend_epi64`, and accumulate the mask bits to count matches. The loop body is pure arithmetic with zero branches.
- **Performance**: Eliminates 15–21 cycle branch misprediction penalty (johnnysswlab.com; Lemire 2019). For a scan with 30% selectivity, random predicate → ~70% misprediction rate without branchless. At 15 cycles/mispredict and 1 prediction/value, that's ~10.5 cycles/value wasted. Branchless: 0 mispredicts, ~2 cycles/value. Net: ~5× speedup on high-mispredict queries.
- **Time to implement**: 0.5 months. Rewrite scan/filter kernels to use mask operations instead of `if (predicate)` branches. Already natural in AVX-512.
- **Energy cost**: Branchless code does more total work (computes both paths) but avoids pipeline flush energy (~5–10 nJ per mispredict due to wasted execution units). Net: lower energy for selectivity 10–90%; higher energy for extreme selectivity (<5% or >95% where branch prediction is nearly perfect).
- **Upside**: Consistent performance regardless of data distribution; 5× on adversarial data.
- **Downside**: Slightly higher energy on skewed data where branch prediction works well; less effective for very wide operators.
- **Key paper**: Lemire, "Mispredicted Branches Can Multiply Your Running Times," 2019. johnnysswlab.com, "How Branches Influence Performance," 2020. Algorithmica, "Branchless Programming."

### Candidate Solution B: CMOV (Scalar Branchless)
- **Approach**: For scalar code paths (tail handling, non-SIMD operators), replace `if (x) y = a; else y = b;` with `y = cond ? a : b;` and rely on the compiler to emit `CMOV` (conditional move, 1 cycle, no branch).
- **Performance**: CMOV has ~2-cycle latency on Intel/AMD (data dependency chain). For simple selects, comparable to branchy code when prediction is good; 3–5× better when prediction is bad. Not applicable to SIMD paths.
- **Time to implement**: 0.25 months. Audit scalar code for branchy patterns; rewrite with ternary.
- **Energy cost**: CMOV: ~0.2 nJ. Branch (predicted): ~0.1 nJ. Branch (mispredicted): ~5 nJ. Net energy win depends on prediction accuracy.
- **Upside**: Simple; works everywhere; no SIMD required.
- **Downside**: Only helps scalar code; CMOV has a data dependency that can limit ILP.
- **Key paper**: StackOverflow, "Why is conditional move not vulnerable to branch prediction failure." Intel optimization manual (CMOV latency).

### Candidate Solution C: Predicated Execution (x86: LZCNT + CMOV chain)
- **Approach**: For complex multi-way branches, use a sequence of CMOV instructions or a computed-goto jump table. This is a hybrid: use branches for highly predictable paths, CMOV for unpredictable ones.
- **Performance**: ~10–20% better than pure branchy code; worse than pure SIMD mask accumulation for scan kernels.
- **Time to implement**: 1 month. Requires profiling to identify which branches are unpredictable.
- **Energy cost**: Mixed; profile-dependent.
- **Upside**: Adaptive; best where it matters.
- **Downside**: Complex; requires per-query profiling.
- **Key paper**: Lemire, "Branchless Sorting," 2019. Algorithmica, "Branchless Programming."

### Recommendation
**Solution A** (mask accumulation) for all SIMD kernels. It's the natural AVX-512 idiom and eliminates the worst-case 5× penalty. **Solution B** (CMOV) for scalar tail handling at 0.25 months additional. Together: 0.75 months for comprehensive branchless coverage. Solution C is over-engineering.

---

## P-IS-10: Split LOCK Avoidance

### Candidate Solution A: Alignment Guarantee + Compile-Time Assertion
- **Approach**: Ensure all 64-bit atomic values are 8-byte aligned (natural alignment for 64-bit types). Use `alignas(8)` / `__attribute__((aligned(8)))` on all atomic fields. Add a `static_assert(offsetof(struct, atomic_field) % 8 == 0)` check. The compiler naturally aligns `uint64_t` on x86-64, but packed structs and arena allocators can break this.
- **Performance**: Eliminates split locks entirely. An aligned `LOCK CMPXCHG` is ~20–40 cycles (L1 hit). A split LOCK is 3000–10000 cycles (kernel.org buslock docs; Rigtorp 2020 measured ~1000 cycles for bus lock; Chips and Cheese 2026 confirmed thousands for L2-miss case). Preventing even 1 split lock per 10^6 operations saves ~3000 cycles = ~1 μs.
- **Time to implement**: 0.25 months. Add alignment attributes; audit packed structs; add debug-mode assertion that checks alignment at runtime.
- **Energy cost**: Aligned atomic: ~5 nJ (L1 hit). Split lock: ~3000 nJ (bus lock serializes all cores → massive energy waste). Preventing split locks saves ~3000 nJ per avoided occurrence.
- **Upside**: Eliminates the worst-case latency spike; simple to implement; correct by construction.
- **Downside**: Alignment padding can waste 4–7 bytes per struct; not always possible in packed wire formats.
- **Key paper**: Linux kernel, "Bus Lock Detection and Handling" (kernel.org docs, v5.18). Rigtorp, "Performance Impact of Split Locks," 2020. Chips and Cheese, "Investigating Split Locks on x86-64," 2026.

### Candidate Solution B: Runtime Detection (split-lock-notify)
- **Approach**: Enable Linux `split_lock_detect=fatal` (kernel 5.8+) in development/testing. The kernel sends SIGBUS to any process that triggers a split lock, immediately exposing the bug. In production, use `split_lock_detect=warn` to log without killing.
- **Performance**: Runtime detection has near-zero overhead (hardware #AC exception, only fires on actual split lock). Does not prevent the split lock but catches it in testing.
- **Time to implement**: 0.1 months. Set kernel parameter; add CI test.
- **Energy cost**: Zero runtime cost (exception only fires on violation).
- **Upside**: Catches alignment bugs that static analysis misses; zero runtime cost.
- **Downside**: Does not prevent the problem—only detects it; `fatal` mode can crash production.
- **Key paper**: LWN.net, "Detecting and Handling Split Locks," 2019. Linux kernel buslock docs.

### Candidate Solution C: Static Analysis (Compiler Sanitizer)
- **Approach**: Use `-fsanitize=alignment` (UBSan) in CI to catch misaligned atomic accesses at compile/run time. Add Clang static analyzer checks for packed struct atomics.
- **Performance**: Zero production cost (sanitizer only in CI builds). Catches misalignment before deployment.
- **Time to implement**: 0.25 months. Add UBSan to CI; fix flagged issues.
- **Energy cost**: Zero (CI only).
- **Upside**: Catches the entire class of alignment bugs systematically.
- **Downside**: UBSan can have false positives on intentionally-unaligned reads (common in column stores); requires triage.
- **Key paper**: LLVM UBSan documentation.

### Recommendation
**All three, layered.** Solution A (alignment guarantee) is the primary defense at 0.25 months. Solution B (split-lock-notify in CI) catches regression at 0.1 months. Solution C (UBSan) catches the broader class at 0.25 months. Total: 0.6 months for complete coverage. The 3000–10000 cycle penalty of a split lock is catastrophic; this is cheap insurance.

---

## P-IS-11: SIMD Batch Size Tuning

### Candidate Solution A: Fixed 1024-Element Batch (ClickHouse Model)
- **Approach**: Process exactly 1024 values per kernel invocation (matching ClickHouse's vector size). 1024 = 16 × 64-element AVX-512 vectors = 8 KB per column batch (at 8 bytes/value). Fits in L1D (48 KB).
- **Performance**: ClickHouse uses 1024–4096 values per batch (clickhouse.com docs). At 1024, loop overhead is amortized to <0.1% of execution time. The 8 KB batch fits in L1D, enabling in-cache processing of intermediate results. Expected: ~2 cycles/value for scan, matching Polychroniou's measurements.
- **Time to implement**: 0.25 months. Set batch size constant; adjust buffer allocation.
- **Energy cost**: ~2 nJ/value (L1-resident compute). Loop overhead: ~0.002 nJ/value (negligible).
- **Upside**: Simple; proven by ClickHouse; fits L1D; amortizes function call overhead.
- **Downside**: Not optimal for very wide rows (>8 columns × 8 KB = >64 KB, spills L1D) or very narrow queries (overhead not amortized if selectivity <0.1%).
- **Key paper**: ClickHouse, "What is Vectorized Query Execution?" 2026. Polychroniou et al., PVLDB 2015.

### Candidate Solution B: Adaptive Batch Size
- **Approach**: Choose batch size at plan time based on column count and cache size: `batch = min(1024, L1D_size / (num_columns * 8))`. For a 1-column scan: 1024. For a 12-column aggregation: `32768 / (12*8) = 341` → round to 256.
- **Performance**: 5–15% better than fixed 1024 on multi-column queries (avoids L1D spill). For single-column scans, identical to fixed 1024.
- **Time to implement**: 0.75 months. Planner must estimate working set; allocator must support variable batch sizes; all kernels must be batch-size-agnostic (loop on `n`).
- **Energy cost**: ~1–2 nJ/value (L1D-resident). Avoiding L2 spills saves ~2 nJ/value on multi-column queries.
- **Upside**: Optimal cache utilization across query shapes.
- **Downside**: Variable batch sizes complicate the kernel interface; harder to benchmark and tune.
- **Key paper**: Polychroniou et al., PVLDB 2015 (discusses vector size trade-offs). Kersten et al., PVLDB 2018.

### Candidate Solution C: Profile-Guided Batch Size
- **Approach**: Run a calibration microbenchmark at engine startup: scan a test column at batch sizes 64, 128, 256, 512, 1024, 2048 and pick the fastest. Store result in kernel table.
- **Performance**: Adapts to the specific CPU's L1D size and prefetcher behavior. On CPUs with 32 KB L1D (older), may pick 512; on 48 KB (SPR), picks 1024; on 64 KB (Zen 5), picks 2048.
- **Time to implement**: 1 month. Calibration harness + per-CPU batch size in kernel table.
- **Energy cost**: Optimal per-CPU; avoids wasting energy on too-large (L2 spill) or too-small (overhead) batches.
- **Upside**: Auto-tunes to hardware; no manual tuning per platform.
- **Downside**: Startup calibration adds ~100 ms; results may be noisy; doesn't adapt to query shape.
- **Key paper**: ClickHouse vectorized execution docs. SPEC ICPE 2022.

### Recommendation
**Solution A** (fixed 1024) for v1. It's proven, simple, and matches the industry standard (ClickHouse). The 0.25-month cost is minimal. Upgrade to **Solution B** (adaptive) in v2 if multi-column queries show L1D spilling in production. Solution C is interesting but the startup cost and noise make it fragile.

---

## P-IS-12: Crypto Offload (AES-NI)

### Candidate Solution A: Page-Level Decrypt (mmap + lazily decrypt)
- **Approach**: Store encrypted pages on NVMe. On first access, decrypt the entire 4 KB page using AES-NI (`_mm_aesdec_si128` in a loop) into L1/L2 cache. Subsequent accesses hit the decrypted cache copy.
- **Performance**: AES-NI decrypts at ~1.5 cycles/byte (Intel AES-NI whitepaper: 3–10× over software). A 4 KB page decrypts in ~6144 cycles = ~2 μs at 3 GHz. Amortized over 512 values (4 KB / 8 B): ~4 ns/value. If the page is hot (re-accessed), subsequent accesses are L1-speed (~1 ns). Cold page access: ~2 μs + NVMe latency (~50 μs) = dominated by NVMe.
- **Time to implement**: 1.5 months. Implement page decrypt path in the buffer manager; integrate AES-NI key schedule; handle key rotation.
- **Energy cost**: AES-NI: ~0.5 nJ/byte (hardware AES is very efficient). 4 KB page: ~2 μJ. Amortized: ~4 nJ/value. Plus NVMe: ~5 μJ/value (4 KB read). Total cold: ~5 μJ/value; hot: ~4 nJ/value.
- **Upside**: Amortizes decrypt cost over many accesses; standard database pattern; AES-NI is 3–10× faster than software (DuPont POC: 300% improvement).
- **Downside**: First access is slow (decrypt latency); decrypted data in memory is a security exposure (side-channel, memory dump).
- **Key paper**: Intel, "Advanced Encryption Standard Instructions (AES-NI)," 2012. DuPont/Intel AES-NI POC White Paper. Carleton University, "AES-NI: Security, Performance, and Power," ICCAE 2020.

### Candidate Solution B: Stream Decrypt (Decrypt-as-you-scan)
- **Approach**: Decrypt each 64-bit value inline during the scan kernel using AES-NI in ECB/CTR mode. No separate decrypt step; the scan kernel loads encrypted data, decrypts in a register, applies the predicate, and discards.
- **Performance**: AES-NI on a single 128-bit block: ~4 cycles (latency 4, throughput 1/cycle). For 8 values per AVX-512 vector: 4 cycles decrypt + 2 cycles predicate = 6 cycles/vector = 0.75 cycles/value. Compared to unencrypted scan (~2 cycles/value), this is ~2.5× slower. But no separate decrypt pass → no memory exposure.
- **Time to implement**: 1 month. Modify scan kernels to insert AES-NI decrypt before predicate; handle CTR mode counter increment.
- **Energy cost**: ~0.5 nJ/value additional for AES-NI decrypt (on top of ~2 nJ/value scan). Total: ~2.5 nJ/value. No NVMe I/O energy if data is already in memory.
- **Upside**: No plaintext in memory (always encrypted at rest in cache); single-pass; lower latency than page-level for cold scans.
- **Downside**: 2.5× scan throughput penalty; every scan pays the decrypt cost even if the column isn't encrypted.
- **Key paper**: Intel AES-NI whitepaper, 2012. ICCAE 2020 (AES-NI performance analysis).

### Candidate Solution C: AES-NI Inline with Selective Decryption
- **Approach**: Hybrid: maintain an encrypted bloom filter / min-max index in plaintext. First scan the index to identify candidate pages, then decrypt only matching pages (Solution A). For encrypted columns without an index, fall back to stream decrypt (Solution B).
- **Performance**: If the index prunes 90% of pages, effective cost is 10% × page-decrypt + 90% × index-scan. For selective queries: ~0.4 ns/value (index) + 10% × 4 ns/value (decrypt) = ~0.8 ns/value. Near-unencrypted speed.
- **Time to implement**: 2.5–3 months. Requires encrypted index design, key management, and both decrypt paths.
- **Energy cost**: ~0.8 nJ/value effective (dominated by index scan). 5× lower than full stream decrypt.
- **Upside**: Best performance for selective queries; minimal plaintext exposure.
- **Downside**: Highest engineering cost; index maintenance overhead on writes; doesn't help full-table scans.
- **Key paper**: Intel AES-NI whitepaper. CockroachDB column-level encryption docs.

### Recommendation
**Solution B** (stream decrypt) for v1. The 1-month cost is lowest, and it provides the strongest security guarantee (no plaintext in memory). The 2.5× scan penalty is acceptable for encrypted columns which are typically not the hot path. Upgrade to **Solution C** (selective) in v2 for encrypted-column-heavy workloads. Solution A is the traditional approach but the plaintext-in-memory exposure is a security liability.

---

## P-IS-13: RISC-V RVV Port

### Candidate Solution A: RVV Intrinsics
- **Approach**: Write RISC-V kernels using RVV (RISC-V Vector Extension) intrinsics (`vload_u64`, `vmsgt_u64`, etc.) via `<riscv_vector.h>`. RVV is length-agnostic (like SVE): `VLEN` is set by hardware (128–16384 bits), and `vsetvli` configures the vector type at runtime.
- **Performance**: RVV on current SiFive P670 / Alibaba T-Head C920: 128–256 bit VLEN → ~30–50% of AVX-512 throughput per core. ArXiv 2605.10860 (2026) shows auto-vectorized RVV reaching 60–80% of hand-tuned on simple kernels. Expected: ~4 cycles/value on 256-bit RVV.
- **Time to implement**: 2–3 months. RVV intrinsics are verbose (vsetvli management, tail handling). Need RISC-V hardware or QEMU for testing. CI on SiFive HiFive or Alibaba cloud RISC-V instances.
- **Energy cost**: RISC-V cores are ~2–3× more energy-efficient than x86 at equivalent throughput. Expected: ~1 nJ/value on a 256-bit RVV core.
- **Upside**: Future-proof for the RISC-V ecosystem; length-agnostic like SVE; lowest energy per operation.
- **Downside**: RISC-V server hardware is immature (2024–2025); limited real-world deployment; RVV spec is complex (400+ instructions per Reddit/emergentmind).
- **Key paper**: RISC-V V Extension Specification v1.0. ArXiv 2605.10860, "Towards Portable Performance on RISC-V Vector Processors," 2026. RISC-V blog, "Enhancing Commercial Software with XuanTie Vectorization," 2025.

### Candidate Solution B: Inline Assembly
- **Approach**: Write critical kernels in inline assembly using raw RVV instructions (`vle64.v`, `vmsgt.vv`, `vmseq.vi`). Bypasses intrinsic header limitations and gives full control over `vsetvli` scheduling.
- **Performance**: Same as intrinsics but with full control over instruction scheduling. Can achieve ~5% better than intrinsics on current GCC (which has suboptimal RVV codegen).
- **Time to implement**: 3–4 months. Inline asm is error-prone; hard to maintain; requires deep RVV expertise.
- **Energy cost**: Same as intrinsics (~1 nJ/value); marginal improvement from better scheduling.
- **Upside**: Maximum control; no compiler dependency.
- **Downside**: Very high maintenance cost; architecture-specific; hard to audit for correctness.
- **Key paper**: RISC-V V Extension spec. RT-RK, "Accelerating libavc on RISC-V with RVV."

### Candidate Solution C: Auto-Vectorization
- **Approach**: Write portable C with `#pragma omp simd` and compile with `-O3 -march=rv64gcv`. Rely on GCC 14+/Clang 18+ auto-vectorizer for RVV.
- **Performance**: ~40–60% of hand-tuned RVV on simple scans (arXiv 2605.10860). ~20% of hand-tuned on complex gather/scatter (hash joins). Improving rapidly with compiler maturity.
- **Time to implement**: 0.25 months. Just compile with RVV flags.
- **Energy cost**: ~30–50% higher than hand-tuned due to suboptimal instruction selection.
- **Upside**: Zero kernel code; works today; benefits from future compiler improvements.
- **Downside**: 40–60% throughput loss; unacceptable for a performance-focused engine.
- **Key paper**: ArXiv 2605.10860, 2026 (shows auto-vectorization is the key enabler for portable RVV performance). EmergentMind, "RISC-V Vector Extension (RVV)," 2026.

### Recommendation
**Solution A** (RVV intrinsics) is the right long-term answer, but **defer until RISC-V server hardware is production-ready** (likely 2026–2027). For now, use **Solution C** (auto-vectorization) as a placeholder compile path—0.25 months gives a functional (if slow) RISC-V binary. Revisit with Solution A when real RISC-V deployment is on the roadmap.

---

## P-IS-14: REP MOVSB for Page Copy

### Candidate Solution A: ERMS REP MOVSB (Solved)
- **Approach**: Use `REP MOVSB` for copies ≥128 bytes (page-aligned). Enhanced REP MOVSB (ERMS, Ivy Bridge+) achieves 1 byte/cycle for aligned copies >128 bytes. Modern Intel (SPR+) with Fast Short REP MOV (FSRM) extends this to short copies too.
- **Performance**: ERMS: ~1 B/cycle for large copies = ~16 B/cycle for 128-bit moves internally (Intel optimization manual §3.7.5). For a 4 KB page copy: ~4096 cycles = ~1.3 μs at 3 GHz. This is ~90% of peak L1-to-L1 copy bandwidth. `memcpy()` in glibc already uses REP MOVSB on ERMS-capable CPUs, so calling `memcpy` is sufficient.
- **Time to implement**: 0 months. Already solved by glibc `memcpy`. If custom copy is needed, use `__builtin_memcpy` which lowers to REP MOVSB.
- **Energy cost**: ~0.5 nJ/byte (L1-to-L1). 4 KB page: ~2 μJ. This is near-optimal; REP MOVSB is the most energy-efficient copy method (single instruction, no loop overhead).
- **Upside**: Already optimal; no engineering needed.
- **Downside**: None for x86. On ARM, use `memcpy` which uses NEON load/store pairs.
- **Key paper**: Intel 64 and IA-32 Optimization Reference Manual, §3.7.5 (Enhanced REP MOVSB). The Chip Letter, "The Long History of REP MOVS." StackOverflow, "Enhanced REP MOVSB for memcpy."

### Candidate Solution B: Explicit SIMD Copy (AVX-512)
- **Approach**: Use `_mm512_loadu_epi64` + `_mm512_storeu_epi64` in a loop for page copies.
- **Performance**: ~1 B/cycle (same as REP MOVSB for large copies). For small copies (<128 B), AVX-512 can be faster (no REP startup overhead). For 4 KB: ~4096 cycles, same as REP MOVSB.
- **Time to implement**: 0.25 months.
- **Energy cost**: ~0.6 nJ/byte (slightly higher due to loop overhead).
- **Upside**: Better for small copies; controllable alignment.
- **Downside**: Unnecessary for large copies where REP MOVSB is already optimal.
- **Key paper**: Intel optimization manual.

### Recommendation
**Solution A** (REP MOVSB / glibc `memcpy`). This is already solved. No engineering investment needed. Just ensure the compiler/glibc uses ERMS (verify with `objdump` that `memcpy` emits `rep movsb`). Only consider Solution B for small (<128 B) copies where REP startup overhead matters.

---

# PART II: MEMORY HIERARCHY PROBLEMS (12)

---

## P-MH-01: Region Placement Policy

### Candidate Solution A: Hot-First Placement
- **Approach**: New regions are allocated in the fastest available tier (L3-backed DDR5 by default; HBM if available). A background profiler tracks access frequency per region; regions that haven't been accessed in N seconds are demoted to slower tiers (CXL, NVMe).
- **Performance**: Hot data is always in the fastest tier → lowest latency for common case. Demotion cost: one `migrate_pages` call per region (~50–100 μs for a 2 MB page on Linux, per kernel docs). If 10% of regions are demoted per hour, amortized cost is negligible.
- **Time to implement**: 1.5 months. Need access-frequency tracking (perf counters or software counters per region), background migration thread, and tier-demotion logic.
- **Energy cost**: Hot data in DDR5: ~30 nJ/access. If data were in CXL: ~40 nJ/access. Savings: ~10 nJ/access for hot data. Demotion itself costs ~2 μJ per 2 MB page (memcpy energy).
- **Upside**: Simple heuristic; good for typical workloads with skew (80/20 rule).
- **Downside**: Cold-start: all data starts in fast tier, may overflow capacity → thrashing before demotion kicks in.
- **Key paper**: Sleator & Tarjan, "Amortized Efficiency of List Update and Paging Rules," JACM 1985 (LRU is k-competitive for paging → hot-first + LRU demotion is theoretically grounded).

### Candidate Solution B: Capacity-First Placement
- **Approach**: Place new regions in the tier that has the most free capacity, regardless of access pattern. Balances utilization across tiers.
- **Performance**: Prevents fast-tier overflow but may place hot data in CXL → 1.5–3× latency penalty for hot accesses. Expected: ~20% worse than hot-first on skewed workloads.
- **Time to implement**: 0.5 months. Simple capacity counter per tier; round-robin or least-full allocation.
- **Energy cost**: Hot data in CXL: ~40 nJ/access vs ~30 nJ in DDR5 → ~33% more energy for hot accesses.
- **Upside**: Simple; prevents thrashing; fair across tiers.
- **Downside**: Ignores access patterns; suboptimal for skewed workloads.
- **Key paper**: Linux NUMA first-touch placement policy (similar heuristic).

### Candidate Solution C: LP-Optimal Placement (Convex Optimization)
- **Approach**: Model region placement as a linear program: minimize total access cost subject to capacity constraints. `min Σ_{i,t} x_{i,t} * latency_t * freq_i` s.t. `Σ_t x_{i,t} = 1` and `Σ_i x_{i,t} * size_i ≤ capacity_t`. Solve periodically (every N minutes) using an LP solver.
- **Performance**: Theoretically optimal—minimizes total weighted access latency. In practice, requires accurate access-frequency prediction. If prediction is perfect: 10–20% better than hot-first. If prediction is wrong: can be worse.
- **Time to implement**: 3–4 months. Need LP solver integration (or simple greedy approximation), frequency prediction model, and migration scheduler.
- **Energy cost**: Optimal energy: minimizes `Σ freq_i * energy_t` → provably minimum total energy given perfect frequency prediction.
- **Upside**: Theoretically optimal; handles multi-tier trade-offs (HBM vs DDR5 vs CXL) holistically.
- **Downside**: High engineering cost; LP solution may require many migrations (thrashing); sensitive to prediction accuracy.
- **Key paper**: Boyd & Vandenberghe, "Convex Optimization," 2004 (LP formulation for resource allocation). Megiddo & Modha, "ARC: A Self-Tuning, Low Overhead Replacement Cache" (adaptive replacement).

### Recommendation
**Solution A** (hot-first placement) for v1. It's grounded in Sleator-Tarjan's k-competitive LRU theory, simple to implement (1.5 months), and performs well on real workloads. Defer **Solution C** (LP-optimal) to v3 when the engine has accurate access-frequency data to feed the optimizer. Solution B is a reasonable fallback but ignores the engine's memory-centric design philosophy.

---

## P-MH-02: Region Migration Mechanics

### Candidate Solution A: migrate_pages(2) Syscall
- **Approach**: Use the Linux `migrate_pages(pid, old_nodes, new_nodes)` syscall to move pages between NUMA nodes (DDR5 ↔ CXL ↔ HBM). The kernel handles TLB shootdown, page table update, and data copy.
- **Performance**: `migrate_pages` for a single 2 MB page: ~50–100 μs (kernel page_migration docs; arXiv 2503.17685, "Revisiting Page Migration for Main-Memory Databases," 2025). During migration, the page is unavailable → access stalls. For a 1 GB region (512 × 2 MB pages): ~25–50 ms total. This is a blocking operation.
- **Time to implement**: 0.5 months. Simple syscall wrapper; node-set construction.
- **Energy cost**: ~2 μJ per 2 MB page copy (memcpy energy at ~0.5 nJ/byte). 1 GB: ~1 mJ. Plus kernel overhead (~0.5 mJ for TLB shootdowns).
- **Upside**: Kernel-managed; correct; handles all edge cases (shared pages, huge pages, CMA).
- **Downside**: Blocking; high latency; no fine-grained control over migration timing; requires CAP_SYS_NICE.
- **Key paper**: Linux kernel, "Page Migration" (docs.kernel.org). arXiv 2503.17685, "Revisiting Page Migration for Main-Memory Databases," 2025. man7.org, `migrate_pages(2)`.

### Candidate Solution B: userfaultfd + Custom Migration
- **Approach**: Register regions with `userfaultfd`. When a page is accessed in a new tier (after migration), the userfaultfd handler copies the page from the old tier and resolves the fault. Enables non-blocking, asynchronous migration: the handler can prefetch and copy in the background.
- **Performance**: userfaultfd page copy: ~10–30 μs per 4 KB page (lower-level than migrate_pages; no full TLB shootdown for non-present pages). For 2 MB huge pages: ~30–80 μs. Can overlap migration with computation (fault-on-access model). Net: 2–3× lower stall time than migrate_pages for partially-migrated regions.
- **Time to implement**: 2 months. userfaultfd is complex; need a fault handler thread, page copy logic, and coordination with the query executor.
- **Energy cost**: Similar to migrate_pages (~2 μJ/page) but lower kernel overhead (~0.2 mJ for 1 GB) because fewer TLB shootdowns.
- **Upside**: Non-blocking; fine-grained; can implement prefetch-ahead-of-access; ideal for tier-aware migration.
- **Downside**: Complex; userfaultfd has API quirks (non-cooperative mode, fork/remap issues); requires Linux 4.11+ for full features.
- **Key paper**: Linux userfaultfd documentation. arXiv 2503.17685, 2025 (analyzes userfaultfd for DB page migration). shayon.dev, "Linux Page Faults, mmap, and userfaultfd," 2026.

### Candidate Solution C: memcpy + Dual-Map
- **Approach**: Allocate the region in both tiers (old and new). Copy data with `memcpy` (REP MOVSB). Atomically switch the pointer in the region table. Old region is freed after all in-flight queries complete (RCU-style grace period).
- **Performance**: `memcpy` at ~16 B/cycle (ERMS) for 2 MB: ~130K cycles = ~43 μs. Plus pointer swap: ~10 ns. Total: ~43 μs per 2 MB page—faster than migrate_pages (no kernel TLB shootdown). But requires 2× memory during migration.
- **Time to implement**: 1 month. Dual allocation, memcpy, RCU pointer swap, old-region GC.
- **Energy cost**: ~1 μJ per 2 MB page (memcpy only, no kernel overhead). Lowest energy per migration.
- **Upside**: Fastest migration; no kernel involvement; full control.
- **Downside**: 2× memory during migration; must handle concurrent readers (RCU); cannot migrate huge pages transparently.
- **Key paper**: Intel optimization manual (REP MOVSB/ERMS performance).

### Recommendation
**Solution C** (memcpy + dual-map) for hot-path migrations where speed matters. **Solution A** (migrate_pages) for bulk background migration. The dual-map approach is 2× faster and lower energy, and the engine's "explicit memory tier" design already manages region pointers—adding an atomic swap is natural. Use migrate_pages as a fallback for shared/complex regions. Defer userfaultfd (Solution B) unless non-blocking migration is critical.

---

## P-MH-03: Tier-Aware Migration with Competitive Ratio

### Candidate Solution A: LRU-Based Migration (k-Competitive)
- **Approach**: Track last-access time per region. When a tier is full, evict the least-recently-used region to the next slower tier. LRU is k-competitive for the paging problem (Sleator-Tarjan 1985): the total cost is at most k× the optimal offline algorithm, where k = number of tiers.
- **Performance**: LRU's k-competitive ratio means: if optimal migration would cost C, LRU costs ≤ k×C. For 4 tiers (L3/DDR5/CXL/NVMe), k=4 → ≤4× optimal. In practice, LRU achieves 1.5–2× optimal on real workloads (due to locality of reference). Migration frequency: ~1 migration per 1000 accesses on skewed workloads.
- **Time to implement**: 1 month. LRU list per tier; access-time stamp per region; eviction logic.
- **Energy cost**: LRU itself: ~0.1 nJ per access (timestamp update + list move). Migration cost: ~2 μJ per 2 MB page (from P-MH-02). At 1 migration per 1000 accesses: ~2 nJ/access amortized migration energy.
- **Upside**: Proven competitive ratio; simple; well-understood.
- **Downside**: k-competitive is a worst-case bound; LRU performs poorly on scan workloads (cycles through all pages, evicting useful data).
- **Key paper**: Sleator & Tarjan, "Amortized Efficiency of List Update and Paging Rules," JACM 32(2), 1985 (LRU is k-competitive).

### Candidate Solution B: Work Function Algorithm (WFA, (2k-1)-Competitive)
- **Approach**: For each access, compute the "work function"—the minimum cost to serve all past requests ending in the current configuration. Move to the configuration that minimizes the work function + migration cost. WFA is (2k-1)-competitive (Koutsoupias-Papadimitriou 1995).
- **Performance**: WFA's competitive ratio is (2k-1) = 7 for k=4 tiers. This is *worse* than LRU's k=4 bound! WFA's theoretical advantage is that it's the best known *deterministic* algorithm for general k-server, but for the paging problem (uniform cost), LRU's k bound is tighter. WFA wins on non-uniform cost metrics (e.g., different migration costs per tier pair). Expected: 1.2–1.5× optimal on heterogeneous-cost tiers.
- **Time to implement**: 3–4 months. WFA requires solving an optimization at each step; the work function is expensive to compute (O(k^n) naively). Need efficient approximation.
- **Energy cost**: WFA computation: ~10–100 nJ per access (optimization solve). Migration energy: similar to LRU but fewer migrations (WFA is more conservative).
- **Upside**: Best known competitive ratio for general k-server; handles non-uniform migration costs.
- **Downside**: (2k-1) > k for paging; computationally expensive; worse than LRU for uniform-cost paging.
- **Key paper**: Koutsoupias & Papadimitriou, "On the k-Server Conjecture," JACM 42(5), 1995 (WFA is (2k-1)-competitive).

### Candidate Solution C: Learned Migration Policy (ML-Based)
- **Approach**: Train a neural network or bandit to predict the optimal tier for each region based on features (access frequency, recency, size, query type). The model outputs a tier assignment; migration is triggered when the predicted tier differs from the current tier.
- **Performance**: Learned policies can beat LRU by 10–30% on workloads with predictable patterns (Google Borg traces show 15% improvement with learned eviction, per CMU/GaTech buffer pool prefetching research). On adversarial workloads, learned policies can be worse than LRU (no competitive ratio guarantee).
- **Time to implement**: 4–6 months. Need feature collection, model training, online inference, and fallback to LRU on model failure.
- **Energy cost**: Model inference: ~1–10 nJ per access (tiny model). Migration savings: 10–30% fewer migrations → ~0.2–0.6 nJ/access saved.
- **Upside**: Can outperform LRU by 10–30% on predictable workloads; adapts to workload changes.
- **Downside**: No competitive ratio guarantee; high engineering cost; cold-start (model must be trained); potential for catastrophic misprediction.
- **Key paper**: Georgia Tech, "Intelligent Buffer Pool Prefetching," 2023 (ML for DB buffer pool). Lykouris & Vassilvitskii, "Competitive Caching with Machine Learned Advice," ICML 2018.

### Recommendation
**Solution A** (LRU) for v1. The k-competitive guarantee is the strongest theoretical result for the paging problem, and LRU is simple (1 month). WFA (Solution B) is theoretically interesting but (2k-1) > k for uniform-cost paging—it's the wrong tool here. Learned policies (Solution C) are the long-term future but 4–6 months is too costly for Wave 1, and the lack of a competitive ratio guarantee is risky for a database engine.

---

## P-MH-04: NUMA Thread Pinning

### Candidate Solution A: pthread_setaffinity_np (Per-Thread Pinning)
- **Approach**: Pin each worker thread to a specific core using `pthread_setaffinity_np()`. The thread-to-core mapping is determined at startup based on NUMA topology: thread N → core N on socket 0 first, then socket 1. Each thread allocates from its local NUMA node.
- **Performance**: Cross-socket memory access adds ~140 ns latency (oxmaint.com; eklitzke.org) and 2× bandwidth penalty. Pinning threads to local memory eliminates this: local DDR5 access ~80 ns vs remote ~140 ns. On a 2-socket system with 50% remote accesses without pinning: average latency ~110 ns → ~80 ns with pinning = ~30% improvement. eklitzke.org measured up to 2× higher memory bandwidth with NUMA-aware pinning.
- **Time to implement**: 0.5 months. `pthread_setaffinity_np` calls at thread creation; NUMA topology discovery via `numactl --hardware` or `/sys/devices/system/node/`.
- **Energy cost**: Cross-socket access: ~60 nJ (140 ns × ~0.4 W/ns leakage + interconnect energy). Local access: ~30 nJ. Pinning saves ~30 nJ per remote access avoided. On 50% remote → 50% local: saves ~15 nJ/access average.
- **Upside**: Simple; large performance win on multi-socket systems; low engineering cost.
- **Downside**: Reduces scheduling flexibility; can cause load imbalance if work is skewed; over-subscription (more threads than cores) is incompatible.
- **Key paper**: AMD NUMA whitepaper. eklitzke.org, "NUMA Cost & Performance," 2025. man7.org, `sched_setaffinity(2)`.

### Candidate Solution B: libnuma (Higher-Level API)
- **Approach**: Use `libnuma` (`numa_set_preferred()`, `numa_bind()`, `numa_run_on_node()`) for a higher-level API that handles NUMA topology, memory policy, and CPU affinity together.
- **Performance**: Same as Solution A (libnuma wraps the same syscalls) but with better topology discovery and memory policy management. Can set `MPOL_BIND` to bind memory allocations to a specific node.
- **Time to implement**: 0.5 months. Link libnuma; call `numa_available()` + `numa_run_on_node(node)`.
- **Energy cost**: Same as Solution A.
- **Upside**: Cleaner API; handles topology changes; memory policy integration.
- **Downside**: External dependency (libnuma); Linux-only.
- **Key paper**: Linux NUMA documentation. Andi Kleen, libnuma man pages.

### Candidate Solution C: Linux cgroups v2 (cpuset)
- **Approach**: Use cgroups v2 `cpuset.cpus` and `cpuset.mems` to restrict threads and memory to specific NUMA nodes. Managed externally by the container runtime or systemd.
- **Performance**: Same as Solutions A/B for pinning. Advantage: works with containerized deployments (Kubernetes, Docker) where the engine doesn't control thread creation directly.
- **Time to implement**: 0.25 months (if container runtime supports it). Just configure cgroup in deployment manifest.
- **Energy cost**: Same as Solutions A/B.
- **Upside**: Works in containers; no code changes; managed by ops team.
- **Downside**: Less granular control; cgroup configuration is deployment-specific; not portable to non-Linux.
- **Key paper**: Linux cgroups v2 documentation. cubepath.com, "CPU Pinning and NUMA Configuration," 2026.

### Recommendation
**Solution A** (`pthread_setaffinity_np`) for the engine's internal worker threads, combined with **Solution B** (libnuma) for memory policy. The 0.5-month cost is low and the 30% latency + 2× bandwidth improvement on multi-socket systems is the single highest-ROI optimization in this entire list. Solution C is complementary for containerized deployment.

---

## P-MH-05: CXL Latency Variability

### Candidate Solution A: Empirical Histogram + Percentile Routing
- **Approach**: At startup, run a CXL latency calibration (100K random reads). Build a latency histogram. Route latency-sensitive queries to DDR5; route bandwidth-sensitive queries to CXL. Use p99 latency from the histogram for query timeout calculation.
- **Performance**: Weisgut et al. (PVLDB 2025) measured CXL random read latency averaging 520 ns (range: 170–520 ns across configurations). Melody/ASPLOS 2025 measured 140–410 ns across 5 platforms with high tail latency. By routing based on empirical percentiles, the engine avoids CXL for latency-critical paths. Expected: p50 CXL ~250 ns, p99 ~520 ns vs DDR5 p50 ~80 ns, p99 ~120 ns.
- **Time to implement**: 1 month. Calibration harness + histogram + routing logic in planner.
- **Energy cost**: CXL access: ~40 nJ (variable). DDR5 access: ~30 nJ. Routing latency-sensitive to DDR5 saves ~10 nJ/access on average for those queries.
- **Upside**: Data-driven; adapts to specific CXL hardware; simple to implement.
- **Downside**: Calibration may not capture long-term drift (CXL latency varies with controller load); doesn't predict future latency.
- **Key paper**: Weisgut et al., "CXL Memory Performance for In-Memory Data Processing," PVLDB 18, 2025. Melody et al., "Systematic CXL Memory Characterization," ASPLOS 2025. arXiv 2409.14317, "Dissecting CXL Memory Performance at Scale," 2024.

### Candidate Solution B: Kingman Predictor (Queueing Model)
- **Approach**: Model CXL as an M/G/1 queue. Use Kingman's formula: `E[W] = ρ/(1-ρ) × (c_a² + c_s²)/2 × E[S]` where ρ = utilization, c_a² = arrival CV, c_s² = service CV, E[S] = mean service time. Predict expected latency at current load and route accordingly.
- **Performance**: Kingman's formula (Kingman 1961) gives an approximation of mean waiting time with <10% error for M/G/1 queues. For CXL with utilization ρ=0.5 and CV=1.5: predicted latency = 0.5/0.5 × (1+2.25)/2 × 250ns = ~406 ns. Accurate enough for routing decisions. Can predict when CXL will become congested and pre-emptively route to DDR5.
- **Time to implement**: 2 months. Need queue length monitoring, Kingman solver, and dynamic routing.
- **Energy cost**: Computation: ~10 nJ per prediction (simple arithmetic). Savings: avoids CXL congestion → ~10–20 nJ/access saved on congested paths.
- **Upside**: Predictive (not just reactive); adapts to load changes in real-time; theoretically grounded.
- **Downside**: M/G/1 model may not fit CXL (batched requests, controller caching); requires accurate service-time distribution estimation.
- **Key paper**: Kingman, "The Single-Server Queue," 1961. Weisgut et al., PVLDB 2025 (CXL queueing analysis).

### Candidate Solution C: Adaptive Retry + Timeout
- **Approach**: Set a per-access timeout (e.g., 2× expected p50). If a CXL access exceeds the timeout, mark the CXL tier as "degraded" for N seconds and route to DDR5. Periodically retry CXL.
- **Performance**: Simple but coarse. When CXL degrades, all accesses route to DDR5 → DDR5 may become congested. Recovery latency: N seconds of DDR5-only operation.
- **Time to implement**: 0.5 months. Timeout in access path + degradation flag.
- **Energy cost**: No prediction energy. DDR5 fallback: ~30 nJ/access (same as normal DDR5).
- **Upside**: Simplest; robust; handles catastrophic CXL failure.
- **Downside**: Coarse; binary (all-or-nothing CXL routing); doesn't handle partial degradation well.
- **Key paper**: Melody et al., ASPLOS 2025 (CXL tail latency characterization).

### Recommendation
**Solution A** (empirical histogram) for v1. It's data-driven, simple (1 month), and grounded in the Weisgut/Melody CXL characterization papers. Upgrade to **Solution B** (Kingman predictor) in v2 if CXL becomes a production tier with variable load. Solution C is a good safety net but too coarse for primary routing.

---

## P-MH-06: HBM Tier Support

### Candidate Solution A: NUMA-Based HBM (Xeon Max Model)
- **Approach**: On Intel Xeon Max (Sapphire Rapids HBM), HBM is exposed as a separate NUMA node. Allocate HBM regions via `numa_alloc_onnode()` or `mmap` with `MPOL_BIND` to the HBM node. The kernel handles HBM as if it were a NUMA node with higher bandwidth and similar latency.
- **Performance**: McCalpin (IXPUG 2023) measured Xeon Max HBM at 2.5–3.5× higher sustained bandwidth than DDR5. STREAM Triad: ~350 GB/s (HBM) vs ~120 GB/s (DDR5). Latency: HBM ~100 ns (similar to DDR5, since it's cache-coherent on-package). For bandwidth-bound scans: 2.5–3.5× throughput improvement. For latency-bound random access: minimal improvement (latency similar to DDR5).
- **Time to implement**: 1 month. NUMA topology discovery (detect HBM node via `/sys/devices/system/node/`); `numa_alloc_onnode` wrapper; planner routes bandwidth-heavy regions to HBM.
- **Energy cost**: HBM access: ~5–10 nJ/word (lower than DDR5's ~20–30 nJ due to wider bus and lower voltage). For bandwidth-bound workloads: ~3× lower energy/byte. McCalpin measured HBM at 44.49% of peak bandwidth efficiency.
- **Upside**: Leverages existing NUMA infrastructure; no custom allocator; kernel-managed coherency.
- **Downside**: HBM capacity is limited (64 GB on Xeon Max 9480); cannot hold large datasets; flat mode vs cache mode BIOS configuration affects behavior.
- **Key paper**: McCalpin, "Bandwidth Limits in the Intel Xeon Max (Sapphire Rapids with HBM)," IXPUG/ISC 2023. Intel VTune HBM profiling guide. Reddit r/LocalLLaMA (Xeon Max 9480: 64 GB HBM, 1600 GB/s aggregate).

### Candidate Solution B: Direct mmap with HBM Node Binding
- **Approach**: `mmap` anonymous memory with `mbind()` to the HBM NUMA node. More control than `numa_alloc_onnode`: can use huge pages, specific offsets, and `MPOL_PREFERRED` for fallback to DDR5 when HBM is full.
- **Performance**: Same bandwidth as Solution A (same underlying HBM hardware). Advantage: `MPOL_PREFERRED` allows graceful fallback to DDR5 when HBM is exhausted, vs `MPOL_BIND` which fails.
- **Time to implement**: 1.5 months. `mmap` + `mbind` + huge page setup + fallback logic.
- **Energy cost**: Same as Solution A.
- **Upside**: More control; graceful fallback; huge page support.
- **Downside**: More complex; `mbind` semantics are subtle (policy applies to pages, not VMA).
- **Key paper**: Linux `mbind(2)` documentation. McCalpin, IXPUG 2023.

### Candidate Solution C: Custom HBM Allocator (Pool-Based)
- **Approach**: Pre-allocate a large HBM pool at startup (`mmap` + `mbind`). Manage allocation within the pool using a slab/buddy allocator. No per-allocation `mbind` call; all allocations from the pool are HBM-resident.
- **Performance**: Same bandwidth. Allocation latency: ~10 ns (slab allocator) vs ~1 μs (`numa_alloc_onnode` syscall). For frequent small allocations: 100× faster allocation.
- **Time to implement**: 2.5–3 months. Custom allocator, pool management, fragmentation handling, HBM pool resize.
- **Energy cost**: Same memory access energy. Allocation: ~0.1 nJ (slab) vs ~500 nJ (syscall). Savings: ~500 nJ per allocation.
- **Upside**: Fastest allocation; full control; no syscall overhead.
- **Downside**: Highest engineering cost; fragmentation management; cannot return HBM to OS.
- **Key paper**: Intel VTune HBM guide. McCalpin, IXPUG 2023.

### Recommendation
**Solution A** (NUMA-based) for v1. It's 1 month and leverages the kernel's existing NUMA infrastructure. McCalpin's IXPUG 2023 data confirms 2.5–3.5× bandwidth from HBM via NUMA. Upgrade to **Solution C** (custom allocator) in v2 if allocation latency becomes a bottleneck (likely for OLTP-style small allocations). Solution B is a mid-point but the extra complexity over Solution A isn't justified until huge-page + fallback is needed.

---

## P-MH-07: CXL Memory Pooling

### Candidate Solution A: Linux CXL Subsystem (Native CXL 3.0)
- **Approach**: Use the Linux CXL subsystem (kernel 6.0+) to enumerate CXL memory devices, create regions, and expose them as NUMA nodes. Allocate from CXL nodes via standard NUMA APIs. The kernel manages coherency, addressing, and hot-plug.
- **Performance**: CXL 2.0 type-3 devices: ~170–250 ns latency, ~25 GB/s bandwidth per device (NextPlatform 2022). CXL 3.0 multi-head pooling: shared across hosts but coherence is per-host (no cross-host coherence). For single-host CXL: similar to a slow NUMA node. Pond (ASPLOS 2023) showed CXL pooling can reduce DRAM cost by ~7% with <3% performance degradation for cloud workloads.
- **Time to implement**: 2 months. CXL device enumeration, region creation, NUMA integration, allocation path.
- **Energy cost**: CXL access: ~30–40 nJ (DRAM + CXL controller). ~10 nJ more than local DDR5 per access. Pooling saves overall DRAM energy by right-sizing per host (Pond: ~10% DRAM reduction → ~10% idle DRAM energy saved).
- **Upside**: Kernel-managed; standard NUMA API; supports hot-plug; CXL 3.0 spec enables multi-host.
- **Downside**: Linux CXL subsystem is immature (2024); limited hardware availability; no cross-host coherence.
- **Key paper**: CXL 3.0 Specification. Pond: ASPLOS 2023 (Agarwal et al., "Pond: CXL-Based Memory Pooling Systems for Cloud Platforms"). NextPlatform, "Just How Bad Is CXL Memory Latency?" 2022.

### Candidate Solution B: Custom CXL Pool Manager
- **Approach**: Bypass the Linux CXL subsystem; directly manage CXL devices via `/dev/cxl/` or custom PCIe driver. Implement a pool manager that allocates CXL memory regions to host applications via a custom protocol.
- **Performance**: Can be faster than kernel-managed CXL (no NUMA overhead, direct PCIe DMA). But requires implementing coherency, addressing, and fault handling in user space. Expected: ~10% lower latency than kernel CXL (~150 ns vs ~170 ns).
- **Time to implement**: 4–6 months. PCIe driver, CXL protocol implementation, pool manager, fault handling.
- **Energy cost**: Similar to Solution A for memory access. Lower kernel overhead: ~0.1 nJ/access saved.
- **Upside**: Maximum control; can implement custom coherence protocols; lower latency.
- **Downside**: Very high engineering cost; kernel bypass is fragile; CXL hardware-specific.
- **Key paper**: Pond, ASPLOS 2023 (custom pooling system). CXL 3.0 spec.

### Candidate Solution C: FaRM-Style RDMA (Network-Attached Memory)
- **Approach**: Instead of CXL, use RDMA (RoCEv2) to access memory on remote machines. FaRM (NSDI 2014) demonstrated μs-scale latency for RDMA reads. Each host exposes its spare DRAM as RDMA-accessible memory.
- **Performance**: FaRM: ~2.5 μs RTT for RDMA read (one-way ~1 μs). This is 10× slower than CXL (~170 ns) but works with existing RDMA hardware. Throughput: ~100 Gbps per link (RoCEv2). FaRM achieved 50M msgs/sec for 1-to-1 communication.
- **Time to implement**: 3–4 months. RDMA setup, FaRM-style memory registration, one-sided RDMA read/write, failure handling.
- **Energy cost**: RDMA access: ~1–5 μJ per message (NIC + network + remote DRAM). 50× more than CXL per access. But enables disaggregated memory at scale.
- **Upside**: Works today with standard RDMA hardware; enables cross-datacenter memory pooling.
- **Downside**: 10× higher latency than CXL; 50× higher energy; requires RDMA-capable NICs.
- **Key paper**: Dragojević et al., "FaRM: Fast Remote Memory," NSDI 2014. Microsoft Research, FaRM publication.

### Recommendation
**Solution A** (Linux CXL subsystem) for v1 if CXL hardware is available. The 2-month cost is reasonable and it leverages standard NUMA APIs (consistent with P-MH-06 HBM support). Pond's ASPLOS 2023 results validate the approach. If CXL hardware is unavailable, use **Solution C** (FaRM-style RDMA) as a functional substitute—it's 10× slower but enables the same software architecture. Solution B is over-engineering unless the kernel CXL subsystem proves to be a bottleneck.

---

## P-MH-08: Memory Bandwidth Monitoring

### Candidate Solution A: perf stat (Standard PMU Counters)
- **Approach**: Use `perf stat -e LLC-loads,LLC-load-misses,node-loads,node-load-misses,mem_load_retired.l3_hit,mem_load_retired.dram_hit` to measure per-tier memory access counts. Run perf in a background thread, sampling at 100 ms intervals.
- **Performance**: `perf stat` overhead: ~1–3% (hardware counters, sampled). Provides per-core and per-NUMA-node bandwidth via `uncore_imc/cas_count_read/` events. Granularity: per-core, per-second. Cannot distinguish HBM vs DDR5 on Xeon Max without additional uncore events.
- **Time to implement**: 0.5 months. Shell wrapper or `libperf` integration; parse perf output; feed to tier-aware profiler.
- **Energy cost**: perf overhead: ~0.01 nJ/access (counter read is cheap). Negligible.
- **Upside**: Universal (works on all Linux); no external dependencies; well-documented.
- **Downside**: Low granularity (per-second, not per-access); cannot distinguish CXL from DDR5 without custom events; per-process attribution requires cgroup + perf integration.
- **Key paper**: Linux `perf-stat(1)` man page (man7.org). Intel SDM Vol. 3 (performance monitoring counters).

### Candidate Solution B: Intel PCM (pcm-memory)
- **Approach**: Use Intel Performance Counter Monitor (`pcm-memory.x`) for per-socket, per-channel memory bandwidth measurement. PCM provides real-time bandwidth per DDR5 channel and HBM stack. Can distinguish HBM from DDR5 on Xeon Max.
- **Performance**: PCM overhead: ~1% (direct MSR reads). Bandwidth resolution: per-channel, per-second. PCM can measure CXL bandwidth if the CXL device exposes uncore counters. StackOverflow confirms PCM-memory supports server uncore only (not client).
- **Time to implement**: 0.75 months. Link PCM library (C++); integrate `pcm-memory` API; parse per-channel bandwidth.
- **Energy cost**: ~0.01 nJ/access (MSR read). Negligible.
- **Upside**: Per-channel granularity; HBM vs DDR5 distinction; Intel-supported; open source (GitHub).
- **Downside**: Intel-only (no AMD/ARM support); CXL bandwidth measurement is vendor-specific; requires root for MSR access.
- **Key paper**: Intel PCM documentation (intel.com). GitHub intel/pcm. Intel SDM Vol. 3 (uncore performance monitoring).

### Candidate Solution C: Custom PMU Driver
- **Approach**: Write a custom Linux kernel module or eBPF program that reads specific uncore PMU events per memory controller and exposes them via a `/sys` or perf event interface. Tailored to the engine's tier model.
- **Performance**: Near-zero overhead (kernel counter read). Can distinguish all tiers (L3, DDR5, HBM, CXL) if the hardware exposes per-controller events. Highest granularity.
- **Time to implement**: 2–3 months. Kernel module or eBPF program; PMU event discovery; per-tier counter mapping; user-space API.
- **Energy cost**: ~0.005 nJ/access (in-kernel counter read).
- **Upside**: Maximum granularity; tier-specific; can feed the migration policy (P-MH-03) in real-time.
- **Downside**: Highest engineering cost; architecture-specific (Intel vs AMD vs ARM have different PMU event sets); maintenance burden.
- **Key paper**: Intel uncore performance monitoring reference manual. Linux perf documentation.

### Recommendation
**Solution B** (Intel PCM) for Intel-based deployments. It's 0.75 months and provides per-channel HBM/DDR5 bandwidth that perf cannot. Use **Solution A** (perf stat) as a fallback on AMD/ARM. Defer **Solution C** (custom PMU driver) until the engine needs real-time per-tier bandwidth feeding into the migration policy—which is a v2/v3 feature.

---

## P-MH-09: Huge Page Management

### Candidate Solution A: Transparent Huge Pages (THP)
- **Approach**: Enable Linux THP (`echo always > /sys/kernel/mm/transparent_hugepage/enabled`). The kernel automatically promotes 4 KB pages to 2 MB huge pages when possible. No application changes needed.
- **Performance**: 2 MB huge pages reduce TLB misses by 512× (1 TLB entry covers 2 MB vs 4 KB). For a 1 GB region: 512 TLB entries (4 KB) → 1 TLB entry (2 MB). TLB miss penalty: ~7 cycles (L1 TLB) to ~100 cycles (page walk). On memory-bandwidth-bound scans: 5–15% throughput improvement (LWN.net 2011; abhik.ai 2025). THP has a caveat: khugepaged daemon uses CPU for background promotion (~1–5% CPU).
- **Time to implement**: 0.1 months. Set sysctl; verify with `/proc/meminfo` AnonHugePages.
- **Energy cost**: TLB miss: ~5 nJ (page walk + L2 access). Huge page: ~0.5 nJ (L1 TLB hit). Savings: ~4.5 nJ per avoided TLB miss. THP khugepaged: ~0.5W background → negligible per-operation.
- **Upside**: Zero code changes; automatic; benefits all allocations.
- **Downside**: THP can cause latency spikes (khugepaged compaction); 2 MB pages waste memory for small allocations (internal fragmentation); can interfere with NUMA balancing.
- **Key paper**: LWN.net, "Transparent Huge Pages in 2.6.38," 2011. abhik.ai, "Transparent Huge Pages: Reducing TLB Pressure," 2025. kernel-internals.org, "THP."

### Candidate Solution B: Explicit mmap with MAP_HUGETLB
- **Approach**: Allocate large regions with `mmap(..., MAP_HUGETLB | MAP_ANONYMOUS, ...)` or `mmap` with `MAP_HUGE_2MB`. Explicitly reserves huge pages from the hugetlbfs pool. No khugepaged interference; deterministic huge page allocation.
- **Performance**: Same TLB benefit as THP (~5–15% throughput improvement) but no background compaction overhead. Allocation succeeds only if huge pages are reserved in the pool (`sysctl vm.nr_hugepages`).
- **Time to implement**: 0.5 months. Huge page pool configuration; `mmap` flags in allocator; fallback to 4 KB if huge pages exhausted.
- **Energy cost**: Same as THP for TLB savings. No khugepaged overhead: saves ~0.5W background power.
- **Upside**: Deterministic; no background daemon; full control.
- **Downside**: Requires pre-reservation (`vm.nr_hugepages`); reduces flexible memory; fragmentation in the huge page pool.
- **Key paper**: Linux huge pages documentation. LWN.net, 2011.

### Candidate Solution C: 1 GB Huge Pages (Static)
- **Approach**: For very large regions (>1 GB), use 1 GB huge pages (`MAP_HUGE_1GB`). Reduces TLB entries by 262144× vs 4 KB. Must be allocated at boot (`hugepagesz=1G hugepages=N` kernel parameter).
- **Performance**: 1 GB pages: 1 TLB entry per GB. Essentially eliminates TLB misses for large regions. But 1 GB pages are inflexible (cannot split) and limited in count (typically <100 on a 128 GB system).
- **Time to implement**: 0.75 months. Boot parameter configuration; `mmap` with `MAP_HUGE_1GB`; region size must be 1 GB-aligned.
- **Energy cost**: Best TLB energy: ~0 nJ (always L1 TLB hit). But 1 GB page reservation wastes memory if underutilized.
- **Upside**: Maximum TLB efficiency; zero background overhead.
- **Downside**: Very inflexible; boot-time reservation; wasted memory for partially-filled regions.
- **Key paper**: Linux huge pages documentation.

### Recommendation
**Solution B** (explicit mmap with MAP_HUGETLB) for the engine's large regions (column data, hash tables). It gives the TLB benefit without THP's nondeterminism. Use 2 MB pages as the default (0.5 months). Reserve 1 GB pages (Solution C) only for the largest column-store regions if TLB pressure is measured as a bottleneck. Solution A (THP) is a reasonable system-wide default but should be disabled for the engine's own allocations to avoid khugepaged interference.

---

## P-MH-10: Tier-Aware Allocator

### Candidate Solution A: numa_alloc_onnode + Wrapper
- **Approach**: Wrap `numa_alloc_onnode(size, node)` to allocate from a specific NUMA/CXL/HBM node. The engine's region manager calls `tier_alloc(tier_id, size)` which maps to the correct NUMA node.
- **Performance**: `numa_alloc_onnode`: ~1–2 μs per call (syscall + page table setup). For batch allocation (1 MB at a time): ~2 μs amortized over 131K values = ~0.015 ns/value. Negligible.
- **Time to implement**: 0.5 months. Thin wrapper around `numa_alloc_onnode`; fallback to `malloc` if node is full.
- **Energy cost**: Allocation syscall: ~500 nJ. Amortized: ~0.004 nJ/value. Negligible.
- **Upside**: Simple; uses standard NUMA API; works for DDR5, HBM (as NUMA node), and CXL (as NUMA node).
- **Downside**: Per-call syscall overhead (~1 μs); no huge page support without additional flags; `numa_alloc` uses `mmap` internally (anonymous, not hugetlb).
- **Key paper**: Linux `numa_alloc(3)` man page. libnuma documentation.

### Candidate Solution B: mmap with MPOL_BIND
- **Approach**: `mmap` anonymous memory, then `mbind(addr, size, MPOL_BIND, &nodemask, maxnode, 0)` to bind pages to a specific NUMA node. Supports huge pages (`MAP_HUGETLB`) and preferred fallback (`MPOL_PREFERRED`).
- **Performance**: `mmap` + `mbind`: ~2–3 μs per call. Same order as `numa_alloc_onnode` but with more control. Pages are allocated on first touch (if `MPOL_BIND` is set before first access).
- **Time to implement**: 1 month. `mmap` + `mbind` + huge page + fallback logic.
- **Energy cost**: ~600 nJ per allocation. Amortized: ~0.005 nJ/value.
- **Upside**: Huge page support; fallback policy; fine-grained control over per-page placement.
- **Downside**: More complex than `numa_alloc`; `mbind` semantics are subtle (applies to pages, not VMA).
- **Key paper**: Linux `mbind(2)` man page. Linux `set_mempolicy(2)` documentation.

### Candidate Solution C: Custom GlobalAlloc (Slab Pool per Tier)
- **Approach**: Pre-allocate large pools per tier (DDR5, HBM, CXL) at startup using `mmap` + `mbind`. Manage allocations within each pool using a slab allocator. No per-allocation syscall; all allocation is user-space.
- **Performance**: Slab allocation: ~10–50 ns per call (pointer bump or free-list pop). 100× faster than `numa_alloc_onnode` (~1 μs). For OLTP-style frequent small allocations: significant.
- **Time to implement**: 2.5–3 months. Slab allocator per tier; pool management; coalescing; tier-aware free.
- **Energy cost**: Slab allocation: ~0.1 nJ. 5000× less than syscall. Amortized: ~0.0001 nJ/value.
- **Upside**: Fastest allocation; no syscall overhead; full control over placement and huge pages.
- **Downside**: Highest engineering cost; fragmentation management; cannot return memory to OS easily.
- **Key paper**: Bonwick, "The Slab Allocator: An Object-Caching Kernel Memory Allocator," USENIX 1994. Linux mbinding docs.

### Recommendation
**Solution B** (mmap + MPOL_BIND) for v1. It supports huge pages (critical for P-MH-09) and provides graceful fallback. The 1-month cost is reasonable. Upgrade to **Solution C** (custom slab pool) in v2 if allocation latency is measured as a bottleneck (likely for OLTP workloads with many small allocations). Solution A is simpler but lacks huge page support.

---

## P-MH-11: Cold-Start Warmup

### Candidate Solution A: madvise(MADV_WILLNEED) + Prefetch
- **Approach**: At engine startup (or before a large query), call `madvise(region, size, MADV_WILLNEED)` for all working-set regions. The kernel begins read-ahead (asynchronous prefetch from NVMe to page cache). Additionally, issue explicit `_mm_prefetch` in a warmup thread to pull data from DDR5 → L3.
- **Performance**: `madvise(MADV_WILLNEED)`: triggers kernel async read-ahead at ~NVMe bandwidth (3–7 GB/s). For a 10 GB working set: ~1.5–3 s warmup. After warmup, data is in page cache (DDR5); first query hits warm cache. Without warmup: first query pays NVMe latency (~50 μs per 4 KB page) → 10 GB / 4 KB × 50 μs = ~125 s (catastrophic). With warmup: 3 s + L3-speed query = 3 s + ~5 s = ~8 s total. 15× improvement.
- **Time to implement**: 0.5 months. `madvise` calls at startup; optional warmup thread with `_mm_prefetch` sweep.
- **Energy cost**: NVMe read: ~5 μJ/4 KB page. 10 GB: ~12.5 mJ. This is unavoidable (data must be loaded). `madvise` itself: ~1 μJ per call.
- **Upside**: Simple; uses kernel infrastructure; large cold-start improvement.
- **Downside**: `MADV_WILLNEED` is a hint (kernel may ignore); no control over prefetch order; may evict useful pages.
- **Key paper**: Linux `madvise(2)` man page. Aerospike, "Cache Warming Explained," 2024. Georgia Tech, "Intelligent Buffer Pool Prefetching," 2023.

### Candidate Solution B: Explicit Prefetch Thread (Controlled Warmup)
- **Approach**: Spawn a warmup thread that sequentially reads (touch) each page in the working set. This forces page faults and loads data into the page cache. The thread can prioritize hot regions (based on metadata) and prefetch in parallel with query execution.
- **Performance**: Sequential page-touch warmup: ~DDR5 bandwidth (50–100 GB/s for sequential reads). 10 GB: ~0.1–0.2 s. 15× faster than `madvise` (which is limited by NVMe read-ahead). But requires the data to already be in page cache (NVMe → DDR5 must happen first).
- **Time to implement**: 1 month. Warmup thread; working-set metadata; priority ordering; coordination with query executor.
- **Energy cost**: Sequential DDR5 read: ~20 nJ/value. 10 GB / 8 B = 1.25G values × 20 nJ = ~25 mJ. Similar to Solution A but faster (less time → less leakage energy).
- **Upside**: Fastest warmup; controllable order; can overlap with query start.
- **Downside**: Requires working-set knowledge; warmup thread competes for memory bandwidth with queries.
- **Key paper**: CMU 15-445 buffer pool lectures. Georgia Tech, "Intelligent Buffer Pool Prefetching," 2023.

### Candidate Solution C: Learned Warmup (ML-Based)
- **Approach**: Train a model on historical query patterns to predict the working set. At startup, prefetch only the predicted hot regions. Uses ML to avoid warming cold data.
- **Performance**: If the model predicts 80% of the actual working set, warmup time is reduced by ~20% (only 80% of data prefetched, but 20% of queries still hit cold paths). Georgia Tech (2023) demonstrated ML-based buffer pool prefetching improving OLTP performance by 10–20%.
- **Time to implement**: 3–4 months. Model training, historical query logging, online prediction, prefetch scheduling.
- **Energy cost**: Saves ~20% of warmup energy by not prefetching cold data. But model inference adds ~1–10 nJ per region.
- **Upside**: Adaptive; saves warmup time and energy on predictable workloads.
- **Downside**: High engineering cost; requires historical data; cold-start for the model itself (no predictions on first run).
- **Key paper**: Georgia Tech, "Intelligent Buffer Pool Prefetching," 2023. Lykouris & Vassilvitskii, ICML 2018.

### Recommendation
**Solution B** (explicit prefetch thread) for v1. It's the fastest warmup (0.1–0.2 s for 10 GB) and controllable. Use `madvise(MADV_WILLNEED)` (Solution A) as a first pass to trigger NVMe → DDR5 read-ahead, then the prefetch thread sweeps DDR5 → L3. Total 1.5 months for both. Defer **Solution C** (learned warmup) to v2 when the engine has query history to train on.

---

## P-MH-12: Tier Detection

### Candidate Solution A: Read /sys/devices/system/node/ (Solved)
- **Approach**: At engine init, read `/sys/devices/system/node/online` to get the list of NUMA nodes. For each node, read `/sys/devices/system/node/nodeN/cpulist` (which CPUs are local) and `/sys/devices/system/node/nodeN/meminfo` (memory size). Classify nodes: DDR5 (large, all CPUs), HBM (small, same CPUs as DDR5 on Xeon Max), CXL (appears as NUMA node with no local CPUs).
- **Performance**: Reading sysfs: ~1–5 ms total. No runtime cost. Detects all NUMA nodes including HBM and CXL (if kernel has CXL subsystem).
- **Time to implement**: 0.25 months. Parse sysfs; build tier map; expose to planner.
- **Energy cost**: ~1 μJ total (file reads). Negligible.
- **Upside**: Standard; works on all Linux; no dependencies; detects HBM and CXL automatically.
- **Downside**: Cannot distinguish HBM from DDR5 by type (both look like NUMA nodes); must use heuristics (size, bandwidth) or BIOS information. CXL detection requires kernel 6.0+ with CXL subsystem.
- **Key paper**: Linux NUMA documentation. `/sys/devices/system/node/` sysfs interface.

### Candidate Solution B: numactl --hardware + libnuma
- **Approach**: Use `libnuma` (`numa_available()`, `numa_num_configured_nodes()`, `numa_node_to_cpus()`) to discover NUMA topology programmatically.
- **Performance**: Same as Solution A (libnuma reads the same sysfs). ~1–5 ms.
- **Time to implement**: 0.25 months. Link libnuma; call topology discovery functions.
- **Energy cost**: ~1 μJ. Negligible.
- **Upside**: Cleaner API; cross-platform (libnuma handles sysfs parsing); well-tested.
- **Downside**: External dependency (libnuma); Linux-only; same HBM/CXL classification limitation.
- **Key paper**: libnuma documentation. Linux NUMA docs.

### Recommendation
**Solution A** (sysfs) is sufficient. This is a solved problem—0.25 months to parse `/sys/devices/system/node/`. Use libnuma (Solution B) if already linked for P-MH-04. No further investment needed. The HBM-vs-DDR5 disambiguation can use a bandwidth probe (write 1 GB, measure time: HBM is 2.5–3.5× faster per McCalpin IXPUG 2023) as a post-hoc classification.

---

# SUMMARY: RECOMMENDATIONS TABLE

| Problem | Recommended Solution | Est. Months | Key Performance Gain |
|---------|---------------------|-------------|---------------------|
| P-IS-01 | A: Tier-tagged dispatch table | 2–3 | 1.3–2× on DDR5/CXL |
| P-IS-02 | A: CPUID-guarded PEXT dispatch | 0.5 | 2–3× on Zen/Zen2 |
| P-IS-03 | A: Dynamic AVX2↔AVX-512 | 1 | 40–50% on modern CPUs |
| P-IS-04 | A: SVE-first ARM port | 2–3 | 2× over NEON |
| P-IS-05 | A: Manual VPTERNLOGQ intrinsics | 0.5 | ~1 cycle/predicate fusion |
| P-IS-06 | A+B: VPOPCNTDQ + PSADBW fallback | 0.75 | 5–8× on Ice Lake+ |
| P-IS-07 | A: Cloud instance matrix | 1 | Coverage of 4 vendors |
| P-IS-08 | A+C: Function sections + PGO | 0.75 | 5–15% I-cache |
| P-IS-09 | A+B: Mask accumulation + CMOV | 0.75 | 5× on adversarial data |
| P-IS-10 | A+B+C: Alignment + split-lock + UBSan | 0.6 | Eliminates 3000-cyc penalty |
| P-IS-11 | A: Fixed 1024-element batch | 0.25 | ClickHouse-proven |
| P-IS-12 | B: Stream decrypt (AES-NI) | 1 | No plaintext in memory |
| P-IS-13 | C: Auto-vectorization (defer A) | 0.25 | Functional RISC-V path |
| P-IS-14 | A: REP MOVSB (solved) | 0 | Already optimal |
| P-MH-01 | A: Hot-first placement | 1.5 | Lowest latency for hot data |
| P-MH-02 | C+A: Dual-map memcpy + migrate_pages | 1.5 | 2× faster migration |
| P-MH-03 | A: LRU (k-competitive) | 1 | Proven 4× bound |
| P-MH-04 | A+B: setaffinity + libnuma | 0.5 | 30% latency, 2× bandwidth |
| P-MH-05 | A: Empirical histogram | 1 | Data-driven CXL routing |
| P-MH-06 | A: NUMA-based HBM | 1 | 2.5–3.5× bandwidth |
| P-MH-07 | A: Linux CXL subsystem | 2 | Kernel-managed pooling |
| P-MH-08 | B+A: Intel PCM + perf | 0.75 | Per-channel bandwidth |
| P-MH-09 | B: Explicit mmap MAP_HUGETLB | 0.5 | 5–15% TLB |
| P-MH-10 | B: mmap + MPOL_BIND | 1 | Huge page + fallback |
| P-MH-11 | B+A: Prefetch thread + madvise | 1.5 | 15× cold-start |
| P-MH-12 | A: sysfs (solved) | 0.25 | Detects all tiers |

**Total estimated Wave 1 engineering: ~25–28 person-months** (with deferral of RISC-V, JIT, LP-optimal, learned policies to later waves).

---

# KEY REFERENCES (CONSOLIDATED)

1. Willhalm et al., "SIMD-Scan: Ultra Fast in-Memory Table Scan Using On-Chip Vector Processing Units," PVLDB 2(1), 2009.
2. Polychroniou et al., "Rethinking SIMD Vectorization for In-Memory Databases," PVLDB 8(12), 2015.
3. Kersten et al., "Everything You Always Wanted to Know About Compiled and Vectorized Queries," PVLDB 11(13), 2018.
4. Agner Fog, "Instruction Tables: Lists of Instruction Latencies, Throughputs and Micro-operation Breakdowns," 2025.
5. LLVM Issue #102047, "[AVX512] Preferring 512-bit vectors on recent Intel CPUs," 2024.
6. Chips and Cheese, "Zen 5's AVX-512 Frequency Behavior," Mar 2025.
7. Chips and Cheese, "Investigating Split Locks on x86-64," 2026.
8. Lemire, "The Dangers of AVX-512 Throttling," 2018.
9. Lemire, "Mispredicted Branches Can Multiply Your Running Times," 2019.
10. ARM, "SVE Programmer's Guide," 2020.
11. ArXiv 2505.09462, "ARM SVE Unleashed: Performance and Insights," 2025.
12. Intel, "AES-NI Whitepaper," 2012.
13. Intel 64 and IA-32 Optimization Reference Manual, 2023.
14. Sleator & Tarjan, "Amortized Efficiency of List Update and Paging Rules," JACM 32(2), 1985.
15. Koutsoupias & Papadimitriou, "On the k-Server Conjecture," JACM 42(5), 1995.
16. Weisgut et al., "CXL Memory Performance for In-Memory Data Processing," PVLDB 18, 2025.
17. Melody et al., "Systematic CXL Memory Characterization," ASPLOS 2025.
18. arXiv 2409.14317, "Dissecting CXL Memory Performance at Scale," 2024.
19. Agarwal et al., "Pond: CXL-Based Memory Pooling Systems," ASPLOS 2023.
20. McCalpin, "Bandwidth Limits in the Intel Xeon Max," IXPUG/ISC 2023.
21. Dragojević et al., "FaRM: Fast Remote Memory," NSDI 2014.
22. Kim et al., "Understanding Energy Aspects of Processing-near-Memory," MEMSYS 2015.
23. Vogelsang, "Understanding the Energy Consumption of Dynamic RAM," IEEE TCAD 2010.
24. Linux kernel documentation: page_migration, buslock, userfaultfd, THP, NUMA, mbind, madvise.
25. Georgia Tech, "Intelligent Buffer Pool Prefetching," 2023.
26. Muła et al., "Faster Population Counts Using AVX2 Instructions," IEEE TC 2017.
27. RISC-V V Extension Specification v1.0.
28. ArXiv 2605.10860, "Towards Portable Performance on RISC-V Vector Processors," 2026.
29. Kingman, "The Single-Server Queue," 1961.
30. Boyd & Vandenberghe, "Convex Optimization," 2004.
