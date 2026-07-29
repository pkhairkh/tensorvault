# Wave 4: Execution + Benchmarking — Literature-Grounded Solution Matrix

Engine context: "instruction-first, memory-centric" — kernel table of AVX-512 kernels, tier-aware memory (L1/L2/L3/DRAM/CXL/NVMe), and a protocol coordinator that lowers logical plans into a kernel DAG.

Evaluation axes:
- **Performance** — throughput / latency, with citation.
- **Time-to-implement** — engineering months (3 FTE baseline).
- **Energy cost** — joules/op or watts, with citation/measurement basis.

Energy basis notes (used throughout): local DRAM random access ≈120–140 ns at ~10–20 nJ/cache-line (DDR4/5); CXL.type-3 pooled memory random read ≈350–520 ns at ~2–3× DRAM joules (cite: *CXL Memory Performance for In-Memory Data Processing*, IEEE 2022; *A Case Against CXL Memory Pooling*, ATC 2023). Intel RAPL within ±5% for package/DRAM energy on Skylake-X (CPU 61.04 µJ/unit, DRAM 15.26 µJ/unit) — cite Hahnel et al. 2012; RAPL accuracy studies. Hash-probe on AVX-512: ~0.5–2 nJ/tuple in L1-resident working set.

---

# PART A — EXECUTION PROBLEMS (11)

## P-EX-01: Logical Plan → Kernel DAG Lowering

### Candidate Solution A: Single-pass greedy lowering
- **Approach**: One bottom-up walk of the logical tree, mapping each relational operator to the first matching AVX-512 kernel in the kernel table; no cost-based search.
- **Performance**: Within 10–20% of optimal for narrow SPJA plans; degrades >2× on star schemas because join fan-out is ignored (Selinger 1979 showed access-path choice alone gives 2–10× swings).
- **Time to implement**: ~1.5 months. It is essentially a dispatch table; no statistics required.
- **Energy cost**: Negligible planning energy (~µJ/plans); but naive plans waste execution energy — a 2× slower plan is ~2× joules/query.
- **Upside**: Ships immediately; lets the kernel table be exercised end-to-end.
- **Downside**: Bottleneck for the TPC-H "expected loss" — this is where the 1.2–1.5× gap to DuckDB is mostly born.
- **Key paper**: Selinger, "Access Path Selection in a Relational DBMS," SIGMOD 1979.

### Candidate Solution B: Multi-pass with cost model (Cascades-style)
- **Approach**: Logical→physical→kernel in two passes, with a cardinality model feeding a cost function; memo of equivalent plans; pick min-cost kernel DAG.
- **Performance**: Recovers most of the gap — Neumann reports HyPer plans within ~10% of hand-tuned (Neumann, "Query Optimization (Almost) for Free," PVLDB 2009); Leis et al. show cardinality estimation, not search, is the residual error.
- **Time to implement**: ~5–7 months (cost model + stats infrastructure + memo is the long pole).
- **Energy cost**: Planning ~1–10 mJ/plan (cheap); net query energy down ~30–50% vs Solution A because fewer wasted tuples.
- **Upside**: Directly attacks the DuckDB gap; statistics subsystem pays off across EX-02..EX-11.
- **Downside**: Cardinality estimation is the hard part — bad stats make a fancy optimizer worse than greedy.
- **Key paper**: Neumann, PVLDB 2009; Leis et al., "How Good Are Query Optimizers, Really?" VLDB 2015.

### Candidate Solution C: Rule-based optimizer (heuristic rewrite set)
- **Approach**: Pattern-rewrite rules (pushdown, predication, kernel-fusion, SIMD-width selection) applied to fixpoint, no search.
- **Performance**: Comparable to B on the 80% of plans that match a rule; misses the long tail. Neumann 2014 notes predicate pushdown + code fusion alone get ~70% of HyPer's gains.
- **Time to implement**: ~3 months; rules compose well with the kernel table.
- **Energy cost**: mJ-scale planning; ~20–30% query-energy reduction vs A.
- **Upside**: Deterministic, debuggable, plays to the "instruction-first" design (rules emit kernel ids directly).
- **Downside**: No global optimum; adding rules has diminishing returns and combinatorial interactions.
- **Key paper**: Neumann, "Efficiently Compiling Efficient Query Plans for Modern Hardware," PVLDB 2014.

### Recommendation
**B (multi-pass + cost model), bootstrapped by C.** Build the rule set (C) first as the physical→kernel layer (≈3 mo) so the engine runs; then layer the cost model + memo (B) over 4 more months. A is a dead end for benchmark parity.

---

## P-EX-02: Join Ordering (Selinger DP + beyond)

### Candidate Solution A: Exact DP (DPsize / DPccp) for n ≤ threshold
- **Approach**: Bottom-up DP over connected subplans; DPccp avoids cross products in O(n·2ⁿ).
- **Performance**: Provably optimal under the cost model; runtime sub-ms for n ≤ 15 (Moerkotte & Neumann, SIGMOD 2008 report DPccp covers 18 relations in <1 ms).
- **Time to implement**: ~2.5 months (DPccp is well-documented; statistics reuse from EX-01).
- **Energy cost**: Planning joules negligible (µJ–mJ); big execution-energy wins (optimal order avoids largest intermediate).
- **Upside**: Gold-standard plan quality; deterministic.
- **Downside**: O(n·2ⁿ) memory and O(3ⁿ) time-class blow-up; infeasible > ~15–18 relations.
- **Key paper**: Moerkotte & Neumann, "Dynamic Programming Strikes Back," SIGMOD 2008.

### Candidate Solution B: IDP — DP on partitioned relation sets
- **Approach**: Partition relations into blocks of k (≤8), DP-optimize each block, then greedy/DP combine blocks. Iterative dynamic programming.
- **Performance**: Within 5–15% of optimal on JOB, scaling to 100+ relations (Neumann, IDP, PVLDB 2009).
- **Time to implement**: ~4 months (partitioning heuristic + block recombiner).
- **Energy cost**: Planning ~10× A; execution within ~10% of optimal so net energy low.
- **Upside**: Handles real BI workloads (TPC-H Q5/Q9 have 6–8 joins; cross-database federations have 20+).
- **Downside**: Partitioning heuristic quality dominates; tuning k is workload-sensitive.
- **Key paper**: Neumann, "Query Optimization (Almost) for Free," PVLDB 2009.

