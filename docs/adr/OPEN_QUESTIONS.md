# Open Questions — Decisions Below 80% Confidence

> Updated 2025-07-30. Three questions resolved (OQ-01, OQ-03, OQ-06) →
> ADR-023, ADR-024, ADR-025. Seven remain open.

---

## Resolved (upgraded to ADRs)

| OQ | Resolution | ADR | Confidence |
|----|-----------|-----|-----------|
| OQ-01 | Calibrated analytic cost model | [ADR-023](./023-calibrated-analytic-cost-model.md) | 85% (was 40%) |
| OQ-03 | rANS for cold-tier columns only | [ADR-025](./025-rans-cold-tier-only.md) | 80% (was 55%) |
| OQ-06 | McDiarmid bounded-differences for join propagation | [ADR-024](./024-mcdiarmid-eps-delta-joins.md) | 85% (was 55%) |

---

## Still open (below 80% confidence)

### OQ-02: CXL commit mechanism — CXL.mem shared record vs CXL.cache flush

**Confidence**: 50% (unchanged — blocked on hardware)

**Context**: Single-rack transactions commit via CXL coherence. Two mechanisms:
1. CXL.mem shared commit record (`cmpxchg16b`) — ~200–500 ns
2. CXL.cache flush + MFENCE — similar latency, different semantics

**Blocker**: No CXL hardware available. The test machine (Zen 5 VM) has no
CXL support.

**Plan**: Get CXL 2.0/3.0 test machine → benchmark both → write ADR.

---

### OQ-04: Raft implementation — openraft, custom, or fork?

**Confidence**: 60% (unchanged — needs code evaluation)

**Context**: Cross-rack transactions use Raft over RoCEv2. Options:
1. openraft (Rust crate) — mature but uses TCP
2. Custom RDMA Raft — best performance but 8+ months
3. Fork openraft + add RDMA transport — middle ground

**Blocker**: Need to evaluate openraft's transport abstraction.

**Plan**: `cargo doc --package openraft` → evaluate transport trait →
prototype RDMA transport if pluggable → write ADR.

---

### OQ-05: Schema migration — functorial, SQL DDL, or hybrid?

**Confidence**: 35% (unchanged — research risk)

**Context**: Functorial data migration (Spivak's Σ ⊣ Δ ⊣ Π) is beautiful
math but unproven at production scale.

**Blocker**: No existing production deployment of functorial migration.

**Plan**: Prototype minimal functorial migration for "add column" →
benchmark vs SQL DDL → write ADR if < 2× slower.

---

### OQ-07: ARM port — SVE, NEON, or both?

**Confidence**: 50% (unchanged — no ARM hardware)

**Context**: ARM kernels (SVE vs NEON) need benchmarking on Graviton 4.

**Blocker**: The test machine is x86-only (Zen 5).

**Plan**: Get AWS Graviton 4 instance → port 2 kernels → benchmark → write ADR.

---

### OQ-08: Hash join spill target — CXL, NVMe, or in-RAM only?

**Confidence**: 45% (unchanged — blocked on CXL hardware)

**Context**: CXL spill is the flagship but requires CXL hardware.

**Blocker**: Same as OQ-02 — no CXL hardware.

**Plan**: Get CXL machine → benchmark 100 GB hash join spill → write ADR.

---

### OQ-09: Trace JIT — Cranelift, LLVM ORC, or hand-written asm?

**Confidence**: 50% (unchanged — needs prototype)

**Context**: The trace JIT compiles hot monomorphic traces. Cranelift is
the safe choice but its AVX-512 codegen is immature.

**Blocker**: Need to prototype Cranelift JIT and benchmark vs hand-written.

**Plan**: Prototype Cranelift JIT for `scan_eq` → benchmark generated code
vs hand-written kernel → write ADR if within 15%.

---

### OQ-10: Distributed transaction protocol — 2PC, Calvin, or saga?

**Confidence**: 40% (unchanged — Calvin constraints unclear)

**Context**: Calvin is theoretically better but requires deterministic
transaction ordering, which constrains the application model.

**Blocker**: Need to understand target workload's tolerance for Calvin.

**Plan**: Prototype Calvin-style ordering for TPC-C → benchmark vs 2PC →
write ADR if > 2× faster.

---

## Summary

| # | Question | Confidence | Status | Blocker |
|---|----------|-----------|--------|---------|
| OQ-01 | Cost model | ✅ 85% | **Resolved → ADR-023** | — |
| OQ-02 | CXL commit mechanism | 50% | Open | No CXL hardware |
| OQ-03 | Column compression | ✅ 80% | **Resolved → ADR-025** | — |
| OQ-04 | Raft implementation | 60% | Open | openraft eval needed |
| OQ-05 | Schema migration | 35% | Open | Research risk |
| OQ-06 | (ε,δ) propagation | ✅ 85% | **Resolved → ADR-024** | — |
| OQ-07 | ARM port | 50% | Open | No ARM hardware |
| OQ-08 | Hash join spill | 45% | Open | No CXL hardware |
| OQ-09 | Trace JIT | 50% | Open | Cranelift prototype needed |
| OQ-10 | Distributed TX | 40% | Open | Calvin constraints unclear |

**3 of 10 resolved.** The resolved questions include the keystone (OQ-01 cost
model), which unblocks 6 other ADRs.

The remaining 7 are blocked on:
- **Hardware access** (3 questions): OQ-02, OQ-07, OQ-08 need CXL/ARM machines
- **Prototyping** (3 questions): OQ-04, OQ-09, OQ-10 need code evaluation
- **Research risk** (1 question): OQ-05 is high-risk, high-reward

**Next steps**: resolve OQ-04 (openraft evaluation) and OQ-09 (Cranelift
prototype) — both can be done without special hardware.
