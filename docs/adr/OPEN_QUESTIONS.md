# Open Questions — Decisions Below 80% Confidence

> Decisions that need more research, prototyping, or debate before we can
> commit to an ADR. Each entry has a target confidence level and a plan for
> reaching it.

---

## OQ-01: The cost model — calibrated analytic, learned, or hybrid?

**Confidence**: 40% — the most important undecided question.

**Context**: The planner needs a cost model to predict query latency from (data size, tier, kernel, CPU). This is the keystone — 6 other ADRs defer to it (ADR-019 join ordering, ADR-016 index selection, ADR-020 admission control, etc.).

**Candidate approaches**:
1. **Calibrated analytic** (Kingman + AVX-512 throughput) — interpretable, ~30% error
2. **Learned** (Neo-style, gradient-boosted model) — 10–30% better when trained, but cold-start
3. **Hybrid** (analytic base + learned residual) — best of both, but complex

**Why it's undecided**: All three approaches have shown promise in the literature, but none has been validated on our specific architecture (tier-aware kernels). The 30% error target for the analytic model is unproven.

**Plan to reach 80%**:
1. Build a minimal calibrated analytic model (2 months)
2. Benchmark it on TPC-H — measure actual error
3. If error > 30%, add a learned residual (2 more months)
4. Re-benchmark; if error < 20%, write the ADR

**Related ADRs**: ADR-019 (join ordering), ADR-016 (index selection), ADR-020 (admission control)

---

## OQ-02: CXL commit mechanism — CXL.mem shared record vs CXL.cache flush

**Confidence**: 50%

**Context**: Single-rack transactions commit via CXL coherence. Two mechanisms:
1. **CXL.mem shared commit record**: write a commit record to a CXL-attached shared memory region using `cmpxchg16b` (atomic 16-byte compare-and-swap). ~200–500 ns per commit.
2. **CXL.cache flush + MFENCE**: use CXL.cache to flush modified cache lines, then issue a memory fence. Similar latency but different semantics.

**Why it's undecided**: We don't have CXL hardware to test on. The latency estimates are from the literature (Das Sharma 2024, Ruijie 2024) and may not match real devices.

**Plan to reach 80%**:
1. Get access to a CXL 2.0/3.0 test machine (cloud or vendor loan)
2. Benchmark both mechanisms on real CXL hardware
3. Pick the one with lower p99 latency
4. Write the ADR

**Related ADRs**: ADR-013 (linear-typed handles), ADR-014 (HLC clocks)

---

## OQ-03: Column compression — rANS, tANS, or zstd?

**Confidence**: 55%

**Context**: Column compression (P-03-03) needs to be entropy-optimal AND SIMD-decodable. The wave research (W2) recommends interleaved rANS, but:
1. **rANS**: 11 GB/s decode, entropy-optimal, but complex to implement correctly
2. **tANS** (Zstd-style): 8 GB/s decode, simpler, proven in production
3. **zstd**: 3 GB/s decode, off-the-shelf, but too slow for the kernel table

**Why it's undecided**: rANS is theoretically best, but the implementation complexity is high (bitstream refill logic in SIMD is tricky). tANS is the safe choice but slightly slower.

**Plan to reach 80%**:
1. Prototype a minimal rANS kernel with AVX-512 `VPGATHERDD` (2 weeks)
2. Benchmark on real column data (TPC-H lineitem, JSON logs)
3. If decode throughput > 8 GB/s and compression ratio > 1.5× over zstd, pick rANS
4. Otherwise, fall back to tANS and write the ADR

**Related ADRs**: ADR-001 (64-bit word), ADR-002 (page format)

---

## OQ-04: Raft implementation — openraft, custom, or fork?

**Confidence**: 60%

**Context**: Cross-rack transactions use Raft over RoCEv2. Options:
1. **openraft** (Rust crate): mature, well-tested, but doesn't use RDMA
2. **Custom RDMA Raft**: best performance (FaRM-style), but 8+ months to build
3. **Fork openraft + add RDMA transport**: middle ground, 4–5 months

**Why it's undecided**: openraft is safe but may be too slow (it uses TCP, not RDMA). Custom is fast but risky. The fork is the sweet spot but depends on openraft's architecture being amenable to transport swapping.

