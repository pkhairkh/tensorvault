# ADR-016: Greedy submodular maximization for index selection

## Status
Accepted

## Confidence
85%

## Context

Given a workload (set of queries) and a storage budget, which indexes should we build? This is NP-hard in general, but the objective (query speedup) is submodular — it exhibits diminishing returns.

The Nemhauser-Wolsey-Fisher theorem (1978) guarantees that the greedy algorithm achieves a (1 - 1/e) ≈ 0.632 approximation for submodular maximization under a cardinality constraint.

## Decision

**Use greedy submodular maximization for index selection.**

Algorithm:
1. Start with an empty index set S = {}
2. For each candidate index i not in S:
   a. Compute the marginal speedup: Δ(i | S) = speedup(S ∪ {i}) - speedup(S)
3. Pick the index with the highest marginal speedup per byte of storage
4. Add it to S if it fits the storage budget
5. Repeat until the budget is exhausted or no index provides positive marginal speedup

The speedup function is evaluated using the cost model (ADR-019, DPccp) on the workload.

## Consequences

### Positive
- **Proven (1-1/e) ≈ 63% guarantee**: the selected indexes are within 63% of the optimal set
- **Simple to implement**: greedy is O(k × n) where k = budget, n = candidates
- **Adapts to workload changes**: re-run periodically as the workload evolves
- **Composable with the cost model**: uses the same cost estimates as the query planner

### Negative
- 63% is not optimal — the LP relaxation could do better but is much more expensive
- Greedy doesn't handle interactions between indexes well (e.g., two indexes that are only useful together)
- Requires accurate workload statistics (query frequency, predicate selectivity)

## Alternatives considered

1. **Exhaustive search** — optimal but O(2^n), infeasible for > 20 candidate indexes. Rejected.
2. **LP relaxation + rounding** — potentially better approximation but the LP is large (one variable per candidate index per query). Deferred.
3. **Learned index selection (Neo-style)** — could outperform greedy but requires training data and has no guarantee. Deferred to research.

## Compatibility

- Compatible with ADR-019 (DPccp): the cost model used for index evaluation is the same one used for join ordering
- Compatible with ADR-010 (LRU migration): indexes are stored as regions and migrated by LRU
- Compatible with ADR-018 (morsel executor): indexes accelerate morsel-level lookups

## References
- Nemhauser, Wolsey & Fisher, "An analysis of approximations for maximizing submodular set functions" Math. Prog. 1978
- Krause & Guestrin, "Beyond Convexity: Submodularity in Machine Learning" 2008
- Chaudhuri & Narasayya, "AutoAdmin" SIGMOD 1997 (Microsoft's index selection tool)
