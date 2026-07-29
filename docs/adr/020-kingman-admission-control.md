# ADR-020: Kingman ρ-guard + token bucket for admission control

## Status
Accepted

## Confidence
80%

## Context

Under high load, the executor must decide which queries to admit and which to queue. If utilization (ρ) approaches 1, latency explodes (Kingman's formula: W ≈ ρ/(1-ρ) × ...). We need admission control that:
1. Prevents the system from thrashing (ρ < 0.8)
2. Provides predictable tail latency
3. Is simple to implement and reason about

## Decision

**Use a two-layer admission control:**
1. **Kingman ρ-guard** (outer): reject new queries if predicted ρ > 0.8
   - ρ = λ / μ, where λ = arrival rate, μ = service rate
   - If ρ > 0.8, the predicted p99 latency is > 5× the unloaded latency (Kingman)
2. **Token bucket** (inner): smooth out bursts
   - Bucket capacity = 2 × max concurrent queries
   - Refill rate = 0.7 × μ (leave 30% headroom)

If a query is rejected by the ρ-guard, it gets an HTTP 503 (retry after N ms).
If a query is throttled by the token bucket, it waits in a queue.

## Consequences

### Positive
- **Predictable p99**: with ρ < 0.8, p99 < 2× the unloaded latency (Kingman)
- **Simple**: two parameters (ρ threshold, bucket size)
- **Composable**: works with the morsel executor (ADR-018) — each query gets a morsel budget
- **Prevents thrash**: rejecting queries is better than letting all queries slow down

### Negative
- Rejects queries at 80% utilization — wastes 20% of capacity
- Kingman assumes G/G/1 queueing; real workloads may be burstier
- Token bucket is static; doesn't adapt to workload changes (future: learned rates)

## Alternatives considered

1. **No admission control** — system thrashes at high load. Rejected.
2. **ML-based admission** — adaptively learns the optimal threshold. Potentially better but no guarantee. Deferred to research.
3. **Strict priority queueing** — starves low-priority queries. Rejected for fairness.
4. **Fair scheduling (CFS)** — good for fairness but doesn't prevent thrash. Used within the executor (ADR-018), not for admission.

## Compatibility

- Compatible with ADR-018 (morsel executor): each admitted query gets a morsel budget
- Compatible with ADR-011 (ZNS WAL): predictable fsync latency makes μ stable
- Compatible with ADR-014 (HLC): timestamps order the admission queue

## References
- Kingman, "The Single Server Queue in Heavy Traffic" 1961
- Kleinrock, "Queueing Systems" Vol. 1 1975
- Schroeder & Harchol-Balter, "Web Servers Under Overload" ACM TOIT 2006