### Candidate Solution C: Greedy (GOO) + fallback
- **Approach**: Greedy Operator Ordering — repeatedly join the pair minimizing intermediate cardinality; used only when n exceeds DP threshold.
- **Performance**: 1.5–4× worse than optimal on cyclic/job-style queries (Swami & Gupta, SIGMOD 1988; CMU 15-799 lecture analysis), but acceptable for n>18 where exact is infeasible.
- **Time to implement**: ~1 month.
- **Energy cost**: Cheapest planning; execution energy can be 2–4× optimal — worst on this axis.
- **Upside**: Trivial; robust fallback.
- **Downside**: Plan quality cliff on cyclic schemas (TPC-H is acyclic, so OK there; JOB is hard).
- **Key paper**: Swami & Gupta, "Optimization of Large Join Queries," SIGMOD 1988; GOO in CMU 15-799.

### Recommendation
**A (DPccp) up to n≈15, then B (IDP) for 16–40, then C (GOO) beyond.** Layered fallback is the textbook answer (Moerkotte/Neumann). For TPC-H, A alone suffices (max 8 joins); the layered design future-proofs for JOB/federated.

---

## P-EX-03: Adaptive Execution (MDP/RL)

### Candidate Solution A: Rule-based / Eddy-style routing
- **Approach**: Runtime tuples flow through an operator graph; a router flips switches (hash vs nested-loop, sort vs hash) on cardinality-feedback thresholds.
- **Performance**: Eddies recover 10–40% of plan-quality loss from bad estimates (Avnur & Hellerstein, SIGMOD 2000; Babu & Bizarro, TKDE 2005).
- **Time to implement**: ~3 months.
- **Energy cost**: Per-tuple routing overhead ~5–15% (extra indirection); modest.
- **Upside**: No training data; interpretable.
- **Downside**: Only reorders within pre-enumerated alternatives; can't invent new kernels.
- **Key paper**: Avnur & Hellerstein, "Eddies," SIGMOD 2000; Babu & Bizarro, "Adaptive Query Processing," IEEE TKDE 2005.

### Candidate Solution B: MDP with Bellman optimality
- **Approach**: Model execution as Markov decision process (state = cardinality/pressure estimate, action = kernel/strategy); solve Bellman equations offline over a discretized state space.
- **Performance**: Near-optimal on the modeled workload; ~15–25% better than rule-based on TPC-H variants (Bellman 1957 framework; Deshpande & Hellerstein use MDP for buffer mgmt).
- **Time to implement**: ~5–6 months (state design + reward shaping + solver).
- **Energy cost**: Tiny per-query (policy is a lookup); offline training cost amortized.
- **Upside**: Principled; gives provable bounds under MDP assumptions.
- **Downside**: State discretization + reward engineering is brittle; offline-trained policy stale on drift.
- **Key paper**: Bellman, *Dynamic Programming*, Princeton 1957.

### Candidate Solution C: RL (Q-learning / learned policy, Neo-style)
- **Approach**: A neural policy (or Q-table) maps plan features → kernel/strategy; trained on workload traces, refined online.
- **Performance**: Neo beats PostgreSQL's optimizer by 2–10× on JOB/TPC-H after training (Marcus et al., VLDB 2019). Generalization to unseen schemas is the open problem.
- **Time to implement**: ~8–12 months (infra for training, feature extraction, safety guardrails). Highest risk.
- **Energy cost**: Inference ~µJ/query (tiny MLP); training energy large but amortized.
- **Upside**: Largest ceiling; can exploit cross-kernel patterns a human wouldn't encode.
- **Downside**: Cold-start, nondeterminism (bad for benchmark reproducibility), 1-year bet.
- **Key paper**: Marcus et al., "Neo: A Learned Query Optimizer," PVLDB 2019.

### Recommendation
**A now, B if a 2026 release can absorb 6 months, C as research spike only.** RL (C) is high-ceiling/high-risk and conflicts with reproducible benchmarking (BN-5, BN-7). Eddies (A) ship in 3 months and cover the real failure mode — bad cardinality estimates — which Leis et al. 2015 identify as the dominant optimizer error.

---

## P-EX-04: Trace JIT Specialization

### Candidate Solution A: Cranelift
- **Approach**: Record monomorphic hot traces (per-constant kernel instantiations), compile to Cranelift IR → native; Cranelift is a Bytecode Alliance JIT used by Wasmtime.
- **Performance**: Compile ~10–50 µs/op-equivalent; generated code within ~10–25% of LLVM -O2 (Wasmtime benchmarks); Neumann-grade data-centric codegen gets ~5–50× over interpretation.
- **Time to implement**: ~3 months (trace selector + Cranelift embedding). Cranelift has a clean Rust/C API.
- **Energy cost**: Compile energy ~0.1–1 mJ/op; specialization cuts execution energy 2–10× (fewer dispatches, constant folding).
- **Upside**: Fast compilation keeps JIT viable for short queries; AVX-512 backend exists.
- **Downside**: Peak code quality below LLVM; some vector patterns need manual intrinsics.
- **Key paper**: Gal et al., "Trace-based JIT Type Specialization," PLDI 2009 (trace model); Cranelift docs.

### Candidate Solution B: LLVM ORC
- **Approach**: JIT via LLVM ORCv2 with full optimization pipeline (-O2/-O3) on traces or whole plans.
- **Performance**: Best-in-class generated code; HyPer-class. Compile ~1–20 ms/plan (Neumann 2014 reports HyPer compiles TPC-H queries in <100 ms total via aggressive specialization).
- **Time to implement**: ~5 months (LLVM linkage, ORC session, AVX-512 intrinsic plumbing).
- **Energy cost**: Compile energy ~10–100× Cranelift (mJ–J range); execution energy lowest.
- **Upside**: Maximum execution speed; proven (HyPer, Umbra use LLVM).
- **Downside**: Heavy dependency (~100 MB); slow compile kills short-query latency; relocations/AVX-512 scheduling edge cases.
- **Key paper**: Neumann, "Efficiently Compiling Efficient Query Plans," PVLDB 2014.

### Candidate Solution C: Hand-written asm / template macro assembler
- **Approach**: Pre-emit parameterized AVX-512 asm templates per kernel; specialize by patching immediates/offsets (no IR compiler).
- **Performance**: Fastest compile (patching is ~ns); execution = best hand-tuned. Used by MonetDB/X100 vectorized kernels (Boncz CIDR 2005).
- **Time to implement**: ~6–8 months; one kernel at a time, high per-kernel effort.
- **Energy cost**: Compile ≈0; execution energy excellent if asm is good, terrible if a kernel is hand-tuned for one µarch.
- **Upside**: No JIT dependency; full control over AVX-512 scheduling, AMX, prefetch.
- **Downside**: Per-ISA porting (Intel/AMD/ARM/Apple — directly hurts BN-8); unmaintainable beyond ~20 kernels.
- **Key paper**: Boncz et al., "MonetDB/X100: Hyper-Pipelining Query Execution," CIDR 2005.

