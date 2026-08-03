# ADR-019: DPccp for n≤15 joins, IDP for n>15

## Status
Accepted

> **⚠️ Implementation note (Wave 54):** DPccp is **NOT** wired to the SQL
> executor. `src/planner/dpccp.rs` exists as a research prototype with unit
> tests, but `QueryEngine::execute()` does not call it. The actual join
> ordering uses a simple heuristic in `src/planner/optimizer.rs` that picks
> between KernelDirect, Vectorized, HashJoin, and TpchFallback strategies.
> Similarly, `src/planner/mcts.rs` (MCTS plan search) and
> `src/planner/learned.rs` (learned cardinality) are not wired to the
> executor.

## Confidence
85%

## Context

Join ordering is NP-hard. The standard approach is dynamic programming (Selinger 1979), which is O(3^n) — feasible for n ≤ 15 joins. Beyond that, we need heuristics or approximations.

DPccp (Moerkotte & Neumann 2008) is the modern standard: it avoids generating cross products during DP, reducing the constant factor by ~2× vs Selinger's original.

For n > 15, IDP (Iterative Dynamic Programming, Neumann 2009) partitions the problem into blocks of k ≤ 8, solves each block with DP, and combines the results.

## Decision

**Use a three-tier join ordering strategy:**
- **n ≤ 15 joins**: DPccp (exact, O(3^n))
- **16 ≤ n ≤ 40 joins**: IDP with block size k=8
- **n > 40 joins**: greedy GOO (Greedy Operator Ordering) fallback

The cost model (from the kernel table + Kingman latency predictor) provides per-join cost estimates.

## Consequences

### Positive
- **Optimal for n ≤ 15**: DPccp finds the best join tree
- **Near-optimal for n ≤ 40**: IDP is within 5–15% of optimal (Neumann 2009)
- **Bounded for n > 40**: GOO is 1.5–4× worse than optimal but always terminates
- TPC-H max is 8 joins → DPccp alone suffices for the benchmark

### Negative
- DPccp is O(3^n) — at n=15, that's ~14M plans (feasible but ~100 ms planning time)
- IDP's quality depends on the partitioning heuristic
- GOO is bad on cyclic schemas (TPC-H Q9 has a cycle)

## Alternatives considered

1. **Selinger DP only** — 2× slower than DPccp due to cross-product generation. Rejected.
2. **Always IDP** — suboptimal for small n where exact DP is cheap. Rejected.
3. **Learned ordering (Neo)** — could outperform but requires training data. Deferred to research.
4. **LP relaxation** — for n > 15, could give better approximation than IDP. Deferred (see OPEN_QUESTIONS.md).

## Compatibility

- Compatible with ADR-016 (submodular index selection): the cost model is shared
- Compatible with ADR-018 (morsel executor): the join plan is executed morsel-by-morsel
- Compatible with ADR-003 (CPUID dispatch): join cost estimates use the kernel table's throughput numbers

## References
- Moerkotte & Neumann, "Dynamic Programming Strikes Back" SIGMOD 2008
- Neumann, "Query Simplification" PVLDB 2009
- Selinger, "Access Path Selection" 1979
- Swami & Gupta, "Optimization of Large Join Queries" 1988 (GOO)
