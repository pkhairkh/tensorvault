# ADR-018: Data-centric morsel-driven pipeline execution

## Status
Accepted

> **⚠️ Implementation note (Wave 54):** The morsel-driven executor described
> in this ADR is **NOT** used by the SQL executor. `src/executor/morsel.rs`
> exists as a research prototype but is not wired to `QueryEngine::execute()`.
> The actual execution path uses **dispatch-based** kernel selection
> (`src/engine/dispatch.rs`) followed by vectorized kernels
> (`src/exec/vectorized.rs`). See ARCHITECTURE.md "The executor" section for
> the real execution flow.

## Confidence
90%

## Context

The executor must:
1. Keep intermediate data in L1/L2 (avoid DRAM round-trips)
2. Scale linearly to 64+ cores without lock contention
3. Respect NUMA boundaries (ADR-008)

The Volcano pull model (iterator `next()`) is 5–10× slower than push-based because each `next()` call has function-call overhead and prevents fusion. HyPer (Neumann 2014) showed push-based compilation is the answer, but full code generation is complex.

The **morsel-driven** model (Leis 2014) is the sweet spot: push-based, data-centric, but without full code generation — it uses pre-compiled kernels (our kernel table).

## Decision

**Use data-centric morsel-driven pipeline execution.**

- A **morsel** = 1024 cells (ADR-007) = one batch
- The scheduler dispatches morsels to worker threads (ADR-008, NUMA-pinned)
- Each worker runs the full pipeline (scan → filter → aggregate) on one morsel, keeping everything in L1/L2
- No intermediate materialization — data flows through registers and L1

```
Morsel dispatcher → Worker 0 (core 0, NUMA 0): [scan→filter→agg] on morsel 0
                  → Worker 1 (core 1, NUMA 0): [scan→filter→agg] on morsel 1
                  → Worker 2 (core 2, NUMA 1): [scan→filter→agg] on morsel 2
                  → ...
```

For operators that can't pipeline (e.g., hash join build), the pipeline is broken at the "pipeline breaker" and the build side is materialized to DRAM.

## Consequences

### Positive
- **5–10× faster than Volcano pull** (Neumann 2014, Boncz 2005)
- **Near-linear scaling to 64+ cores** (Leis 2014) — morsels are independent
- **L1/L2 resident** — intermediate data never hits DRAM (for pipelineable operators)
- **NUMA-aware** — morsels are dispatched to workers on the data's NUMA node
- Uses the kernel table (ADR-003) — no code generation needed for v1

### Negative
- Pipeline breakers (hash join build, sort) materialize to DRAM — can't avoid this
- Morsel size must be tuned to fit in L1 (1024 cells × 8 bytes = 8 KB, fits in 32 KB L1)
- More complex than Volcano (scheduling, pipeline boundaries)

## Alternatives considered

1. **Volcano pull model** — 5–10× slower. Rejected.
2. **HyPer-style push + full codegen (LLVM)** — best performance but requires LLVM dependency and compile-time overhead. Deferred to future ADR (trace JIT).
3. **Vectorized (X100 style)** — similar to morsel but without NUMA-awareness. Subsumed by morsel-driven.

## Compatibility

- Compatible with ADR-001 (64-bit word): morsels are arrays of u64
- Compatible with ADR-007 (1024 batch): morsel = batch = 1024 cells
- Compatible with ADR-008 (NUMA pinning): workers are pinned, morsels are NUMA-local
- Compatible with ADR-010 (LRU migration): morsel access updates the region's LRU position
- Compatible with ADR-003 (CPUID dispatch): each morsel picks the kernel for its tier

## References
- Leis et al., "Morsel-Driven Parallelism" SIGMOD 2014
- Neumann, "Efficiently Compiling Efficient Query Plans" PVLDB 2014
- Boncz et al., "MonetDB/X100" CIDR 2005
- `src/executor/scheduler.rs` (current implementation — to be upgraded to morsel-driven)