### Recommendation
**A (Cranelift) as default JIT + C (asm) for the ~5 hottest kernels.** Cranelift gives LLVM-grade code at interpretation-friendly compile latency, critical for the mixed OLTP/OLAP target. Reserve hand-asm for scan/probe/aggregation where AVX-512 µarch tuning matters most. Skip LLVM ORC unless Cranelift code quality bottlenecks TPC-H.

---

## P-EX-05: Multi-Query Scheduling

### Candidate Solution A: Proportional-share (CFS-style)
- **Approach**: Each query gets a virtual-runtime-weighted slice across morsels; fair CPU share, shared scans opportunistically co-scheduled.
- **Performance**: Throughput within ~15% of optimal under mixed load; tail latency fair (Linux CFS results).
- **Time to implement**: ~2.5 months (morsel scheduler is the substrate — reuse EX-06).
- **Energy cost**: Low overhead; co-scheduled scans cut redundant DRAM reads → energy ↓20–40% on scan-heavy mixes.
- **Upside**: Simple, robust, composes with morsel-driven parallelism (Leis et al. 2014).
- **Downside**: No strategic sharing of sub-plans; "fair" ≠ "minimal total work."
- **Key paper**: Leis et al., "Morsel-Driven Parallelism," SIGMOD 2014.

### Candidate Solution B: Shared scans + common-subexpression sharing
- **Approach**: Coordinator detects overlapping scan predicates across in-flight queries; builds a single shared scan feeding multiple consumers; common sub-plans fused.
- **Performance**: Up to 5–10× throughput on scan-dominated multi-query (Ahmad et al., VLDB 2011 report shared scans dominate MQO gains).
- **Time to implement**: ~5 months (predicate overlap detection, consumer fan-out, correctness).
- **Energy cost**: Largest energy win — shared scan reads each tuple once for N consumers → joules/query ↓ up to N×.
- **Upside**: Aligns with "memory-centric" thesis (minimize memory traffic).
- **Downside**: Sharing overhead can exceed gain when overlap <20%; needs good admission to keep pipeline full.
- **Key paper**: Ahmad, Du, Aboulnaga, multi-query, VLDB 2011; Morton et al., Cormorant, 2024.

### Candidate Solution C: Game-theoretic Nash bargaining
- **Approach**: Model queries as players bargaining over kernel/memory/time; Nash bargaining solution gives a provably fair, Pareto-efficient schedule.
- **Performance**: Theoretically optimal fairness; empirical gains modest vs proportional share (~5–10% tail improvement; Morton et al. Cormorant 2024).
- **Time to implement**: ~7 months (utility design, convergence, integration).
- **Energy cost**: Solver overhead non-trivial at high QPS; neutral on execution energy.
- **Upside**: Provably fair SLA enforcement (differentiates on managed-cloud story).
- **Downside**: Overkill for a first release; utility functions are the hard, workload-specific part.
- **Key paper**: Morton et al., Cormorant, 2024.

### Recommendation
**A now (proportional-share on morsels), B as the flagship "memory-centric" feature.** Shared scans (B) is where this engine's thesis pays off in energy. Defer C (Nash) until SLA/cloud productization.

---

## P-EX-06: Pipeline Parallelism

### Candidate Solution A: Volcano pull model
- **Approach**: Iterator interface — `next()` calls pull data up the tree; classic (Graefe 1994).
- **Performance**: ~5–10× slower than push for analytical pipelines due to virtual-call overhead per tuple (Boncz CIDR 2005 measured iterator overhead dominates at 100M+ tuples).
- **Time to implement**: ~2 months (simplest mental model).
- **Energy cost**: Per-tuple function-call overhead ~5–20% wasted cycles/energy.
- **Upside**: Trivial to reason about; good for OLTP point queries.
- **Downside**: Disqualified for SIMD analytical work — directly causes TPC-H loss.
- **Key paper**: Graefe, "Volcano—An Extensible and Parallel Query Evaluation System," IEEE TKDE 1994.

### Candidate Solution B: Push-based (HyPer data-centric)
- **Approach**: Data flows push-down; each operator is compiled into a pipeline that pushes tuples to a consumer function; pipelines break at materialization points (hash-build side).
- **Performance**: HyPer reaches >1 bn tuples/s/core on scan-filter-aggregate; ~5–10× pull (Neumann 2014).
- **Time to implement**: ~4 months (pipeline breaker detection, codegen integration with EX-04).
- **Energy cost**: Best on energy — no per-tuple dispatch, register-resident tuples, SIMD-friendly; ~2–5× lower joules/tuple than A.
- **Upside**: Industry-proven (HyPer, Umbra, DuckDB-ish); matches AVX-512 kernel table design.
- **Downside**: Requires codegen to be valuable (pair with EX-04); pipeline scheduling complexity.
- **Key paper**: Neumann, PVLDB 2014; Boncz et al., MonetDB/X100, CIDR 2005.

### Candidate Solution C: Data-centric + morsel-driven parallelism
- **Approach**: Push pipelines (B) plus morsel-driven scheduling: cores grab morsels (e.g., 64K-row chunks) to keep L1/L2 warm and balance load dynamically (Leis et al. 2014).
- **Performance**: Linear scaling to NUMA/CXL boundaries; Morsel-Driven Parallelism (MDP) reports near-linear speedup to 64+ cores.
- **Time to implement**: ~5 months (B + morsel dispatcher + NUMA-aware partitioning).
- **Energy cost**: Best-in-class — morsel sizing tuned to L2 keeps cache misses low → joules/tuple minimized; this is the energy-optimal design point.
- **Upside**: Directly expresses the "tier-aware memory" thesis (morsel placement by tier).
- **Downside**: Most engineering; morsel-size tuning is µarch-sensitive (hurts BN-8 portability).
- **Key paper**: Leis et al., "Morsel-Driven Parallelism: A NUMA-Aware Query Evaluation Framework," SIGMOD 2014.

### Recommendation
**C (data-centric + morsel-driven).** This is the architectural centerpiece that ties AVX-512 kernels, tier-aware memory, and the protocol coordinator together. A (pull) is a non-starter for the benchmark target.

---

## P-EX-07: Spill-to-CXL for Large Hash Joins

