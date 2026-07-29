# Execution Problems

> Problems related to the executor: the scheduler of instruction streams that
> lowers logical plans to kernel DAGs and dispatches them respecting data
> dependencies and tier bandwidth.
>
> **Research source**: `docs/research/optimization_theory_db.md` (MDP, online
> optimization), `docs/research/spectral_db_research.md` (join ordering).

---

## P-07-01: Logical plan → kernel DAG lowering 🟡

**Layer**: Executor
**Status**: 🟡 partial (basic lowering exists in `src/executor/plan.rs`)
**Math**: IV (DP for join ordering)
**Effort**: M
**Impact**: high

### Problem

The current `lower_to_kernels()` function does a naive tree walk: for each
`PlanNode`, emit a `KernelInvocation`. It doesn't:
- Pick the optimal join order
- Choose between hash join vs leapfrog join vs nested loop
- Fuse predicates (see P-01-05)
- Consider tier placement when choosing kernels

### Open questions

- Should lowering be a single pass or multi-pass (logical → physical → kernel)?
- How do we integrate the cost model (P-05-03)?

### Success criteria

- A `Lowerer` that takes a logical plan + cost model and emits an optimal
  kernel DAG.
- The lowering considers join order, predicate fusion, and tier placement.

---

## P-07-02: Join ordering (Selinger DP + beyond) 🔴

**Layer**: Executor
**Status**: 🔴 open
**Math**: IV (DP — Selinger 1979; LP relaxation — P-05-13)
**Effort**: L
**Impact**: high

### Problem

For n-way joins, the join order dominates performance. Selinger DP is
O(3^n) — fine for n ≤ 15, too slow beyond.

The engine needs:
1. Selinger DP for n ≤ 15
2. Branch-and-bound for 15 < n ≤ 25 (see P-05-13)
3. Heuristics (greedy, GOO) for n > 25
4. AGM-fractional-cover for worst-case-optimal joins (see P-05-05)

### Open questions

- When should we use leapfrog join (worst-case optimal) vs hash join
  (average-case faster)?
- How do we handle cross-rack joins (where the join order affects network
  traffic)?

### Success criteria

- A `JoinOrderer` that picks the optimal algorithm per join.
- Benchmark: TPC-H queries with ≥ 5 joins, within 2× of optimal.

---

## P-07-03: Adaptive execution (MDP / RL) 🔴

**Layer**: Executor
**Status**: 🔴 open
**Math**: IV (MDP — Bellman equation; RL — Q-learning)
**Effort**: XL
**Impact**: high

### Problem

The executor should adapt at runtime: if a hash table doesn't fit in L3,
spill to DDR5; if a scan is slower than expected (CXL contention), switch
to a smaller batch size.

This is naturally an MDP:
- State: current plan, observed cardinalities, tier utilization
- Actions: switch algorithm, change batch size, migrate region
- Reward: -latency (or -energy)
- Transitions: determined by the data and hardware

### Open questions

- Can we use Q-learning to learn the optimal policy from query traces?
- How do we ensure safety (the adaptation never makes things worse)?

### Success criteria

- An `AdaptiveExecutor` that monitors execution and switches strategies.
- Benchmark: 2× improvement on queries with cardinality estimation errors.

---

## P-07-04: Trace JIT specialization 🔴

**Layer**: Executor
**Status**: 🔴 open
**Math**: none (engineering; related to TraceMonkey)
**Effort**: XL
**Impact**: high

### Problem

This is the trace JIT from the original architecture doc. The executor
records a trace of (kernel, tag distribution) per batch. When a batch is
monomorphic (all same type), the trace is compiled to specialized machine
code via Cranelift, with tag checks hoisted out.

### Open questions

- How many batches before we compile a trace? (Too few → overhead; too many
  → missed specialization.)
- How do we handle trace invalidation (when the tag distribution changes)?

### Success criteria

- A `TraceJit` that compiles monomorphic traces.
- Benchmark: 2–4× speedup on monomorphic batches.

---

## P-07-05: Multi-query scheduling 🔴

**Layer**: Executor
**Status**: 🔴 open
**Math**: IV (game theory — Nash equilibrium for fair scheduling)
**Effort**: L
**Impact**: medium

### Problem

Multiple concurrent queries compete for resources (CPU, memory bandwidth,
NVMe I/O). The scheduler must:
1. Share resources fairly (no query starves)
2. Coalesce shared scans (if two queries scan the same table, do it once)
3. Prioritize latency-sensitive queries over batch

### Open questions

- Should we use fair scheduling (CFS-like) or proportional-share?
- Can we use game theory to prove fairness (Nash equilibrium)?

### Success criteria

- A `MultiQueryScheduler` that coalesces shared scans.
- Benchmark: 2 concurrent queries on the same table at 1.5× the throughput
  of running them separately.

---

## P-07-06: Pipeline parallelism 🟡

**Layer**: Executor
**Status**: 🟡 partial (executor processes one kernel at a time)
**Math**: none
**Effort**: M
**Impact**: high

### Problem

The current executor runs kernels sequentially: scan → filter → aggregate.
Each kernel reads its input fully before producing output. This wastes
memory bandwidth (intermediate results spill to DRAM).

Pipeline parallelism: the scan produces 4 KB batches, the filter consumes
them immediately, the aggregate accumulates. Everything stays in L1/L2.

### Open questions

- How many stages can we pipeline before register pressure hurts?
- How do we handle operators with different batch sizes (e.g., scan = 504
  cells, aggregate = 1 cell)?