**Plan to reach 80%**:
1. Evaluate openraft's transport abstraction (1 week)
2. If it's pluggable, prototype an RDMA transport (2 months)
3. Benchmark cross-rack commit latency
4. If < 15 µs p99, write the ADR for fork-openraft
5. If > 15 µs, start a custom implementation

**Related ADRs**: ADR-014 (HLC clocks), ADR-011 (ZNS WAL)

---

## OQ-05: Schema migration — functorial, SQL DDL, or hybrid?

**Confidence**: 35%

**Context**: Schema evolution (P-05-09) could use:
1. **Functorial data migration** (Spivak's Σ ⊣ Δ ⊣ Π): mathematically principled, but the CQL implementation is slow and unproven at scale
2. **SQL DDL + data copy**: standard, fast, but no correctness guarantees
3. **Hybrid**: use functorial for correctness-critical migrations, SQL DDL for routine ones

**Why it's undecided**: Functorial migration is beautiful math but has never been deployed in a production database. The risk is high; the benefit (correctness proofs) may not justify the engineering cost.

**Plan to reach 80%**:
1. Implement a minimal functorial migration for a simple schema change (add column) (2 months)
2. Benchmark against SQL DDL
3. If the functorial approach is < 2× slower and provably correct, write the ADR
4. Otherwise, use SQL DDL and defer functorial to research

**Related ADRs**: None (schema is the last layer)

---

## OQ-06: (ε,δ) propagation through joins — McDiarmid, union bound, or Bayesian?

**Confidence**: 55%

**Context**: ADR-015 uses empirical Bernstein for single-operator approximate queries. But when operators compose (e.g., approximate scan → approximate join → approximate aggregate), the errors propagate. The wave research (W3) recommends McDiarmid, but:
1. **Union bound**: δ_total = δ₁ + δ₂ + ... — loose, δ grows linearly
2. **McDiarmid**: tighter, selectivity-aware, but requires bounded-differences analysis per operator
3. **Bayesian**: tightest, but requires a prior and is harder to reason about

**Why it's undecided**: McDiarmid is theoretically better but requires per-operator analysis that we haven't done. Union bound is safe but may force unnecessarily large samples.

**Plan to reach 80%**:
1. Derive McDiarmid bounds for the 3 most common join patterns (equi-join, range-join, semi-join) (2 months)
2. Compare sample sizes vs union bound on TPC-H joins
3. If McDiarmid saves > 30% samples, write the ADR
4. Otherwise, use union bound (simpler)

**Related ADRs**: ADR-015 (empirical Bernstein), ADR-017 (similarity search)

---

## OQ-07: ARM port — SVE, NEON, or both?

**Confidence**: 50%

**Context**: ADR-003 handles x86 (AVX-512, AVX2, scalar). ARM needs a separate kernel set. Options:
1. **SVE-first** (variable-length vectors): future-proof, best on Graviton 4 / Neoverse V2, but SVE is new and less tested
2. **NEON-first** (fixed 128-bit): simpler, works on all ARM64, but 2× slower than SVE on SVE-capable hardware
3. **Both**: SVE for SVE-capable CPUs, NEON fallback — more engineering

**Why it's undecided**: We don't have ARM hardware to benchmark on. The SVE vs NEON performance gap is estimated at 2× but unverified for our kernels.

**Plan to reach 80%**:
1. Get access to a Graviton 4 instance (SVE2) and a Graviton 3 instance (NEON only) (1 week)
2. Port `scan_eq` and `sum_f64` to both SVE and NEON (1 month)
3. Benchmark the gap
4. If SVE is > 1.5× faster than NEON, write the ADR for SVE-first with NEON fallback
5. Otherwise, NEON-only (simpler)

**Related ADRs**: ADR-003 (CPUID dispatch), ADR-007 (batch size)

---

## OQ-08: Hash join spill target — CXL, NVMe, or in-RAM only?

**Confidence**: 45%

**Context**: When a hash table doesn't fit in DDR5, it must spill. Options:
1. **CXL spill** (ADR P-07-07): 20–200× faster than NVMe, but CXL is not universally available
2. **NVMe spill** (FOEDUS-style): works everywhere, but 10–50× slower
3. **In-RAM only** (scale-up): buy more RAM, avoid spilling entirely

**Why it's undecided**: CXL spill is the flagship (showcases the tier-aware architecture), but it requires CXL hardware. If we can't get CXL devices, the whole feature is moot.

**Plan to reach 80%**:
1. Get a CXL 2.0 test machine (same as OQ-02)
2. Benchmark CXL spill vs NVMe spill on a 100 GB hash join (2 months)
3. If CXL is > 20× faster, write the ADR for CXL spill with NVMe fallback
4. If CXL hardware is unavailable, defer this ADR indefinitely

**Related ADRs**: ADR-010 (LRU migration), ADR-018 (morsel executor)

---

## OQ-09: Trace JIT — Cranelift, LLVM ORC, or hand-written asm?

**Confidence**: 50%

**Context**: The trace JIT (P-07-04) compiles hot monomorphic traces to native code. Options:
1. **Cranelift**: fast compile (~10–50 µs), within 10–25% of LLVM -O2, good for short-lived traces
2. **LLVM ORC**: best code quality, but 10–100× slower compile — only worth it for long-lived traces
3. **Hand-written asm templates**: fastest compile (ns), but inflexible

**Why it's undecided**: Cranelift is the safe choice, but its AVX-512 code generation is immature. LLVM ORC is proven (HyPer uses it) but adds a 100 MB dependency and slow compile.

**Plan to reach 80%**:
1. Prototype a Cranelift-based trace JIT for `scan_eq` (1 month)
2. Benchmark the compiled code vs the hand-written kernel
3. If within 15% of hand-written, use Cranelift
4. If > 15% gap, evaluate LLVM ORC or hand-written asm for the hottest 5 kernels

**Related ADRs**: ADR-003 (CPUID dispatch), ADR-018 (morsel executor)

---

## OQ-10: Distributed transaction protocol — 2PC, Calvin, or saga?

**Confidence**: 40%

**Context**: Cross-rack transactions (P-04-05) need a protocol. Options:
1. **2PC** (Two-Phase Commit): standard, blocking, ~50 µs commit
2. **Calvin** (deterministic): fast, requires pre-ordering, no aborts for CC
3. **Saga**: eventual consistency, not suitable for OLTP

**Why it's undecided**: Calvin is theoretically better (no 2PC overhead) but requires deterministic transaction ordering, which constrains the application model. 2PC is safe but slow. We need to understand the target workload's tolerance for Calvin's constraints.

**Plan to reach 80%**:
1. Prototype Calvin-style deterministic ordering for TPC-C (2 months)
2. Benchmark against 2PC
3. If Calvin is > 2× faster and the constraints are acceptable, write the ADR
4. Otherwise, use 2PC with CXL optimization

**Related ADRs**: ADR-014 (HLC clocks), ADR-020 (admission control)

---

## Summary

| # | Question | Confidence | Blocker | Plan |
|---|----------|-----------|---------|------|
| OQ-01 | Cost model | 40% | No validation data | Build minimal model, benchmark on TPC-H |
| OQ-02 | CXL commit mechanism | 50% | No CXL hardware | Get CXL test machine, benchmark |
| OQ-03 | Column compression | 55% | rANS complexity | Prototype rANS kernel, benchmark |
| OQ-04 | Raft implementation | 60% | openraft transport eval | Evaluate openraft, prototype RDMA |
| OQ-05 | Schema migration | 35% | Functorial unproven at scale | Prototype minimal functorial migration |
| OQ-06 | (ε,δ) propagation | 55% | Per-operator McDiarmid analysis | Derive bounds for 3 join patterns |
| OQ-07 | ARM port | 50% | No ARM hardware | Get Graviton 4, port 2 kernels |
| OQ-08 | Hash join spill | 45% | CXL hardware dependency | Same as OQ-02 |
| OQ-09 | Trace JIT backend | 50% | Cranelift AVX-512 maturity | Prototype, benchmark vs hand-written |
| OQ-10 | Distributed TX protocol | 40% | Calvin constraints unclear | Prototype Calvin for TPC-C |

**The critical path**: OQ-01 (cost model) unblocks the most ADRs. OQ-02 (CXL commit) and OQ-08 (CXL spill) are blocked on hardware access. The rest can proceed in parallel once the cost model is settled.

**When these reach 80% confidence, we write the remaining ADRs and then produce the true fine draft.**