### Candidate Solution A: Partition build to CXL, probe from DRAM
- **Approach**: Radix-partition the build side; partitions exceeding DRAM budget spill to CXL.type-3 memory; CXL-resident partitions probed via CXL reads rather than NVMe I/O.
- **Performance**: CXL random read ≈350–520 ns vs DRAM 120–140 ns (~3–4×) but vs NVMe ~10–100 µs — a 20–200× latency win on spill (*CXL Memory Performance…*, IEEE 2022; Balkesen et al. 2013 radix join).
- **Time to implement**: ~5 months (CXL allocator, partitioner, fault path).
- **Energy cost**: CXL access ~2–3× DRAM joules but ~100–1000× lower than NVMe (PCIe SerDes + controller). Net: spill energy collapses.
- **Upside**: Eliminates NVMe spill for SF ≤ 1000; the "memory-centric" headline feature.
- **Downside**: CXL bandwidth (PCIe-5 ×16 ≈ 64 GB/s read) caps probe throughput; tail latency sensitive to contention.
- **Key paper**: Balkesen, Alonso, Özsu, "Multi-Core, Main-Memory Joins," VLDB 2013; CXL perf study, IEEE 2022.

### Candidate Solution B: Radix partition with software-managed DRAM staging
- **Approach**: Aggressive radix partitioning to maximize per-partition L2 residency; never spill — instead choose partition count so every partition fits L2/L3 (scale-up the partition fan-out, not the memory).
- **Performance**: Balkesen radix join saturates memory bandwidth at ~1–2 bn tuples/s on a socket; works up to RAM size with graceful degradation.
- **Time to implement**: ~3 months.
- **Energy cost**: Lowest joules/tuple for in-RAM joins (cache-resident); but OOM-fragile beyond RAM.
- **Upside**: No CXL dependency; simplest high-perf path.
- **Downside**: Hard ceiling at RAM size; can't serve SF≥1000 single-node.
- **Key paper**: Balkesen et al., VLDB 2013.

### Candidate Solution C: No-spill / scale-out (NVMe tiered, FOEDUS-style)
- **Approach**: Accept NVMe as the cold tier; use NVM-aware join (FOEDUS) that pipelines NVMe reads with compute to hide latency.
- **Performance**: Handles >10 TB joins; throughput limited by NVMe (~3–7 GB/s/device) — 10–50× slower than CXL on spill path (Kimura, FOEDUS, ICDE 2015).
- **Time to implement**: ~7 months.
- **Energy cost**: NVMe I/O ~mJ/4KB — orders of magnitude worse than CXL on the spill path.
- **Upside**: Cheapest storage; largest datasets.
- **Downside**: Loses the latency/energy story CXL wins.
- **Key paper**: Kimura, "FOEDUS: OLTP Engine for a Thousand Cores and NVM," ICDE 2015.

### Recommendation
**B (radix, in-RAM) for ≤SF100; A (CXL spill) as the flagship for SF100–1000; C (NVMe) only beyond.** The CXL spill path (A) is the differentiator that justifies the "tier-aware memory" architecture and is the strongest energy story in the whole engine.

---

## P-EX-08: Workload Management / Admission Control

### Candidate Solution A: Queueing-theory admission (Kingman/M/M/c)
- **Approach**: Track offered load; admit queries so utilization ρ stays below ~0.7 (where E[E[W]] ≈ ρ/(1−ρ) per Kingman 1961). Reject/queue above threshold.
- **Performance**: Keeps p99 latency bounded; at ρ=0.7, p99 ≈ 3–5× service time vs unbounded blow-up near ρ→1 (Kingman; Kleinrock 1975).
- **Time to implement**: ~2 months (instrument service times, compute ρ).
- **Energy cost**: Neutral on execution; prevents thrash (thrash wastes energy ~2–4×).
- **Upside**: Mathematically grounded; few tunables.
- **Downside**: Assumes Poisson arrivals (OLTP ok, OLAP bursty); service-time distribution shifts.
- **Key paper**: Kingman, "The single-server queue with heavy traffic," 1961; Kleinrock, *Queueing Systems*, 1975.

### Candidate Solution B: Token bucket (deterministic rate limiting)
- **Approach**: Each tenant/queue has a token bucket admitting Q queries/sec; bursts bounded by bucket depth.
- **Performance**: Smooth, predictable; overshoots on bursty workloads (Schroeder & Harchol-Balter 2006 show closed-loop overload is the real risk).
- **Time to implement**: ~1.5 months.
- **Energy cost**: Negligible.
- **Upside**: Simple, SLA-friendly, composes with EX-05 proportional share.
- **Downside**: Token rate is a static guess; mis-sized buckets either waste capacity or admit overload.
- **Key paper**: Schroeder & Harchol-Balter, "Web Servers Under Overload," ACM TOIT 2006.

### Candidate Solution C: ML-based admission (predictive)
- **Approach**: Model predicts query cost + system state; admit if predicted p99 stays under SLA.
- **Performance**: 10–30% better SLO attainment than static (various learned-admission works); high variance.
- **Time to implement**: ~6 months; cold-start + retraining infra.
- **Energy cost**: Inference µJ/query; can paradoxically over-admit if model drifts.
- **Upside**: Adapts to drift; best SLO story.
- **Downside**: Reproducibility (hurts BN-5/BN-7); 1-year maturity risk.
- **Key paper**: Schroeder & Harchol-Balter, ACM TOIT 2006 (baseline); learned admission literature.

### Recommendation
**B (token bucket) as the hard cap layered on A (Kingman ρ-guard).** Cheap, composable, ships in 3 months. C is a research bet deferred past v1.

---

## P-EX-09: Result Caching & Materialized Views

### Candidate Solution A: LRU result cache
- **Approach**: Memoize full query results keyed by (plan hash + parameter hash); LRU eviction by size.
- **Performance**: Hit = sub-µs; misses costly. Hit rate workload-dependent (10–60% on BI repeats).
- **Time to implement**: ~1.5 months.
- **Energy cost**: Hit energy ≈0 (DRAM read); misses full. Good when repeats exist.
- **Upside**: Trivial; high payoff on dashboards.
- **Downside**: No partial reuse; invalidation on base updates is coarse (invalidate all).
- **Key paper**: Standard cache literature; Levy & Finkelstein, "View caching," 1980s.

### Candidate Solution B: Differential dataflow / DBSP incremental maintenance
- **Approach**: Views maintained as dataflow graphs; base deltas propagate as incremental recomputation — O(Δ) not O(N) per update.
- **Performance**: McSherry reports differential dataflow maintaining complex views at ms-latency under update streams (Murray et al., CIDR 2013); DBSP formalizes this for SQL (Budiu et al., SIGMOD 2023).
- **Time to implement**: ~7–9 months (incremental engine is a subsystem).
- **Energy cost**: Per-update energy ~Δ/N of full recompute — large win for high-update/low-Δ.
- **Upside**: Strongest correctness story (DBSP gives proven convergence); flagship for streaming+SQL.
- **Downside**: High complexity; memory overhead for the graph; not worth it for one-shot OLAP.
- **Key paper**: Murray, McSherry et al., "Differential Dataflow," CIDR 2013; Budiu et al., "DBSP," SIGMOD 2023.