### Success criteria

- A `Pipeline` executor that runs scan → filter → aggregate in L1.
- Benchmark: 3-stage pipeline at 1.5× the throughput of sequential execution.

---

## P-07-07: Spill-to-CXL for large hash joins 🔴

**Layer**: Executor
**Status**: 🔴 open
**Math**: none
**Effort**: M
**Impact**: high

### Problem

When a hash table doesn't fit in L3 (typically > 32 MB), it spills to
DDR5. If it doesn't fit in DDR5 (typically > 512 GB), it spills to NVMe.

But NVMe is 1000× slower than DDR5. CXL is a middle ground (~250 ns vs
~90 ns for DDR5, but ~20 µs for NVMe). The executor should spill to CXL
before NVMe.

### Open questions

- How do we partition the hash table across L3 / DDR5 / CXL?
- How do we handle the probe side when the build side is split across tiers?

### Success criteria

- A `SpillingHashJoin` that spills to CXL when DDR5 is full.
- Benchmark: 1 TB hash join at 2× the throughput of NVMe-only spill.

---

## P-07-08: Workload management (concurrency control) 🔴

**Layer**: Executor
**Status**: 🔴 open
**Math**: III (queueing theory — Little's law, Kingman)
**Effort**: L
**Impact**: high

### Problem

Under high load, the executor must decide which queries to admit and which
to queue. This is a queueing problem:
- Arrival rate λ (queries/sec)
- Service rate μ (queries/sec per worker)
- Utilization ρ = λ/μ
- Little's law: L = λW (in-flight = arrival × wait)

If ρ ≥ 1, the queue grows unboundedly. The executor should:
1. Monitor λ and μ
2. Reject or throttle queries when ρ > 0.8
3. Use Kingman's formula (P-02-06) to predict wait times

### Open questions

- Should we use admission control (reject) or backpressure (slow down)?
- How do we handle mixed workloads (OLTP + OLAP)?

### Success criteria

- A `WorkloadManager` that monitors queue length and admits/rejects queries.
- Benchmark: at 90% utilization, p99 latency < 2× the unloaded latency.

---

## P-07-09: Result caching and materialized views 🔴

**Layer**: Executor
**Status**: 🔴 open
**Math**: IV (submodular — view selection)
**Effort**: L
**Impact**: medium

### Problem

Frequently-run queries should be cached. Materialized views (pre-computed
query results) are a generalization.

The engine needs:
1. A result cache (per-query, LRU)
2. A materialized view manager (incremental maintenance via differential
   dataflow — see `docs/research/optimization_theory_db.md` §13)
3. A view selector (which views to materialize, given a storage budget —
   submodular maximization, P-05-14)

### Open questions

- How do we detect when a cached result is stale?
- Can we use differential dataflow for incremental view maintenance?

### Success criteria

- A `ResultCache` with LRU eviction.
- A `ViewManager` that maintains views incrementally.
- Benchmark: 10× speedup on repeated queries.

---

## P-07-10: Distributed query execution 🔴

**Layer**: Executor
**Status**: 🔴 open
**Math**: II (spectral partitioning for data placement)
**Effort**: XL
**Impact**: high

### Problem

For queries that span multiple nodes (e.g., a join between a table on node
A and a table on node B), the executor must:
1. Decide where to run each operator (data locality vs compute locality)
2. Ship data between nodes (via RoCEv2 RDMA)
3. Handle failures (a node goes down mid-query)

### Open questions

- Should we use a "push" model (send data to the compute) or "pull" (fetch
  data to the compute)?
- How do we handle skew (one node has more data than others)?

### Success criteria

- A `DistributedExecutor` that runs multi-node queries.
- Benchmark: 2-node join at 1.5× the throughput of single-node.

---

## P-07-11: Code generation (Cranelift) 🔴

**Layer**: Executor
**Status**: 🔴 open
**Math**: none (engineering)
**Effort**: XL
**Impact**: high

### Problem

For hot queries, the executor should compile the kernel DAG to native code
via Cranelift, eliminating the dispatch overhead. This is the "push-based
data-centric JIT" from HyPer (Neumann 2011).

### Open questions

- When should we JIT vs interpret? (Threshold: query runs > N times?)
- How do we handle JIT'd code invalidation (when the data tier changes)?

### Success criteria

- A `CraneliftJit` that compiles hot query plans.
- Benchmark: 2× speedup on repeated queries vs interpreted.

---

## Summary

| # | Problem | Status | Math | Effort | Impact |
|---|---------|--------|------|--------|--------|
| 01 | Logical plan → kernel DAG lowering | 🟡 | IV | M | high |
| 02 | Join ordering (Selinger DP + beyond) | 🔴 | IV | L | high |
| 03 | Adaptive execution (MDP / RL) | 🔴 | IV | XL | high |
| 04 | Trace JIT specialization | 🔴 | — | XL | high |
| 05 | Multi-query scheduling | 🔴 | IV | L | medium |
| 06 | Pipeline parallelism | 🟡 | — | M | high |
| 07 | Spill-to-CXL for large hash joins | 🔴 | — | M | high |
| 08 | Workload management (concurrency) | 🔴 | III | L | high |
| 09 | Result caching and materialized views | 🔴 | IV | L | medium |
| 10 | Distributed query execution | 🔴 | II | XL | high |
| 11 | Code generation (Cranelift) | 🔴 | — | XL | high |
