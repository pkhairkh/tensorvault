# ADR-008: NUMA-aware thread pinning via pthread_setaffinity_np

## Status
Accepted

## Confidence
90%

## Context

Cross-socket memory access is 1.5–2× local latency and 2–4× energy (see `cpu-energy-kb.md` §5.6). Without thread pinning, the OS scheduler may move a worker thread to a different NUMA node, causing every subsequent memory access to cross the socket.

The data-centric morsel executor (ADR-018) requires NUMA-local data access to keep morsels in L2/L3.

## Decision

**Pin each worker thread to a specific physical core via `pthread_setaffinity_np` (Linux) or `thread_policy_set` (macOS).** The pinning policy:
- One worker thread per physical core (SMT off for v1; revisit with SMT benchmarks)
- Worker thread N is pinned to NUMA node `N / cores_per_node`
- Morsels are dispatched to workers based on the data's NUMA node

## Consequences

### Positive
- Eliminates cross-socket memory access on the hot path
- L2/L3 hit rate improves (data stays local to the worker)
- Predictable tail latency (no scheduler-induced migrations)

### Negative
- Reduces total core utilization (SMT off means half the logical cores unused)
- Requires explicit NUMA topology detection at startup
- May conflict with container orchestration (Kubernetes CPU limits) — mitigated by reading `sched_getaffinity` at startup

## Alternatives considered

1. **No pinning, rely on Linux `autoNUMA`** — too slow for OLTP (migration takes ms). Rejected.
2. **SMT on, one worker per logical core** — 30–40% more throughput but unpredictable tail latency. Deferred to v2 with benchmarks.
3. **`numactl --cpunodebind`** — process-level, not thread-level. Insufficient for the morsel executor.

## References
- `docs/architecture/cpu-energy-kb.md` §5.6 (cross-NUMA latency/energy)
- AMD, "Optimizing EPYC Memory with NUMA" whitepaper
- Leis et al., "Morsel-Driven Parallelism" SIGMOD 2014