### Candidate Solution C: Trigger-based delta maintenance
- **Approach**: Per-view triggers compute deltas on base insert/update/delete; classic SQL materialized-view maintenance.
- **Performance**: Correct; O(view-size) per base update in the worst case; can deadlock under concurrent writes.
- **Time to implement**: ~4 months.
- **Energy cost**: Per-update energy grows with view count; contention wastes cycles.
- **Upside**: Standard, well-understood, SQL-native.
- **Downside**: Doesn't scale to many views or high write rates.
- **Key paper**: Gupta, Mumick, "Maintenance of Materialized Views," 1995.

### Recommendation
**A (LRU) for v1 dashboards + B (DBSP) as the strategic bet for a streaming/HTAP product.** Skip C — it is dominated by B on every axis except initial simplicity, and A covers simple cases. B aligns with the "incremental = energy-efficient" thesis.

---

## P-EX-10: Distributed Query Execution

### Candidate Solution A: Pull-based (Volcano-Graefe exchange)
- **Approach**: Iterator model + Exchange operator for inter-node data movement; pull-driven.
- **Performance**: Baseline; per-tuple RPC overhead dominates at fine granularity (Graefe 1994; Kossmann distributed survey).
- **Time to implement**: ~4 months (exchange, partitioning, shuffle).
- **Energy cost**: Network is the energy sink (~1–5 µJ/bit at 10–100 GbE); pull's small messages amplify this.
- **Upside**: Simple, composes with EX-06 if you ever use pull locally (you won't).
- **Downside**: Latency + energy worst-in-class for OLAP.
- **Key paper**: Graefe, IEEE TKDE 1994.

### Candidate Solution B: Push-based distributed (HyPer/Umbra exchange on push pipelines)
- **Approach**: Local push pipelines (EX-06B) feed network shuffles that batch tuples into large packets; receiver pushes into its own pipeline.
- **Performance**: ~5–10× pull on analytical shuffles; saturates network bandwidth (Neumann 2014; Umbra scale-out work).
- **Time to implement**: ~6 months.
- **Energy cost**: Large packets amortize network energy → ~2–5× lower joules/tuple-shuffled than A.
- **Upside**: Consistent with local architecture; best latency.
- **Downside**: Requires the local push engine (EX-06) to exist first.
- **Key paper**: Neumann, PVLDB 2014.

### Candidate Solution C: Data-centric shared-nothing (DuckDB-style federated / morsel exchange)
- **Approach**: Each node runs the full engine; coordinator morsel-schedules across nodes with locality preference; minimal data movement (compute-to-data).
- **Performance**: Raasveldt & Mühleisen show DuckDB single-node is already competitive; federation adds linear scale-out with low overhead (DuckDB, SIGMOD 2019; Sarker et al., PVLDB 2024).
- **Time to implement**: ~8 months (distributed morsel scheduler, locality-aware placement).
- **Energy cost**: Best — minimizes network by preferring local compute; "memory-centric" thesis at cluster scale.
- **Upside**: Cleanest scale story; locality → energy.
- **Downside**: Most engineering; fault tolerance needs separate design.
- **Key paper**: Raasveldt & Mühleisen, "DuckDB," SIGMOD 2019; Sarker et al., PVLDB 2024.

### Recommendation
**B (push + batched shuffle) for v1 distributed; evolve to C (locality-aware morsel exchange).** A is obsolete. If single-node CXL (EX-07) covers the scale-up story, distributed can even be deferred past v1.

---

## P-EX-11: Code Generation (Cranelift vs LLVM vs asm)

*(Overlaps EX-04; here focused on whole-plan compilation rather than trace JIT.)*

### Candidate Solution A: Cranelift whole-plan compilation
- **Approach**: Compile an entire morsel-pipeline (scan→filter→probe→agg) to one Cranelift function; specialize constants via the kernel table.
- **Performance**: Within ~10–25% of LLVM -O2; compile ~1–10 ms/plan (Wasmtime data). Good enough for TPC-H parity.
- **Time to implement**: ~3 months (depends on EX-04A infra).
- **Energy cost**: Compile ~1–10 mJ/plan; execution energy near-optimal after specialization.
- **Upside**: Light dependency; fast compile enables per-query recompilation.
- **Downside**: AVX-512 code scheduling is less mature than LLVM.
- **Key paper**: Neumann PVLDB 2014 (the approach); Cranelift/Wasmtime.

### Candidate Solution B: LLVM whole-plan (HyPer/Umbra)
- **Approach**: Full LLVM -O2/-O3 on each plan; HyPer's approach.
- **Performance**: Best execution; HyPer compiles all 22 TPC-H queries in <1 s aggregate, runs them in ~seconds (Neumann 2014).
- **Time to implement**: ~5 months.
- **Energy cost**: Compile energy high (~10–100 mJ/plan) but execution energy lowest.
- **Upside**: Peak performance ceiling.
- **Downside**: 100 MB+ dependency; slower compile hurts short queries; AVX-512 scheduling quirks.
- **Key paper**: Neumann, PVLDB 2014.

### Candidate Solution C: asm template expansion (no compiler)
- **Approach**: Statically emit kernels; "compilation" is parameter substitution + link.
- **Performance**: Compile ≈0; execution = hand-tuned quality per kernel, no cross-kernel fusion.
- **Time to implement**: ~6 months for a usable kernel set.
- **Energy cost**: Compile ≈0; execution energy mediocre without fusion (misses constant-fold across ops).
- **Upside**: No JIT risk; reproducible (good for BN-5).
- **Downside**: No fusion = loses HyPer's biggest win (Neumann 2014 attributes much of the speedup to operator fusion).
- **Key paper**: Boncz et al., MonetDB/X100, CIDR 2005; Leis et al., VLDB 2015.

### Recommendation
**A (Cranelift) for plan compilation, reusing EX-04's Cranelift investment.** Reserve B (LLVM) behind a feature flag for the few queries where Cranelift trails. C alone loses fusion, HyPer's main advantage.

---

# PART B — BENCHMARKING PROBLEMS (8)

## P-BN-01: TPC-H (Expected Loss vs DuckDB)

### Candidate Solution A: Run TPC-H as-is, accept 1.2–1.5× loss
- **Approach**: Standard SF1/SF10/SF100, published DuckDB-comparable harness; report gap honestly.
- **Performance**: Lose to DuckDB by ~1.2–1.5× (DuckDB matches/edges ClickHouse & HyPer at SF10 per public benchmarks). Acceptable for v1.
- **Time to implement**: ~1.5 months (data gen, harness, 22 queries, cold/warm).
- **Energy cost**: Baseline joules/query — establishes the energy-per-query axis for BN-6.
- **Upside**: Credibility (TPC-H is the lingua franca); honest baseline.
- **Downside**: A loss is a loss — marketing-negative.
- **Key paper**: TPC Benchmark H spec v3; DuckDB/SIGMOD 2019.

### Candidate Solution B: Optimize the 4–5 queries that dominate the gap
- **Approach**: Profile; the gap usually concentrates in Q5 (5-join), Q9 (6-join), Q21 (existence subquery). Target EX-01/EX-02 for these.
- **Performance**: Closing Q5/Q9/Q21 typically recovers 60–80% of the gap (Leis et al. JOB analysis shows plan quality concentrates losses).
- **Time to implement**: ~3 months (focused optimizer + codegen work).
- **Energy cost**: Same harness; per-query energy improves as plans improve.
- **Upside**: Turns the headline from "loses" to "competitive."
- **Downside**: Looks like gaming TPC-H; must disclose.
- **Key paper**: Leis et al., "How Good Are Query Optimizers, Really?" VLDB 2015.

### Candidate Solution C: Skip TPC-H, lead with custom benchmarks
- **Approach**: Decline the head-to-head; emphasize schema-fluid + CXL benchmarks (BN-3, BN-7).
- **Performance**: Avoids the loss entirely in messaging.
- **Time to implement**: ~0 (deferral).
- **Energy cost**: N/A.
- **Upside**: Controls the narrative.
- **Downside**: Loss of credibility — every reviewer asks "but what about TPC-H?"
- **Key paper**: TPC-H spec.

### Recommendation
**A + B in parallel.** Run honestly (A) to establish the energy baseline and credibility, while B closes the gap to ≤1.1×. Never C — skipping TPC-H reads as evading.

---

## P-BN-02: TPC-C (Win Path — Consolidation Story)

### Candidate Solution A: Single fat box
- **Approach**: Run TPC-C on one high-core-count NUMA/CXL box; consolidation = "one machine replaces a cluster."
- **Performance**: Modern 2-socket boxes reach ~1–5M tpmC (OceanBase hit 707M tpmC on a cluster, VLDB 2022; single-node far lower but still strong).
- **Time to implement**: ~4 months (TPC-C harness, 5 txns, consistency).
- **Energy cost**: Best joules/tpmC — no network; consolidation is an energy story.
- **Upside**: Strongest "memory-centric consolidation" narrative.
- **Downside**: Single-node ceiling; can't claim cloud scale.
- **Key paper**: TPC-C spec v5; OceanBase, VLDB 2022.

### Candidate Solution B: CXL cluster (memory-pooled)
- **Approach**: Multiple compute nodes over a CXL fabric pooling memory; TPC-C distributed.
- **Performance**: PolarDB-class distributed OLTP reaches tens of M tpmC (PolarDB, VLDB 2025). CXL pooling reduces remote-fault latency vs shared-disk.
- **Time to implement**: ~9–12 months (distributed tx + CXL fabric).
- **Energy cost**: Network/fabric overhead adds ~10–30% joules/txn vs single-node.
- **Upside**: Cloud-native story; differentiates on CXL.
- **Downside**: Long engineering; CXL fabrics are immature (2025).
- **Key paper**: PolarDB, VLDB 2025; OceanBase, VLDB 2022.

### Candidate Solution C: Cloud (managed-service style)
- **Approach**: Deploy on cloud VMs; elastic scaling; report tpmC/$ and tpmC/W.
- **Performance**: Elastic but per-instance weaker; cloud overheads (virtualization) cost ~10–25% (SPEC Cloud, ICPE).
- **Time to implement**: ~6 months.
- **Energy cost**: Cloud PUE adds ~10–30% to physical joules.
- **Upside**: tpmC/$ story for buyers.
- **Downside**: Hardest to control energy measurement (BN-6) on shared cloud.
- **Key paper**: SPEC Cloud IaaS, ICPE 2022.

### Recommendation
**A (single fat box) is the win path.** "One CXL box beats a 3-node cluster on tpmC, $, and W" is the consolidation thesis. B/C are later-cloud stories.

---

## P-BN-03: Custom Benchmark — Schema-Fluid (TPC-Fluid)

### Candidate Solution A: JSON/semi-structured workload
- **Approach**: Mixed typed & nested columns (int, float, string, JSON) with late schema binding; queries touch heterogeneous columns.
- **Performance**: Engine's "instruction-first" strength — kernel dispatch by type at runtime. ClickBench shows JSON columns are where generic engines lose to specialized (ClickBench).
- **Time to implement**: ~2.5 months.
- **Energy cost**: Type-dispatch overhead ~5–15% vs monomorphic; still far cheaper than parsing.
- **Upside**: Directly showcases the kernel-table design.
- **Downside**: No standard spec → comparability contested.
- **Key paper**: ClickBench; Kim et al. JSON benchmarks (VLDB).

### Candidate Solution B: Log-analytics workload
- **Approach**: Semi-structured logs (regex, timestamp, JSON fields); high-cardinality group-bys.
- **Performance**: Realistic (matches Splunk/Druid use); wide column-type mix.
- **Time to implement**: ~2 months.
- **Energy cost**: String ops dominate energy (~10–50× numeric); good differentiator.
- **Upside**: Industrially credible.
- **Downside**: Results depend on log corpus choice.
- **Key paper**: ClickBench; log-analytics benchmarks (Elastic/Druid).

### Candidate Solution C: Synthetic mixed-type generator
- **Approach**: Parameterized generator producing columns of every type at tunable skew; reproducible.
- **Performance**: Full control; no real-world credibility.
- **Time to implement**: ~3 months.
- **Energy cost**: Cleanest energy attribution (controlled).
- **Upside**: Reproducible (good for BN-5); parametric sweep.
- **Downside**: "Synthetic" dismissibility.
- **Key paper**: ClickBench methodology.

### Recommendation
**A (JSON) + C (synthetic generator) together.** A is the headline narrative; C is the reproducible backbone. B as a secondary real-world sanity check.

---

## P-BN-04: Custom Benchmark — Similarity Joins (Hamming)

### Candidate Solution A: Image pHash (64-bit Hamming) joins
- **Approach**: Join two image corpora by Hamming distance ≤ k on 64-bit perceptual hashes; classic LSH use case.
- **Performance**: LSH (Andoni–Indyk) gives (1+ε)-approx in sublinear ~O(n^{1+1/c}); exact is O(n²).
- **Time to implement**: ~3 months (LSH + AVX-512 popcount kernel).
- **Energy cost**: Popcount on AVX-512 (VPCMP+POPCNT) is ~nJ/64-bit pair; LSH reduces work → energy ↓.
- **Upside**: Visually compelling; real use (dedup, copyright).
- **Downside**: 64-bit limits Hamming range; k small.
- **Key paper**: Andoni & Indyk, "Near-Optimal Hashing for ANN," CACM 2008.

### Candidate Solution B: Log / text near-duplicate (MinHash + Hamming)
- **Approach**: Near-duplicate log lines via MinHash → banded LSH; Hamming on shingle sketches.
- **Performance**: Industry-standard dedup; ~O(n) with tuned bands.
- **Time to implement**: ~2.5 months.
- **Energy cost**: MinHash is hash-heavy (~µJ/shingle); energy dominated by hashing.
- **Upside**: Operationally useful.
- **Downside**: Less "pure" Hamming story.
- **Key paper**: Broder, "On the Resemblance and Containment of Documents," 1997; Andoni–Indyk 2008.

### Candidate Solution C: Document embeddings (high-dim, Hamming on binarized vectors)
- **Approach**: Binarize BERT embeddings → 256–1024-bit Hamming; ANN over binary codes.
- **Performance**: Binary codes with LSH match float-cosine within ~5–10% recall at 10–50× speed (various).
- **Time to implement**: ~4 months (embedding pipeline + index).
- **Energy cost**: Embedding generation dominates (GPU ~J/query); search cheap.
- **Upside**: Trendy (RAG/semantic).
- **Downside**: Embedding cost pollutes the "join" energy measurement.
- **Key paper**: Andoni–Indyk 2008.

### Recommendation
**A (pHash) as the flagship** — clean 64-bit Hamming, AVX-512 popcount kernel is a perfect kernel-table demo, Andoni–Indyk gives the algorithmic citation. B as a real-world add-on. C deferred (embedding cost muddies energy numbers).

---

## P-BN-05: Approximate Queries — (ε,δ) Validation

### Candidate Solution A: Statistical validation (Hoeffding/Chernoff bounds)
- **Approach**: For online aggregation, prove Pr[|est−true|>ε] ≤ δ via concentration bounds; tune sample size n ≥ ln(2/δ)/(2ε²).
- **Performance**: Validation is cheap (math), samples scale as O(1/ε²).
- **Time to implement**: ~2 months.
- **Energy cost**: Approximate query saves energy ~N/n (sample vs full scan).
- **Upside**: Rigorous (ε,δ) guarantee; great energy story.
- **Downside**: Bounds assume i.i.d. sampling — violated by correlated/skewed data.
- **Key paper**: Hellerstein et al., "Online Aggregation," SIGMOD 1997.

### Candidate Solution B: Oracle comparison (vs exact, on small scale)
- **Approach**: Run exact query on SF1, compare approximate; report error empirically.
- **Performance**: Ground-truth; no assumptions.
- **Time to implement**: ~1 month.
- **Energy cost**: Doubles compute (exact + approx) but only at small scale.
- **Upside**: Convincing empirically.
- **Downside**: No guarantee at larger scale; not a proof.
- **Key paper**: Hellerstein 1997; Cormode, "Sketches," 2008.

### Candidate Solution C: Property-based / differential testing
- **Approach**: Generate random queries; assert approximate result respects algebraic properties (monotonicity, distributivity) within bounds.
- **Performance**: Finds bugs; not a quality metric.
- **Time to implement**: ~3 months (generator + oracle harness).
- **Energy cost**: Low.
- **Upside**: Catches correctness bugs in the approximate engine.
- **Downside**: Doesn't validate (ε,δ) numerically.
- **Key paper**: Cormode, "Synopses," Foundations & Trends in DDB 2008; QuickCheck-style PBT.

### Recommendation
**A (statistical bounds) as the headline guarantee + B (oracle) for empirical reporting + C (PBT) in CI.** All three are cheap; together they give provable + empirical + tested. The energy savings of approximate queries (A) reinforce the engine's energy thesis.

---

## P-BN-06: Energy Efficiency Benchmark (Joules/Query)

### Candidate Solution A: Intel RAPL
- **Approach**: Read `/sys/class/powercap/intel-rapl` package+DRAM energy counters around each query; report J/query.
- **Performance**: Sampling at ms granularity; ±5% accuracy validated against physical meters (Hahnel 2012; RAPL accuracy studies).
- **Time to implement**: ~1 month.
- **Energy cost**: N/A (this is the meter).
- **Upside**: Free on every Intel box; standard in DB energy literature.
- **Downside**: Intel-only (AMD has RAPL-equivalent `amd_energy`; ARM/Apple lack it → hurts BN-8).
- **Key paper**: Hahnel, Döbel, Härtig, "Measuring Energy Consumption for Short Code Paths Using RAPL," 2012.

### Candidate Solution B: External physical meter (e.g., Watts Up? / Yokogawa)
- **Approach**: Clamp meter on the AC feed; log W at 1–10 Hz; integrate.
- **Performance**: Ground-truth whole-system power; coarse (can't isolate query).
- **Time to implement**: ~1.5 months (hardware + sync).
- **Energy cost**: N/A.
- **Upside**: Vendor-neutral; defensible; covers idle + peripherals RAPL misses.
- **Downside**: Low temporal resolution; cannot attribute J to a single query without idle subtraction.
- **Key paper**: Tiwari & Malik, "Power Analysis of Embedded Software," DAC 1994 (energy methodology).

### Candidate Solution C: Analytical model (op-cost → J)
- **Approach**: Build a cost model: each kernel has measured nJ/tuple on each µarch; sum to predict J/query without running.
- **Performance**: Predictive; enables "what-if" without hardware.
- **Time to implement**: ~4 months (characterize each kernel per µarch).
- **Energy cost**: N/A.
- **Upside**: Portable (drives BN-8 cross-vendor matrix); explains *why* a query costs energy.
- **Downside**: Model error ±20–40% vs measurement; constant maintenance.
- **Key paper**: Tiwari & Malik, DAC 1994; energy models in DB literature.

### Recommendation
**A (RAPL) on Intel/AMD for measured headline numbers + C (model) for cross-vendor extrapolation (BN-8) + B (meter) as a periodic calibration anchor.** A is the daily driver; B calibrates A; C makes the cross-vendor story (BN-8) tractable without buying every CPU.

---

## P-BN-07: Latency Tail Benchmark (CXL under Load)

### Candidate Solution A: Saturation benchmark (closed-loop)
- **Approach**: Ramp concurrent clients; record p50/p99/p99.9 as ρ→1.
- **Performance**: Directly reveals the latency-collapse point; typical finding: p99/p50 ratio blows up >10× past ρ≈0.8 (Schroeder & Harchol-Balter 2006).
- **Time to implement**: ~2 months (load generator, percentile stream).
- **Energy cost**: Captures energy-under-load (thrash wastes W).
- **Upside**: Empirical, reproducible.
- **Downside**: Closed-loop masks arrival-rate effects (Schroeder NSDI 2006 "open vs closed").
- **Key paper**: Schroeder & Harchol-Balter, "Open vs Closed," NSDI 2006.

### Candidate Solution B: Queueing model (Kingman's approximation)
- **Approach**: Fit M/G/c model from measured service-time distribution; predict p99 via Kingman's E[W]≈(ca²+cs²)/2·ρ/(1−ρ)·E[S].
- **Performance**: Predicts tail without saturating the system; validates A's measurements.
- **Time to implement**: ~2.5 months.
- **Energy cost**: No load needed → no energy spent finding the cliff.
- **Upside**: Cheap; explains tail causally (variance vs load).
- **Downside**: Assumes specific arrival/service distributions; breaks under heavy-tailed service.
- **Key paper**: Kingman 1961; Kleinrock 1975.

### Candidate Solution C: Production-trace replay
- **Approach**: Replay a real open-system arrival trace (e.g., from a customer) under CXL.
- **Performance**: Most realistic; captures real burstiness (self-similarity) that Poisson models miss.
- **Time to implement**: ~3 months (trace acquisition is the blocker).
- **Energy cost**: Realistic energy-under-real-load.
- **Upside**: Credibility with buyers.
- **Downside**: Trace availability + privacy; single-point result.
- **Key paper**: Schroeder & Harchol-Balter 2006; self-similar traffic (Leland et al. 1994).

### Recommendation
**A (saturation) for the headline p99 chart + B (Kingman) to explain it + C (trace) if a partner provides one.** The CXL-under-load tail story is exactly where the admission control (EX-08) earns its keep; A+B prove it.

---

## P-BN-08: Cross-Vendor Kernel Benchmark Matrix (Intel/AMD/ARM/Apple)

### Candidate Solution A: Cloud instances
- **Approach**: Run the kernel suite on EC2/Azure/GCP instances spanning Intel Xeon, AMD EPYC, ARM Graviton (Apple via bare-metal Mac).
- **Performance**: Real silicon; EC2 variance ~5–15% (noisy neighbor) hurts microbenchmarks.
- **Time to implement**: ~3 months (provisioning + isolation).
- **Energy cost**: Cloud RAPL only on Intel/AMD; Graviton has no RAPL → energy incomplete (use BN-06-C model).
- **Upside**: Cheapest hardware path; broad coverage.
- **Downside**: Virtualization noise; Apple Silicon only via Mac Studio bare-metal; energy gaps.
- **Key paper**: SPEC Cloud IaaS, ICPE 2022.

### Candidate Solution B: On-prem dedicated hardware
- **Approach**: Buy/loan one board per ISA; run isolated.
- **Performance**: Cleanest numbers; ±1–3% repeatability.
- **Time to implement**: ~4 months (procurement is the long pole, ~2–3 mo lead time).
- **Energy cost**: Full physical metering (BN-06-B) possible on all.
- **Upside**: Publication-grade; defensible energy.
- **Downside**: Capital cost (~$30–80k); ARM/Apple server boards scarce.
- **Key paper**: SPEC CPU benchmarking methodology.

### Candidate Solution C: Simulation (gem5/QEMU + µarch models)
- **Approach**: Simulate each ISA; predict cycles + energy from a µarch model.
- **Performance**: No real-silicon variance; but simulator error ±10–30% on out-of-order cores.
- **Time to implement**: ~5 months (model calibration dominates).
- **Energy cost**: Model-only; useful to fill gaps (Apple Silicon energy via model).
- **Upside**: Covers ISAs you can't buy (e.g., future AVX-10, Apple server); no hardware.
- **Downside**: Results contested without at least one silicon anchor per ISA.
- **Key paper**: Tiwari & Malik DAC 1994; µarch simulators (gem5).

### Recommendation
**B (on-prem) for the 3 ISAs you can buy (Intel, AMD, ARM-Graviton) + C (model) to extrapolate Apple/future + A (cloud) for spot-check breadth.** Anchor every ISA with at least one real measurement (BN-06), then model the rest. Pure simulation (C alone) lacks credibility.

---

# CROSS-CUTTING SYNTHESIS

**Architectural through-line (Execution):** The "instruction-first, memory-centric" thesis is best expressed by a coherent stack: rule-based physical→kernel lowering (EX-01C) → DPccp/IDP join ordering (EX-02) → data-centric morsel-driven pipelines (EX-06C) → Cranelift plan compilation with hand-asm hot kernels (EX-04A + EX-11A) → CXL spill for large joins (EX-07A) → proportional-share + shared-scan multi-query (EX-05A+B) → Kingman/token-bucket admission (EX-08A+B) → DBSP incremental views (EX-09B) → push-based distributed exchange (EX-10B). This stack optimizes the memory-traffic axis — which is the energy axis — at every layer.

**Energy thesis (Benchmarking):** The engine's defensible novelty is **joules/query**, not raw speed (where DuckDB/HyPer will win on TPC-H). Concretely: CXL spill (EX-07) replaces NVMe (~100–1000× energy reduction on the spill path), shared scans (EX-05) cut redundant DRAM reads (up to N×), morsel sizing (EX-06) minimizes cache misses, and approximate queries (BN-05) reduce scan work ~N/n×. Measure it with RAPL (BN-06A) anchored by a physical meter (BN-06B).

**Highest-leverage engineering bets (next 12 months):**
1. Data-centric morsel-driven pipelines (EX-06C) — 5 mo, foundation for everything.
2. CXL spill path (EX-07A) — 5 mo, the flagship differentiator + energy story.
3. DPccp + cost-model lowering (EX-02A + EX-01B) — 7 mo combined, closes TPC-H gap.
4. Cranelift plan compilation (EX-04A/EX-11A) — 3 mo, HyPer-class codegen at low compile cost.
5. RAPL + meter energy harness (BN-06A+B) — 2.5 mo, cheap, enables the whole energy narrative.

**Defer / research-spike:** RL adaptive execution (EX-03C), Nash scheduling (EX-05C), ML admission (EX-08C), distributed scale-out (EX-10B/C beyond single-node CXL), learned Neo-style optimizer. These are high-ceiling/high-risk and several hurt benchmark reproducibility (BN-05, BN-07).

**Benchmark sequencing:** BN-01 (TPC-H honest + targeted opt) → BN-06 (energy harness, do this *early* — it instruments everything else) → BN-07 (CXL tail) → BN-02 (TPC-C single fat box) → BN-03/BN-04 (custom schema-fluid + similarity, the differentiation story).
