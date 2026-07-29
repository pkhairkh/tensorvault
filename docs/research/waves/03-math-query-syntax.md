# Wave 3 Research: Math + Query Syntax Problems

**Engine context:** instruction-first, memory-centric DB engine; AVX-512 kernel table; tier-aware memory (L3 / DDR5 / CXL.mem / NVMe); protocol coordinator. SQL extended with `APPROXIMATE`, `TIER`, `SIMILAR TO`, `CONSISTENCY`, `SCOPE`, `USING`, `MEMORY BUDGET`, `ENERGY BUDGET`, `CONTINUOUS QUERY`.

**Energy baseline used throughout** (measured via RAPL, Hähnel et al. SIGMETRICS/GreenMetrics 2012; validated by Khan et al. 2018 — RAPL matches external metering to within a constant offset, accurate for ≥ ~10 ms windows; sub-ms sampling adds overhead without benefit):

| Tier | Access | Energy per op (nJ) |
|---|---|---|
| L3 hit | ~5 ns | ~1–3 nJ |
| DDR5 64B random read | ~80 ns | ~10–20 nJ |
| CXL.mem 64B read | ~170 ns | ~30–60 nJ |
| NVMe 4 KB read | ~80 µs | ~3–8 µJ |
| AVX-512 VPOPCNTDQ | 64 popcounts, 1/cyc | ~0.2 nJ/popcount |

---

# PART A — MATHEMATICAL PROBLEMS

## P-M-01: MDL Schema Selection — Closed-form or LP?

### Candidate Solution A: Greedy per-column MDL
- **Approach**: For each column choose the model T minimizing total description length L(T) = −log₂ p(x|T) + L(T); iterate greedily, O(|T|) per column.
- **Performance**: O(|T|·n) per column; for a 1 M-row column with ~20 candidate types this is ~20 M ops, sub-second. No iteration across columns, so trivially parallel across columns (Grünwald, *The Minimum Description Length Principle*, MIT Press 2007).
- **Time to implement**: ~1.5 months. A single well-tested scoring routine plus a model registry.
- **Energy cost**: ~pure compute, AVX-friendly. Estimated ~0.5–2 mJ per column at 1 M rows (RAPL-model estimate; dominated by histogram passes).
- **Upside**: Simple, robust, embarrassingly parallel; degrades gracefully on novel distributions.
- **Downside**: Greedy can miss joint structure (correlated columns), so it leaves bits on the table vs. joint MDL.
- **Key paper**: Grünwald 2007 (MDL); Rissanen 1978 (*Automatica*), "Modeling by shortest data description."

### Candidate Solution B: LP relaxation over a candidate-type lattice
- **Approach**: Encode the choice of types per (column, partition) as a 0/1 packing LP and relax; constraints cap memory/time. Solve with simplex/interior-point and round.
- **Performance**: LP with ~10⁴ vars solves in tens of ms with HiGHS; scales to ~10⁶ vars with decomposition (Bertsimas-Tsitsiklis). Far slower than greedy.
- **Time to implement**: ~4 months (LP integration, warm starts, rounding heuristics, validation against greedy).
- **Energy cost**: ~10–50× greedy — LP solvers are branch-heavy, low ILP. Estimated ~10–50 mJ per column-set on a 32-col table.
- **Upside**: Captures cross-column coupling (e.g., dict-encode A if B is dict-encoded); provable optimality gap on the relaxed problem.
- **Downside**: Solver dependency, energy cost, harder to reason about; rounding can violate the lattice ordering.
- **Key paper**: Raghavan-Thompson 1987 (*STOC*), "Probabilistic construction of deterministic algorithms."

### Recommendation
**Greedy (A) as the default, with a closed-form fast path for the two common cases** — uniform (store min/max, no model) and low-cardinality categorical (dictionary + bitmap). Reserve LP (B) for offline `ANALYZE` re-optimization of the hottest, widest tables where the ~5–10% extra compression justifies the ~20× energy. This honors the engine's energy budget hint natively.

---

## P-M-02: Spectral Partitioning for NUMA

### Candidate Solution A: Spectral bisection via Fiedler vector
- **Approach**: Build the graph Laplacian L = D − A of the access-affinity graph; partition by the sign of the Fiedler vector (2nd-smallest eigenvector). Use Lanczos iteration.
- **Performance**: Lanczos converges in O(k·m) for k iterations, m edges; for a 256-core NUMA graph (m ~ 10⁴) this is ~ms-scale per recompute. Bisection cut quality provably ≤ √(2λ₂·|V|) (Chung, *Spectral Graph Theory*, AMS 1997).
- **Time to implement**: ~2 months (Laplacian build, Lanczos, sign-cut placement). Reuse Eigen/Spectra.
- **Energy cost**: Dominated by sparse mat-vec products, ~1–5 mJ per partition decision on a 256-node graph.
- **Upside**: Mathematically grounded balance/communication tradeoff; smooth re-partitioning on access-pattern drift.
- **Downside**: Fiedler-only bisection is weak on graphs with clustered community structure (multi-way needs k eigenvectors).
- **Key paper**: Chung 1997; Fiedler 1973 (*Czech. Math. J.*).

### Candidate Solution B: Spielman–Teng nearly-linear Laplacian solver
- **Approach**: Solve Lx = b in nearly-linear time O(m log^c n) using the Spielman–Teng SDD solver; enables solving the Laplacian system for the Fiedler vector and for electrical-flow partitioning.
- **Performance**: O(m polylog n); verified nearly-linear in practice with higher constants (Li-Spielman-Theng-Wu parallel solver, SPAA 2023). For our sizes this is comparable to Lanczos but slower in absolute terms due to constants.
- **Time to implement**: ~6+ months — the solver itself is a research artifact; integrating a maintained implementation is the bottleneck.
- **Energy cost**: ~2–3× Lanczos in practice due to low arithmetic intensity and recursion.
- **Upside**: Provably near-linear, supports arbitrary linear-inequality partitioning constraints, future-proof.
- **Downside**: Implementation complexity and constants make it lose to Lanczos at DB-scale graph sizes.
- **Key paper**: Spielman-Teng, *JACM* 2004/2011 ("Nearly-linear time algorithms for graph partitioning…").

### Candidate Solution C: METIS multilevel k-way
- **Approach**: Coarsen → partition → uncoarsen with refinement; standard industrial default for graph partitioning.
- **Performance**: O(m) practical; partitions 10⁶-node graphs in <1 s; cut quality within a few % of spectral (Karypis-Kumar, *SIAM J. Sci. Comput.* 1998).
- **Time to implement**: ~1 month (call into libmetis / KaHIP).
- **Energy cost**: Lowest of the three — cache-friendly coarsening; ~0.5–1 mJ per partition.
- **Upside**: Battle-tested, fast, handles multi-constraint (balance memory AND compute) partitioning natively.
- **Downside**: No spectral guarantee; heuristic; external dependency.
- **Key paper**: Karypis-Kumar 1998; Sanders-Schulz KaHIP (ALENEX 2013).

### Recommendation
**METIS (C) for production partitioning, spectral bisection (A) as the fallback when NUMA access patterns are smooth/diffusive** (where the Fiedler guarantee matters). Defer (B) unless linear-constraint partitioning becomes a hard requirement — at DB graph sizes the constants dwarf the asymptotic win, and the energy/implementation cost is the worst of the three.

---

## P-M-03: Closed-form Cost Model (Kingman + AVX-512)

### Candidate Solution A: Calibrated analytic model
- **Approach**: Closed-form latency = compute (Σ kernel_cycles × inv_throughput) + queueing (Kingman's formula E[W] ≈ (ρ/(1−ρ))·(c_a²+c_s²)/2·E[S]) + memory-tier transfer (Σ accesses × tier_latency). Calibrate constants via microbenchmarks.
- **Performance**: O(plan size) to evaluate, microseconds; Kingman 1961 gives the M/G/1 expected wait in closed form; AVX-512 throughput table from Agner Fog / uops.info feeds the compute term.
- **Time to implement**: ~2 months (microbench harness + calibration pipeline + sanity tests).
- **Energy cost**: Negligible to evaluate (~µJ per plan); the cost is one-time calibration (~seconds of RAPL runs).
- **Upside**: Interpretable, debuggable, zero inference latency at query time; degrades gracefully out-of-distribution.
- **Downside**: Kingman assumes M/G/1 and steady state; bursty CXL traffic violates this, biasing estimates 10–30%.
- **Key paper**: Kingman 1961 (*Cambridge Phil. Soc.*); Marcus et al., *PVLDB* 12(11), 2019 ("Neo") as the accuracy target.

### Candidate Solution B: Learned model (gradient-boosted)
- **Approach**: Train a GBM/NN on (plan features, observed latency, energy) from the query log; predict latency + energy per plan.
- **Performance**: Inference ~µs with a GBM; Neo reports ~23% latency reduction vs. the default PostgreSQL planner (Random Forest cost model).
- **Time to implement**: ~4 months (feature engineering, online collection, retraining loop, fallback).
- **Energy cost**: Inference ~10–100× the analytic model (~µJ vs. sub-µJ); training is amortized offline.
- **Upside**: Captures the AVX-512 downclocking and cache-contention nonlinearities the closed form misses.
- **Downside**: Opaque, needs continuous labeled data, drifts on schema/DDL changes, cold-start problem.
- **Key paper**: Marcus et al., *PVLDB* 2019 (Neo); Marcus et al. SIGMOD 2021 (Bao).

### Recommendation
**Hybrid**: closed-form (A) as the always-on predictor with a learned residual correction (B-lite) layered on top, retrained nightly. This keeps sub-µJ evaluation, preserves interpretability, and absorbs the nonlinearities (AVX-512 downclock, CXL contention) that Kingman cannot — at the cost of one extra ~2-month increment for the residual learner.

---

## P-M-04: (ε, δ) Propagation through the DAG

### Candidate Solution A: Union bound
- **Approach**: Compose k independent estimators; total δ = Σ δ_i, total ε bounded by worst child plus the linear ε propagation. Straightforward.
- **Performance**: O(1) composition cost; trivially parallel.
- **Time to implement**: ~0.5 month — it is essentially bookkeeping.
- **Energy cost**: ~0 (a few additions).
- **Upside**: Distribution-free, trivially correct, no independence sampling needed beyond the children's assumptions.
- **Downside**: Extremely loose — δ grows linearly so a 100-node DAG needs each child at δ/100, exploding sample sizes ~100×. Pathological for deep DAGs.
- **Key paper**: Hoeffding 1963 (*JASA*); standard probability union bound.

### Candidate Solution B: McDiarmid's bounded-differences inequality
- **Approach**: Treat the whole DAG as a function f of independent samples; if changing one sample moves f by at most c_i, then Pr[|f−Ef| ≥ t] ≤ 2 exp(−2t²/Σc_i²). Composes multiplicatively-ish rather than additively.
- **Performance**: O(DAG size) to bound; still constant overhead.
- **Time to implement**: ~1.5 months (need per-operator sensitivity c_i analysis for every kernel in the table).
- **Energy cost**: ~0 marginal.
- **Upside**: Far tighter than union bound for DAGs with many small contributions (Σc_i² grows slowly); no independence-of-estimators requirement, only independence of input samples.
- **Downside**: Requires bounding the Lipschitz constant of every operator — painful for joins/sketches; can still be loose for heavy-tailed data.
- **Key paper**: McDiarmid 1989 (*Surveys in Combinatorics*); Hoeffding 1963.

### Candidate Solution C: Bayesian posterior propagation
- **Approach**: Each estimator emits a posterior over the true value; compose via the DAG as a probabilistic program giving a joint posterior and credible intervals.
- **Performance**: Posterior composition needs MCMC/VI — ms to seconds; too slow for per-query unless using conjugate updates.
- **Time to implement**: ~5+ months (probabilistic-programming integration).
- **Energy cost**: Orders of magnitude higher — 1–100 mJ per composition vs. ~nJ.
- **Upside**: Tightest intervals; naturally incorporates priors and per-tier noise models.
- **Downside**: Inference cost dominates for short queries; priors are a footgun; hard to give PAC-style guarantees.
- **Key paper**: Ghahramani 2015 (*Nature*); sequential Monte Carlo in databases.

### Recommendation
**McDiarmid (B) as the default**, with per-kernel sensitivity constants precomputed once for each entry in the AVX-512 kernel table. Fall back to union bound (A) for the ~5% of operators where the Lipschitz bound is intractable. Reserve Bayesian (C) for an offline `EXPLAIN APPROXIMATE` "tight bounds" mode where the user explicitly opts into seconds-scale analysis.

---

## P-M-05: AGM Worst-case-optimal Joins

### Candidate Solution A: Leapfrog triejoin
- **Approach**: For each attribute in turn, intersect sorted iterators of matching tuples using leapfrog-search; complexity O(#output + IN^{ρ*}), ρ* the fractional edge cover — worst-case optimal up to log.
- **Performance**: Veldhuizen reports leapfrog "dramatically better than NPRR" on cyclic queries; on triangle queries ~2–5× faster than binary-join plans. The log-factor gap is small in practice.
- **Time to implement**: ~3 months (needs trie/sorted indexes per attribute, iterator framework).
- **Energy cost**: Cache-friendly sequential intersection; ~1–3 nJ per output tuple (L3-resident). AVX-512 VPOPCNTDQ/Galloping hybrid lowers constant.
- **Upside**: Simple to implement, strong cyclic-query performance, already industrialized at RelationalAI.
- **Downside**: Requires trie/sorted indexes on every join attribute; loses to binary hash joins on acyclic queries by 1.5–3×.
- **Key paper**: Veldhuizen, *ICDT* 2014 ("Leapfrog Triejoin").

### Candidate Solution B: Minesweeper (NPRR-style)
- **Approach**: Recursive jump-search that finds "empty boxes" in the join hypergraph and skips them; AGM-optimal without log factor.
- **Performance**: Worst-case optimal without the log factor; in practice higher constants than leapfrog due to recursive calls.
- **Time to implement**: ~5 months (more complex recursion + box maintenance).
- **Energy cost**: ~1.5–2× leapfrog (more indirect memory accesses, recursion overhead).
- **Upside**: Removes leapfrog's log factor — matters when IN is huge and ρ* large.
- **Downside**: Higher constants; harder to tune; rarely wins except on pathological dense cyclic data.
- **Key paper**: Ngo-Ré-Rudra 2014 (*PODS*); Atserias-Grohe-Marx 2008 (*JACM* 2013, "Size bounds and query plans").

### Candidate Solution C: Recursive (binary) joins with worst-case-aware ordering
- **Approach**: Standard binary joins but with AGM-aware plan enumeration (pick sub-plan minimizing AGM bound at each step).
- **Performance**: Not worst-case optimal; on triangles O(N²) vs leapfrog's O(N^{1.5}). Loses badly on cyclic.
- **Time to implement**: ~1.5 months (reuse existing join infra).
- **Energy cost**: Highly variable; on acyclic queries the cheapest, on cyclic queries the most expensive.
- **Upside**: Reuses the existing binary-join engine; wins on acyclic star/snowflake queries.
- **Downside**: Catastrophic on cyclic queries — defeats the purpose.
- **Key paper**: Atserias-Grohe-Marx 2008.

### Recommendation
**Leapfrog (A) for cyclic / many-to-many joins, falling back to binary (C) for acyclic queries** chosen by the cost model (P-M-03). Minesweeper (B) only if a workload is dominated by dense ρ* ≥ 2 cyclic joins where the log factor matters — otherwise the implementation/energy cost is not repaid.

---

## P-M-06: Tensor Train for Multi-column Compression

### Candidate Solution A: TT-SVD
- **Approach**: Repeated SVD unfolding along each mode to produce a low-rank TT decomposition of the joint distribution of correlated columns; store cores.
- **Performance**: O(n d r³) for n rows, d columns, TT-rank r. Oseledets reports 10⁵–10⁶× compression on smooth high-order tensors with r ≪ n; for typical 8-column groups r ~ 5–20.
- **Time to implement**: ~3 months (SVD harness, row-reconstruction for predicate pushdown).
- **Energy cost**: Decomposition: ~50–200 mJ per column-group (dominated by SVD); reconstruction per predicate ~1–5 nJ.
- **Upside**: Provably avoids the curse of dimensionality; excellent on smoothly-correlated columns (timestamps+IDs).
- **Downside**: Decomposition is offline-only; random/sparse columns give r ≈ n (no compression); predicate evaluation needs TT-tensor contraction.
- **Key paper**: Oseledets, *SIAM J. Sci. Comput.* 33(5), 2011 ("Tensor-Train Decomposition").

### Candidate Solution B: TT-Cross (interpolative)
- **Approach**: Approximate TT-SVD using only O(n r d) entries via skeleton/interpolative decomposition (TT-Cross); no full materialization needed.
- **Performance**: O(n r d) vs O(n d r³); ~10–100× faster decomposition than TT-SVD at modest accuracy loss.
- **Time to implement**: ~4 months (maxvol subroutines, pivoting heuristics).
- **Energy cost**: ~10–50× cheaper decomposition than TT-SVD — ~5–20 mJ per group.
- **Upside**: Streaming-friendly; works on data that doesn't fit in memory; online updates possible.
- **Downside**: Pivoting is fragile on degenerate columns; accuracy depends on the pivot heuristic.
- **Key paper**: Oseledets-Tyrtyshnikov, *Russian J. Numer. Anal. Math. Modelling* 2010; Savostyanov-Oseledets 2011.

### Candidate Solution C: Randomized TT
- **Approach**: Random projection + single-pass sketching to estimate TT cores in one streaming pass.
- **Performance**: O(nnz) single pass; approximate cores.
- **Time to implement**: ~3.5 months (random-projection matrices, sketch aggregation).
- **Energy cost**: Lowest decomposition cost (~1–5 mJ per group), but reconstruction accuracy is the worst.
- **Upside**: True one-pass streaming, ideal for CXL/NVMe-resident data.
- **Downside**: Weakest accuracy; needs careful tuning of the oversampling parameter.
- **Key paper**: Huber et al., *arXiv* 2017 ("Randomized tensor train decomposition"); Tropp et al. on randomized SVD.

### Recommendation
**TT-SVD (A) for static, hot, smoothly-correlated column groups** (the offline `ANALYZE` path); **TT-Cross (B) for streaming ingestion** of correlated columns that won't fit in L3/DDR5. Skip randomized (C) unless NVMe-resident one-pass compression becomes a stated goal — its accuracy loss on real DB columns rarely justifies the energy savings.

---

## P-M-07: Universal Source Coding for Schema-on-Read

### Candidate Solution A: LZ4 (LZ77-family)
- **Approach**: Sliding-window LZ77 with small literal/match costs; universal for any stationary ergodic source, converges to entropy (Lempel-Ziv 1977).
- **Performance**: LZ4 decodes at ~4 GB/s/core (single-threaded, SIMD); FSE-based zstd ~1.5 GB/s with better ratio.
- **Time to implement**: ~1 month (call into lz4/lz4flex).
- **Energy cost**: ~5–10 nJ/byte decode (RAPL-measured class for LZ4).
- **Upside**: Universal (no model needed), extremely fast, schema-on-read trivially — just decompress the bytes.
- **Downside**: ~2× worse ratio than rANS on skewed distributions; no random access without block indexing.
- **Key paper**: Ziv-Lempel, *IEEE Trans. Inf. Theory* 23(3), 1977.

### Candidate Solution B: rANS (range Asymmetric Numeral Systems)
- **Approach**: Static or semi-adaptive rANS with per-block frequency tables; entropy approaches H for known distributions; near-arithmetic-coding ratio at LZ-like speed.
- **Performance**: rANS decode ~1–2 GB/s/core (vectorized, interleaved); ratio within 1% of entropy. Universal when paired with adaptive tables.
- **Time to implement**: ~2.5 months (table learning, vectorized decoder, FSE-style state machine).
- **Energy cost**: ~8–15 nJ/byte decode — slightly worse than LZ4 per byte, but ~30–50% fewer bytes from DRAM/CXL, so net energy lower for memory-bound scans.
- **Upside**: Best ratio among practical fast coders; excellent for tier-resident cold data.
- **Downside**: Static tables need an `ANALYZE` pass; worse on highly non-stationary data than adaptive LZ.
- **Key paper**: Duda 2009 (arXiv:1311.2540); Andrews 2013 on rANS.

### Candidate Solution C: Context-Tree Weighting (CTW)
- **Approach**: Maintain a context tree of depth D; weight all context models; provably universal and minimax-optimal for finite-context sources.
- **Performance**: CTW is ~100–1000× slower than LZ4 (~1–10 MB/s); mainly of theoretical interest at DB scale.
- **Time to implement**: ~5 months and still too slow.
- **Energy cost**: ~1–5 µJ/byte — 100× LZ4.
- **Upside**: Provably optimal redundancy O(1) on finite-state sources; the gold standard for unknown-context data.
- **Downside**: Far too slow for online DB use; memory footprint explodes with context depth.
- **Key paper**: Willems-Shtarkov-Tjalkens, *IEEE Trans. Inf. Theory* 41(3), 1995.

### Recommendation
**rANS (B) for cold tier-resident data (CXL/NVMe)** where ratio dominates, **LZ4 (A) for hot L3/DDR5 data** where decode speed dominates. CTW (C) is academically interesting but not viable at DB throughput. The choice per column is driven by the `TIER` hint and the energy model.

---

## P-M-08: Online Tier Replacement (k-server)

### Candidate Solution A: LRU with hysteresis
- **Approach**: Track recency per (page, tier); evict the least-recently-used from the faster tier to the slower; add hysteresis to avoid thrash.
- **Performance**: O(1) per access; competitive ratio k for the k-server problem (Sleator-Tarjan 1985) — i.e. k× optimal where k = number of tiers served.
- **Time to implement**: ~1 month.
- **Energy cost**: ~minimal (a few ns of bookkeeping); ~0.1 nJ/access.
- **Upside**: Trivial, robust, cache-friendly; well understood.
- **Downside**: k-competitive is loose (4× for 4 tiers); blind to access-pattern locality beyond recency.
- **Key paper**: Sleator-Tarjan, *JACM* 32(1), 1985.

### Candidate Solution B: Work Function Algorithm (WFA)
- **Approach**: For each request, move the configuration minimizing the work function w_t(C) = min cost to serve the request sequence ending in C. Provably (2k−1)-competitive (Koutsoupias-Papadimitriou 1995).
- **Performance**: Naive WFA is O(k²) per request — prohibitive for k=4 tiers but ok; with caching of work-function values ~O(k) amortized.
- **Time to implement**: ~3 months (work-function table, efficient recomputation).
- **Energy cost**: ~5–20× LRU per access (~0.5–2 nJ) due to recomputation.
- **Upside**: Provably near-optimal; the best known deterministic guarantee.
- **Downside**: Implementation complexity; constants make it lose to LRU except on adversarial access patterns.
- **Key paper**: Koutsoupias-Papadimitriou, *JACM* 42(5), 1995.

### Candidate Solution C: Learned replacement policy
- **Approach**: Train a small neural/GBM model to predict next-access time per page; evict furthest-future (Belady-optimal in expectation).
- **Performance**: Inference ~10–100 ns/page; offline-trained, online-fine-tuned. Can match or beat LRU on real traces.
- **Time to implement**: ~4 months (trace collection, training, online inference path, fallback).
- **Energy cost**: ~10–50 nJ/page for inference — high but amortized over many accesses.
- **Upside**: Adapts to workload; can beat (2k−1) on real data.
- **Downside**: Cold start, drift, opaque, added inference latency on the hot path.
- **Key paper**: Beckmann et al. (PARROT); Lykouris-Vassilvitskii 2018 (competitive caching with ML advice).

### Recommendation
**LRU with hysteresis (A) as the always-on baseline; WFA (B) reserved for the topmost tier (L3↔DDR5) where k=2 makes WFA cheap and (2k−1)=3-competitive**. Skip learned (C) unless telemetry shows LRU thrashing on a stable workload — its inference energy is rarely repaid in a general-purpose engine.

---

## P-M-09: Functorial Schema Migration Correctness

### Candidate Solution A: CQL (Categorical Query Language) embedding
- **Approach**: Model the schema as a category and migrations as the three adjoint functors Σ ⊣ Δ ⊣ Π (left adjoint "existential", middle "schema restriction", right adjoint "universal"). CQL provides a reference implementation; correctness follows from adjunction laws.
- **Performance**: Δ and Σ are roughly relational-algebra cost; Π requires co-limit computation that can be exponential on cyclic schemas (Schultz-Spivak).
- **Time to implement**: ~5 months (embed/interoperate with the CQL IDE; map CQL plans to your kernel table).
- **Energy cost**: Modest per migration except Π; ~mJ-scale for typical schemas.
- **Upside**: Mathematical correctness guarantee; migration is provably lossless/lossy in precisely-characterized ways; bidirectional round-tripping.
- **Downside**: Π on cyclic schemas is computationally explosive; category-theory learning curve for the team; tooling immaturity.
- **Key paper**: Spivak, *Inf. & Comp.* 2012 ("Functorial data migration"); Schultz-Spivak, *LICS* 2016.

### Candidate Solution B: Custom DSL with algebraic laws
- **Approach**: Define a small migration DSL with explicit Σ/Δ/Π-like operators but without full categorical generality; prove the laws you need (e.g., ΔΣ ⊣, round-tripping) using Lean/Coq.
- **Performance**: Tuned to your engine; can avoid Π's exponential cases by construction.
- **Time to implement**: ~6 months (DSL design + mechanized proofs + codegen).
- **Energy cost**: Lowest — only the cases you support are compiled.
- **Upside**: Pays for exactly the migrations you need; proof obligations are explicit.
- **Downside**: Cannot reuse CQL tooling; the team must own the proof obligations forever.
- **Key paper**: Spivak-Wisnesky, *arXiv* 1212.5303, 2012 (FQL relational foundations).

### Candidate Solution C: SQL DDL compilation with translation validation
- **Approach**: Keep migrations in SQL DDL but run a translation validator (equivalence checker) over the source/target schemas at compile time.
- **Performance**: Validation cost is the bottleneck — SMT-based equivalence can be seconds to minutes per migration.
- **Time to implement**: ~3 months if a capable validator exists.
- **Energy cost**: Validation is offline; runtime energy is just the compiled DDL.
- **Upside**: Familiar SQL interface; no category theory for users.
- **Downside**: Validator only checks the migrations it can express; no a-priori correctness class; cannot express Π-style universal migrations.
- **Key paper**: Su et al. on translation validation; Eisenberg/VLDB-style view-equivalence checking.

### Recommendation
**Custom DSL (B) for the migration primitives, with CQL (A) used as the reference oracle in tests.** This gets the correctness guarantee where it matters (the engine's own migrations) without paying CQL's Π-cost on cyclic schemas in production. SQL DDL compilation (C) is the user-facing syntax, validated against (B).

---

## P-M-10: Sheaf-theoretic Distributed Consistency

### Candidate Solution A: Sheaf model with consistency radius
- **Approach**: Model replicas as a sheaf over the network topology; local sections are replica states; the gluing condition is consistency. Use Robinson's *consistency radius* to quantify global inconsistency and drive repair.
- **Performance**: Consistency-radius computation is O(edges × local-dims); per heartbeat ~ms-scale for 100s of replicas.
- **Time to implement**: ~6 months (sheaf representation, radius computation, repair scheduler).
- **Energy cost**: ~mJ-scale per heartbeat; repair cost depends on radius magnitude.
- **Upside**: Unified continuous measure of inconsistency (not just strong/eventual); subsumes CRDT-style eventual and quorum-style strong as special sheaves.
- **Downside**: Conceptual complexity; team needs category-theory literacy; repair policy is underspecified by the model.
- **Key paper**: Robinson 2017 ("Sheaves are the canonical datastructure for sensor integration"); Mac Lane-Moerdijk 1992 (*Sheaves in Geometry and Logic*).

### Candidate Solution B: Cosheaf for failure-tolerant aggregation
- **Approach**: Use a cosheaf (dual construction) so that local computations push forward to a global value, tolerating node loss by functoriality.
- **Performance**: Aggregation cost = sum of local cosheaf functor applications; very cheap.
- **Time to implement**: ~4 months (cosheaf for the limited set of aggregate operators).
- **Energy cost**: Lowest — pure local computation.
- **Upside**: Natural for aggregations (count, sum, sketch-merge) that must tolerate failures; explicit gluing.
- **Downside**: Doesn't directly model arbitrary replica state; weaker than the full sheaf model.
- **Key paper**: Curry 2014 ("Sheaves, cosheaves and applications", PhD UC Northridge).

### Candidate Solution C: Homotopy-type / simplicial consistency
- **Approach**: Model the system state as a simplicial set; consistency = the existence of a filler for every horn. This captures multi-party consistency at all dimensions.
- **Performance**: Horn-filling is NP-hard in general; only feasible for low dimensions and small replica sets.
- **Time to implement**: ~9+ months — research-grade.
- **Energy cost**: Prohibitive at scale (combinatorial blowup).
- **Upside**: Captures n-party consensus (Herlihy-Shavit style) in one framework.
- **Downside**: Impractical at DB scale; mainly a theoretical lens.
- **Key paper**: Herlihy-Shavit 1999 (topological structure of wait-free computation); Spivak, *dlib* 2009.

### Recommendation
**Cosheaf (B) for aggregations and sketch merges — it is cheap, correct, and failure-tolerant.** Use the full sheaf model (A) with consistency radius as the *monitoring and repair* layer for the protocol coordinator (mapping `CONSISTENCY` hints to a target radius). Skip homotopy-type (C) — it informs the theory but is not deployable.

---

## P-M-11: PAC Guarantees for Approximate SQL

### Candidate Solution A: Hoeffding-based guarantees
- **Approach**: For bounded aggregates, sample n iid rows; Hoeffding gives Pr[|μ̂−μ| ≥ ε] ≤ 2 exp(−2nε²/(b−a)²). Invert for n = ln(2/δ)/(2ε²)·(b−a)².
- **Performance**: n ~ 10⁴ for ε=1%, δ=1% on bounded data — milliseconds per aggregate.
- **Time to implement**: ~1 month.
- **Energy cost**: ~μJ per aggregate (dominated by the scan of n samples).
- **Upside**: Distribution-free, dead simple, tight on bounded data.
- **Downside**: Range-dependent (b−a)² blows up sample size on wide-range columns; ignores variance.
- **Key paper**: Hoeffding 1963; Valiant 1984 (*TCS*, "A theory of the learnable").

### Candidate Solution B: Bernstein (variance-aware)
- **Approach**: Use Bernstein's inequality n ~ (σ²/ε²) ln(2/δ) + ln(2/δ)/(3ε); uses empirical variance σ².
- **Performance**: For low-variance columns ~10–100× fewer samples than Hoeffding.
- **Time to implement**: ~1.5 months (need online variance estimate).
- **Energy cost**: ~10–100× lower than Hoeffding on low-variance data.
- **Upside**: Dramatically tighter on low-variance data; matches Hoeffding worst case.
- **Downside**: Variance estimate itself is noisy at small n; needs sequential stopping.
- **Key paper**: Maurer-Pontil 2009 (*COLT*, empirical Bernstein); Bernstein 1927.

### Candidate Solution C: Empirical Bernstein with sequential stopping
- **Approach**: Combine empirical-Bernstein bounds with a sequential probability ratio test to stop sampling as soon as the bound is met.
- **Performance**: Stops at the *actual* required n, often 2–10× fewer than fixed-n Bernstein.
- **Time to implement**: ~2.5 months (sequential stopping logic, audit trail for the (ε,δ) guarantee).
- **Energy cost**: Best — stops early, ~3–10× lower than fixed-n.
- **Upside**: Tightest practical bound; defensible (ε,δ) at query time; naturally supports `APPROXIMATE WITHIN ε CONFIDENCE 1−δ`.
- **Downside**: More complex; must keep a per-query audit log proving the bound held.
- **Key paper**: Maurer-Pontil 2009; Audibert-Munos-Szepesvári 2007 (empirical Bernstein with exploration).

### Recommendation
**Empirical Bernstein with sequential stopping (C) as the default `APPROXIMATE` engine**, because it gives the tightest defensible (ε,δ) at the lowest energy. Fall back to Hoeffding (A) when variance estimation is itself untrustworthy (tiny samples, heavy tails). This directly powers query-syntax problem Q-1.

---

## P-M-12: Concentration Bounds for Sketch Composition

### Candidate Solution A: McDiarmid (bounded differences over the stream)
- **Approach**: Composing HLL+CM treats the merged sketch as f(stream); McDiarmid bounds deviation by Σc_i² where c_i is the max change from one stream element.
- **Performance**: O(1) to evaluate the bound after composition.
- **Time to implement**: ~1.5 months (need per-sketch sensitivity constants).
- **Energy cost**: Negligible (constant-time bound evaluation).
- **Upside**: Composes sketches without inflating δ linearly; works for mergeable sketches generally.
- **Downside**: c_i for CM can be large (max counter increment), making the bound loose for heavy-hitter streams.
- **Key paper**: McDiarmid 1989; Cormode-Muthukrishnan, *J. Alg.* 2005 (Count-Min sketch).

### Candidate Solution B: Per-sketch worst-case error composition
- **Approach**: Compose each sketch's own published error guarantee additively: CM error ε₁ + ε₂, HLL relative error sqrt(σ₁² + σ₂²) under independent merges; δ sums.
- **Performance**: O(1).
- **Time to implement**: ~0.5 month.
- **Energy cost**: Negligible.
- **Upside**: Uses the sketches' native, tight guarantees; well-understood.
- **Downside**: δ grows linearly with composition depth; assumes merge independence which fails under correlated streams.
- **Key paper**: Flajolet et al. 2007 (HLL concentration); Cormode-Muthukrishnan 2005.

### Candidate Solution C: Worst-case additive (max over children)
- **Approach**: Take the maximum error among children; δ = max δ_i.
- **Performance**: O(children).
- **Time to implement**: ~0.25 month.
- **Energy cost**: Negligible.
- **Upside**: δ does not grow — simplest correct composition for union/aggregation semantics.
- **Downside**: ε is the worst child's, so for unions of many small sketches the bound is wildly loose.
- **Key paper**: Standard probability union/maximum arguments.

### Recommendation
**Per-sketch worst-case composition (B) for the common case** (it uses the sketches' native, tight bounds), **McDiarmid (A) for deep DAG compositions** where linear δ-growth becomes unacceptable. Max-over-children (C) is a cheap fallback for unions of identically-parameterized sketches.

---

## P-M-13: LP Relaxation for Join Ordering

### Candidate Solution A: LP relaxation + rounding
- **Approach**: Formulate the join ordering as an integer program over connected subplans; relax to LP; round with a randomized/LP-guided heuristic.
- **Performance**: For n>15 joins, LP with O(n²) vars solves in tens of ms with HiGHS/Gurobi; rounding produces near-optimal plans (Steinke-Nutt 2004 report <5% gap).
- **Time to implement**: ~3 months (IP formulation, LP integration, rounding).
- **Energy cost**: ~10–100 mJ per query (LP at this scale); 100–1000× greedy.
- **Upside**: Handles n>15 where DP (Selinger) is O(3ⁿ) infeasible; provable gap.
- **Downside**: Energy cost is high; LP solver is a heavy dependency; rounding can violate constraints.
- **Key paper**: Raghavan-Thompson 1987; Steinke-Nutt 2004 (LP-based join enumeration).

### Candidate Solution B: SDP (Goemans-Williamson style)
- **Approach**: Lift to a semidefinite program for tighter relaxations; round via hyperplane rounding.
- **Performance**: SDP is O(n³) to O(n⁶) — practical only to n ~ 30–50; gap ~0.878 for max-cut, less crisp for join trees.
- **Time to implement**: ~5 months (SDP solver integration; rounding).
- **Energy cost**: ~10–100× LP — prohibitive per query; offline-only.
- **Upside**: Tightest known relaxation in many cases.
- **Downside**: Far too slow/energy-heavy for online; gap not characterized for join ordering specifically.
- **Key paper**: Goemans-Williamson, *JACM* 1995.

### Candidate Solution C: Greedy / randomized
- **Approach**: Greedy pick the lowest-cost join iteratively; boost with randomized restarts (GOO / IKKBZ-style).
- **Performance**: O(n²) per restart; IKKBZ gives a provably 1-dominating tree for acyclic queries.
- **Time to implement**: ~1 month.
- **Energy cost**: ~µJ per query — 1000× cheaper than LP.
- **Upside**: Cheap, robust, fast; good enough for most real workloads.
- **Downside**: No provable optimality gap; loses 10–30% on dense cyclic queries.
- **Key paper**: IKKBZ (Krishnamurthy et al. 1986); Neumann-Kemper (Hyperjoin, *BTW* 2015).

### Recommendation
**Greedy (C) as the online default; promote to LP+rounding (A) for queries with >15 joins (the regime the problem names)** — at that size LP's tens-of-ms cost is acceptable relative to the query. SDP (B) only as an offline "explain and re-optimize" tool for the few hottest mega-join templates.

---

## P-M-14: Submodular Index Selection

### Candidate Solution A: Greedy (1 − 1/e)
- **Approach**: Index benefit is submodular (diminishing returns); greedy adds the index with max marginal benefit each step → (1−1/e) ≈ 0.63 approximation in O(k·n) evaluations.
- **Performance**: For k=10 indexes over n=1000 candidates: ~10⁴ cost-model evaluations, ~seconds.
- **Time to implement**: ~2 months (cost model calls + memoization).
- **Energy cost**: ~mJ-scale per evaluation × 10⁴ ≈ ~10 mJ per index-set decision.
- **Upside**: Provably near-optimal; trivially parallel; works with any (even learned) cost model.
- **Downside**: 0.63 gap is loose; can be beaten by 10–20% with continuous methods.
- **Key paper**: Nemhauser-Wolsey-Fisher, *Math. Programming* 1978; Krause-Guestrin, *JMLR* 2008.

### Candidate Solution B: Continuous submodular optimization
- **Approach**: Relax to a continuous submodular function and run Frank-Wolfe / lazy greedy OOO; achieves (1−1/e) with continuous interpolation and often tighter in practice.
- **Performance**: O(iterations × eval) — converges in ~10–100 iterations.
- **Time to implement**: ~3.5 months (continuous relaxation + FW).
- **Energy cost**: ~2–3× greedy.
- **Upside**: Supports continuous "fractional index" budgets, smoother tradeoffs; better on real workloads by ~5–15%.
- **Downside**: Relaxation can be non-trivial; FW convergence tuning.
- **Key paper**: Bach 2013 (continuous submodular); Calinescu et al. 2011.

### Candidate Solution C: LP for submodular maximization
- **Approach**: Multilinear extension + pipage rounding using an LP relaxation.
- **Performance**: O(n³) LP — much slower than greedy.
- **Time to implement**: ~5 months.
- **Energy cost**: ~10–50× greedy.
- **Upside**: Same (1−1/e) guarantee but handles matroid/knapsack constraints natively.
- **Downside**: Energy and engineering cost; rarely beats (B) on practical instances.
- **Key paper**: Calinescu-Chekuri-Pál-Vondrák, *Math. Oper. Res.* 2011.

### Recommendation
**Greedy (A) as the default**, upgraded to continuous submodular Frank-Wolfe (B) when the index budget is a hard constraint (`MEMORY BUDGET`) that greedy over- or under-shoots. LP (C) only for offline what-if analysis. This pairs naturally with the `MEMORY BUDGET` hint (Q-7).

---

## P-M-15: Linear-logic Type System for CXL Refs

### Candidate Solution A: Rust-style affine types
- **Approach**: Encode CXL memory references as affine (use-once) types enforced by the borrow checker; a CXL handle can be used but not duplicated, guaranteeing it cannot escape its scope. Use Rust or a Rust-front-end DSL for the kernel table.
- **Performance**: Zero runtime cost — purely compile-time.
- **Time to implement**: ~3 months (type encoding, lifetime annotations, FFI to the C++ engine).
- **Energy cost**: Zero runtime; compile-time only.
- **Upside**: Battle-tested; no runtime overhead; catches escapes statically.
- **Downside**: Rust integration friction; lifetime annotations get viral; some kernel patterns need `unsafe`.
- **Key paper**: Walker 2005 ("Substructural type systems"); Girard 1987 ("Linear logic").

### Candidate Solution B: External checker (separate static analysis)
- **Approach**: Keep the engine in C++ but run a separate flow-sensitive analysis over the kernel IR that flags CXL-handle escapes.
- **Performance**: Compile-time only; seconds per kernel.
- **Time to implement**: ~4 months (IR design, abstract-interpretation checker).
- **Energy cost**: Zero runtime.
- **Upside**: Works with existing C++ code; no language migration; can express engine-specific policies (e.g., "CXL ref may not be returned from a `SCOPE RACK`-bounded function").
- **Downside**: Two systems to keep in sync; checker can be unsound on unsafe pointer arithmetic.
- **Key paper**: Girard 1987; Kobayashi-Pierce-Turner 1996 (linear typing for linearity).

### Candidate Solution C: Refinement types (LiquidHaskell-style)
- **Approach**: Annotate types with refinement predicates (e.g., `{r : CXLRef | scope r = LOCAL}`) checked by an SMT solver.
- **Performance**: Compile-time SMT checks, seconds to minutes.
- **Time to implement**: ~6 months (predicates, SMT integration, annotation burden).
- **Energy cost**: Zero runtime.
- **Upside**: Most expressive — can encode tier, scope, and energy-budget refinements in one framework.
- **Downside**: Highest annotation burden; SMT timeouts; steepest learning curve.
- **Key paper**: Vazou et al. 2014 (LiquidHaskell); Freeman-Pfenning 1991 (refinement types).

### Recommendation
**Affine Rust types (A) for the kernel-table authoring path**, because the zero-runtime-cost safety is exactly what CXL (which has weaker-than-DRAM persistence and caching semantics) demands. Use external checker (B) as a belt-and-suspenders audit over the compiled kernel IR. Refinement types (C) are aspirational — revisit only if tier/scope refinements need to be first-class in the query language.

---

# PART B — QUERY SYNTAX PROBLEMS

## P-Q-01: Approximate Queries with (ε, δ)

`SELECT APPROXIMATE WITHIN ε CONFIDENCE 1−δ COUNT(DISTINCT …)`

### Candidate Solution A: Hoeffding-based sampling
- **Approach**: Random row sampling with Hoeffding's bound; n = ln(2/δ)/(2ε²) samples.
- **Performance**: For ε=1%, δ=1%: n ≈ 26 500 rows; ~ms for a count. Independent of table size.
- **Time to implement**: ~1.5 months.
- **Energy cost**: ~μJ per query (scan cost ∝ n, not N).
- **Upside**: Distribution-free; trivial to reason about; powers `APPROXIMATE WITHIN`.
- **Downside**: Range-dependent; ignores variance; loose for low-variance data.
- **Key paper**: Hellerstein et al., SIGMOD 1997 ("Online Aggregation"); Hoeffding 1963.

### Candidate Solution B: Sketch-based (HLL / CM)
- **Approach**: Maintain a precomputed HLL (count-distinct) or CM (frequencies); answer from the sketch with its native error guarantee (HLL ~0.81% at 12 KB).
- **Performance**: O(1) per query (sketch lookup); sub-ms; independent of table size entirely.
- **Time to implement**: ~2 months (sketch maintenance on ingest).
- **Energy cost**: ~nJ per query (sketch is L3-resident); near-zero marginal.
- **Upside**: Best latency and energy; supports set-union (mergeable).
- **Downside**: Sketch must be maintained; (ε,δ) is the sketch's fixed guarantee, not user-chosen.
- **Key paper**: Cormode-Garofalakis 2008; Flajolet et al. 2007 (HLL).

### Candidate Solution C: Sequential empirical-Bernstein online aggregation
- **Approach**: Stream rows; stop as soon as the empirical-Bernstein (ε,δ) bound is satisfied. Interactive progress bar à la Hellerstein.
- **Performance**: Stops at the *actual* required n; often 2–10× fewer samples than Hoeffding.
- **Time to implement**: ~2.5 months (sequential stopping, audit trail).
- **Energy cost**: ~3–10× lower than (A); comparable to (B) but with user-chosen (ε,δ).
- **Upside**: Tightest user-tunable bound; interactive; defensible PAC guarantee.
- **Downside**: More complex; needs audit log.
- **Key paper**: Maurer-Pontil 2009; Hellerstein 1997.

### Recommendation
**Sketch (B) when the query maps to a maintained sketch** (count-distinct, heavy hitters); otherwise **sequential empirical-Bernstein (C)**, which is the natural runtime for `APPROXIMATE WITHIN ε CONFIDENCE 1−δ`. Hoeffding (A) is the documented fallback for cold paths. This directly uses P-M-11.

---

## P-Q-02: Tier Hints (`TIER L3` / `TIER CXL`)

### Candidate Solution A: Hard constraint
- **Approach**: `TIER L3` is a hard requirement — the planner errors out if the working set cannot fit the chosen tier.
- **Performance**: Plan-time check; runtime unchanged.
- **Time to implement**: ~1 month.
- **Energy cost**: Zero marginal.
- **Upside**: Predictable; honors user intent exactly.
- **Downside**: Brittle — spills under load cause hard failures; no graceful degradation.
- **Key paper**: ClickHouse MergeTree `min_bytes_for_wide_part` / `max_parts_in_total` settings (ClickHouse docs).

### Candidate Solution B: Soft hint with cost-model override
- **Approach**: `TIER CXL` is a preference; the cost model (P-M-03) may override if a different tier is provably cheaper within an energy/latency slack.
- **Performance**: One extra cost-model evaluation per candidate tier.
- **Time to implement**: ~2 months.
- **Energy cost**: ~µJ per plan.
- **Upside**: Robust to spills; honors intent when feasible.
- **Downside**: User loses predictability; "why didn't my hint take?" debugging burden.
- **Key paper**: ClickHouse settings; MySQL `MEMORY`/`INMEMORY` hints.

### Candidate Solution C: Fully automatic tiering
- **Approach**: No hint; the engine's tier-replacement policy (P-M-08) chooses. `TIER` keyword is documented but ignored.
- **Performance**: Optimal-in-expectation via WFA/LRU.
- **Time to implement**: ~0 months (already built).
- **Energy cost**: Zero marginal.
- **Upside**: Zero user burden.
- **Downside**: No escape hatch for expert users; debugging is opaque.
- **Key paper**: Mesa / Bigtable tiering papers.

### Recommendation
**Soft hint (B) as the default semantics**, with `TIER L3 STRICT` (suffix) opting into hard constraint (A). This matches the engine's instruction-first philosophy: hints guide the planner but the cost model remains authoritative. Fully-automatic (C) is the implicit behavior when no hint is given.

---

## P-Q-03: Similarity Search and Joins (`SIMILAR TO … WITHIN HAMMING DISTANCE k`)

### Candidate Solution A: Brute-force AVX-512 VPOPCNTDQ
- **Approach**: Compute Hamming distance via `VPOPCNTDQ` (64 popcounts/instruction, 1/cycle throughput on ICL+) over all candidates; filter by k.
- **Performance**: ~64-byte vectors, ~1 cycle/element → ~10⁹ distances/sec/core; for 10⁶ candidates ~1 ms/core.
- **Time to implement**: ~1.5 months.
- **Energy cost**: ~0.2 nJ per 64-bit Hamming distance (L3-resident); ~0.2 mJ for a 10⁶-candidate scan.
- **Upside**: Exact; trivially correct; no index; AVX-512-native.
- **Downside**: O(N) per query — loses to LSH above ~10⁷ candidates.
- **Key paper**: Intel AVX-512 VPOPCNTDQ (Intel SDM); Broder 1997 (resemblance/min-hash).

### Candidate Solution B: Locality-sensitive hashing (LSH)
- **Approach**: Build k-bit LSH buckets; query only candidates in colliding buckets. Hamming LSH gives O(N^{1/c}) for c-approximate near neighbor.
- **Performance**: For c=2, query time ~N^{0.5} — ~10³ speedup over brute at N=10⁶.
- **Time to implement**: ~3 months (LSH family, bucketing, recall tuning).
- **Energy cost**: ~10–100× lower than brute at scale; dominated by bucket lookups.
- **Upside**: Theoretically near-optimal (Andoni-Indyk); scales to billions.
- **Downside**: Approximate (false negatives); index build cost; parameter tuning per workload.
- **Key paper**: Andoni-Indyk, *Comm. ACM* 51(1), 2008; Broder 1997.

### Candidate Solution C: Sketch-based (min-hash / SimHash)
- **Approach**: Precompute b-bit SimHash/min-hash signatures; only compute true distance on signatures within b·k bits. Two-stage filter.
- **Performance**: ~10–100× fewer true-distance computations; sub-ms at 10⁷.
- **Time to implement**: ~2.5 months.
- **Energy cost**: ~5–20× lower than brute; signature lookup is L3-resident.
- **Upside**: Index is compact (b bits/doc); mergeable; well-understood recall curves.
- **Downside**: Recall ≤ 100%; tuning b vs. k vs. candidate-set size.
- **Key paper**: Charikar 2002 (SimHash); Broder 1997 (min-wise).

### Recommendation
**Brute-force VPOPCNTDQ (A) for N ≤ ~10⁶ or k very large** (where LSH buckets saturate); **LSH (B) for N > 10⁶ with moderate k**. Sketch-based (C) is a good middle ground when index storage is constrained. The planner picks based on N, k, and `MEMORY BUDGET` — brute is the always-correct fallback.

---

## P-Q-04: Consistency Level Selection (`CONSISTENCY STRONG` / `READ_COMMITTED` / `EVENTUAL`)

### Candidate Solution A: Per-query consistency
- **Approach**: Each query carries its `CONSISTENCY` level; the protocol coordinator routes to the appropriate quorum or read-replica path per query.
- **Performance**: STRONG adds 1 RTT for quorum; EVENTUAL is local-read latency.
- **Time to implement**: ~2 months (per-query routing, replica selection).
- **Energy cost**: STRONG ~2× EVENTUAL per query (network RTT energy ~µJ/RTT).
- **Upside**: Maximum flexibility; per-query cost/perf tradeoff.
- **Downside**: Reasoning about mixed consistency within a transaction is hard.
- **Key paper**: Viotti-Rupakula 2015 (consistency survey); Burrows 2006 (Chubby).

### Candidate Solution B: Per-statement (transaction-scoped)
- **Approach**: `CONSISTENCY` is declared once per transaction; all statements inherit.
- **Performance**: Same as (A) per statement; save the routing decision overhead.
- **Time to implement**: ~2.5 months (transaction context plumbing).
- **Energy cost**: Marginally lower than (A) (one routing decision per txn).
- **Upside**: Semantically clean — transactions are the unit of consistency reasoning.
- **Downside**: Less granular; can't read EVENTUAL inside a STRONG txn.
- **Key paper**: Spanner (Pang et al. 2012, *OSDI*); Viotti 2015.

### Candidate Solution C: Per-transaction with mixed-mode sub-statements
- **Approach**: Default at transaction level; statements may downgrade (not upgrade) within the transaction.
- **Performance**: Adds a consistency-ratchet check per statement.
- **Time to implement**: ~3.5 months.
- **Energy cost**: Negligible marginal.
- **Upside**: Best of both — safe by default, optimization where allowed.
- **Downside**: Ratchet semantics must be precisely documented to avoid surprises.
- **Key paper**: Bailis et al. 2013 (Bolt-on causal consistency); Kraska 2014.

### Recommendation
**Per-transaction (B) as the default, with downgrade-only sub-statement overrides (C)** — this aligns with the protocol coordinator's `SCOPE` (Q-5) and avoids the per-query reasoning hazards of (A). Spanner-style per-transaction consistency is the established industrial pattern.

---

## P-Q-05: Protocol-aware Transactions (`SCOPE RACK` / `REGION` / `GLOBAL`)

### Candidate Solution A: Static analysis
- **Approach**: At plan time, infer the minimum scope a transaction needs from its read/write set; reject or reroute if it violates the declared `SCOPE`.
- **Performance**: Plan-time only; runtime unchanged.
- **Time to implement**: ~3 months (read/write-set analysis, scope inference).
- **Energy cost**: Zero runtime.
- **Upside**: Catches scope violations before execution; enables rack-local fast paths.
- **Downside**: Imprecise analysis over-conservatively escalates to GLOBAL.
- **Key paper**: Thomson et al. 2012 (Calvin, *CIDR*); Abadi deterministic txn work.

### Candidate Solution B: Runtime detection
- **Approach**: Track the actual replicas touched; escalate scope dynamically if the touched set exceeds the declared scope.
- **Performance**: Per-access bookkeeping ~ns.
- **Time to implement**: ~2 months.
- **Energy cost**: ~nJ per access.
- **Upside**: Precise — only escalates when actually needed.
- **Downside**: Late detection — a transaction may already have started GLOBAL-incompatible work.
- **Key paper**: Calvin (Thomson 2012); FaRM (Dragojević 2014).

### Candidate Solution C: Manual annotation only
- **Approach**: Trust the user's `SCOPE`; no analysis.
- **Performance**: Zero overhead.
- **Time to implement**: ~0.5 month.
- **Energy cost**: Zero.
- **Upside**: Simplest; expert users in control.
- **Downside**: Incorrect annotation → silent correctness violations.
- **Key paper**: Calvin; Spanner's `SCOPE`-like placement groups.

### Recommendation
**Static analysis (A) at plan time with runtime detection (B) as the safety net.** Calvin's deterministic approach is the right reference: the planner proves the scope is achievable, and the runtime escalates if reality disagrees. Manual (C) is unsafe as the sole mechanism.

---

## P-Q-06: Sketch-aware Aggregations (`USING HYPERLOGLOG`)

### Candidate Solution A: Explicit syntax
- **Approach**: `COUNT(DISTINCT x) USING HYPERLOGLOG(0.01)` forces a specific sketch with a stated precision.
- **Performance**: O(1) sketch lookup.
- **Time to implement**: ~1.5 months.
- **Energy cost**: ~nJ per query.
- **Upside**: User control; predictable; debuggable.
- **Downside**: Pushes sketch expertise onto users.
- **Key paper**: Cormode-Garofalakis 2008 ("Sketches for massive data").

### Candidate Solution B: Auto-selection
- **Approach**: Planner chooses the sketch by aggregate shape (count-distinct→HLL, freq→CM, quantile→KLL) and tolerance.
- **Performance**: Same O(1) at runtime.
- **Time to implement**: ~3 months (selection rules, maintenance).
- **Energy cost**: Negligible marginal.
- **Upside**: No user burden; expert system encoded once.
- **Downside**: Opaque; surprises on data distribution changes.
- **Key paper**: Cormode-Garofalakis 2008; Apache DataSketches (KLL, *Lambert et al.*).

### Candidate Solution C: Hint (advisory)
- **Approach**: `USING HYPERLOGLOG` is advisory; planner may substitute if it proves equivalent or cheaper.
- **Performance**: Plan-time check.
- **Time to implement**: ~2 months.
- **Energy cost**: Negligible.
- **Upside**: Best of (A) and (B).
- **Downside**: "Did my hint take?" debugging.
- **Key paper**: PostgreSQL `enable_hashjoin`-style GUCs; Cormode 2008.

### Recommendation
**Hint (C) as the default** — the planner is authoritative but honors user hints when valid. Auto-selection (B) is the implicit behavior when no hint is given; explicit (A) syntax (`USING … (precision)`) is the override knob for experts. This mirrors the `TIER` hint philosophy from Q-2.

---

## P-Q-07: Memory Budget Hints (`MEMORY BUDGET 4 GB`)

### Candidate Solution A: Hard limit
- **Approach**: Enforce a hard RSS/working-set cap via the allocator; spill to CXL/NVMe on overflow.
- **Performance**: Allocator overhead ~ns/allocation; spill cost on overflow.
- **Time to implement**: ~2 months (allocator integration, spill path).
- **Energy cost**: Zero marginal; spill to CXL ~3× DRAM energy.
- **Upside**: Predictable; honors the hint exactly; co-tenant friendly.
- **Downside**: Hard failures / severe spills if budget mis-estimated.
- **Key paper**: Goel et al., *VLDB* 2014 (memory-bounded query processing); authority-controlled RSS (jemalloc arenas).

### Candidate Solution B: Soft hint (target)
- **Approach**: Treat as a target the cost model optimizes toward; may exceed by a slack.
- **Performance**: One extra cost-model pass.
- **Time to implement**: ~2.5 months.
- **Energy cost**: Negligible.
- **Upside**: Robust; graceful.
- **Downside**: Soft — can be exceeded; harder to reason about for co-tenancy.
- **Key paper**: Goel 2014; Lit-Molnar.

### Candidate Solution C: Adaptive (engine-managed)
- **Approach**: No hint; engine manages per-query memory via the global admission controller.
- **Performance**: Optimal under global pressure.
- **Time to implement**: ~0 months (already built).
- **Energy cost**: Zero marginal.
- **Upside**: No user burden; globally fair.
- **Downside**: No per-query isolation; OOM kills under contention.
- **Key paper**: Redshift / Spark admission control papers.

### Recommendation
**Soft hint (B) as default**, with `MEMORY BUDGET 4 GB STRICT` opting into (A). Pairs with the submodular index selector (P-M-14) which uses the budget as the knapsack constraint. Fully-adaptive (C) is the no-hint behavior.

---

## P-Q-08: Energy-aware Queries (`ENERGY BUDGET 100 J`)

### Candidate Solution A: RAPL measurement + throttle
- **Approach**: Measure actual joules via RAPL (Hähnel 2012) at the package/DRAM domain; abort or degrade the query if the budget is exceeded.
- **Performance**: RAPL read ~µs; sampling at ≥10 ms granularity (finer adds overhead without accuracy gain).
- **Time to implement**: ~2.5 months (RAPL polling, query throttling, abort semantics).
- **Energy cost**: RAPL polling itself ~nJ; negligible.
- **Upside**: Ground-truth measurement; honors the budget exactly; auditable.
- **Downside**: RAPL is per-package, not per-query, in hardware — attributing joules to a single query needs model-based apportionment (constant offset issue, Khan 2018).
- **Key paper**: Hähnel et al. 2012 (SIGMETRICS/GreenMetrics); Khan et al. 2018 (DRAM RAPL validation); Tiwari et al. 1994 (power modeling).

### Candidate Solution B: Model-based prediction
- **Approach**: Use the energy model from P-M-03 to predict joules pre-execution; choose the plan that fits the budget; instrument runtime to confirm.
- **Performance**: Pre-execution model eval ~µs; runtime instrumentation negligible.
- **Time to implement**: ~3.5 months (energy model, plan selection under budget).
- **Energy cost**: Negligible.
- **Upside**: Predictive — avoids wasted work; integrates with cost model.
- **Downside**: Model error → over- or under-shoot; needs runtime RAPL to confirm.
- **Key paper**: Tiwari 1994; Wang et al. on power modeling.

### Candidate Solution C: DPU / NIC offload
- **Approach**: Offload energy-expensive operators (scan, decompression) to a BlueField/SmartNIC DPU whose energy is metered separately.
- **Performance**: DPU throughput ~10–40 Gb/s; offload latency ~µs.
- **Time to implement**: ~6+ months (DPU programming, split-execution planner).
- **Energy cost**: Shifts joules from host (measured by RAPL) to DPU (measured by BMC) — total may be similar but host budget is preserved.
- **Upside**: Decouples energy accounting; uses idle DPU cycles.
- **Downside**: Highest engineering cost; DPU energy accounting is non-trivial; latency penalty for small queries.
- **Key paper**: Tiwari 1994; Nebula / BlueField benchmark papers.

### Recommendation
**Model-based prediction (B) to choose the plan, RAPL measurement (A) to enforce and audit the budget at runtime.** This is the only combination that is both predictive and trustworthy. DPU offload (C) is a strategic direction for a future revision where DPU joules are first-class — not Wave-3 scope.

---

## P-Q-09: Streaming / Continuous Queries (`CONTINUOUS QUERY`)

### Candidate Solution A: Windowed stream-relational algebra (CQL)
- **Approach**: Treat streams as sliding-window relations; re-evaluate the relational plan on each window slide. The CQL semantics (Arasu-Babu-Widom 2006) give a precise relation↔stream duality.
- **Performance**: Per-window evaluation; cost ∝ window size / slide. For 1 s tumbling windows over 10⁵ events/s, ~10⁵ ops/sec/core.
- **Time to implement**: ~3 months (window operators, stream-to-relation adapters).
- **Energy cost**: ~nJ per event (L3-resident window state).
- **Upside**: Familiar SQL semantics; precise formal model; incremental evaluation well-studied.
- **Downside**: Window choice is critical; out-of-order/watermark handling needed.
- **Key paper**: Arasu-Babu-Widom, *ACM TODS* 2006 ("The CQL continuous query language").

### Candidate Solution B: Coalgebraic stream processing
- **Approach**: Model streams coalgebraically as (state, next) transition systems; queries are coalgebra homomorphisms giving principled composition and minimization.
- **Performance**: Equivalent to dataflow; minimization can fuse operators for ~10–30% throughput.
- **Time to implement**: ~5 months (coalgebraic IR, fusion/minimization passes).
- **Energy cost**: Lowest after fusion; ~5–20% lower than (A).
- **Upside**: Principled operator fusion; formal equivalence to relational plans.
- **Downside**: Highest conceptual cost; team needs coalgebra literacy.
- **Key paper**: Rutten 2000 ("Universal coalgebra"); Silva-Bonchi-Rutten on coalgebraic streams.

### Candidate Solution C: Complex event processing (CEP)
- **Approach**: Pattern-based; declare patterns (`WITHIN 5 sec FOLLOWED-BY …`) over the stream; NFA-based matching.
- **Performance**: NFA matching O(events × states); high for many patterns.
- **Time to implement**: ~3.5 months (pattern compiler, NFA engine).
- **Energy cost**: ~nJ–µJ per event depending on pattern complexity.
- **Upside**: Powerful for temporal-pattern queries that SQL struggles to express.
- **Downside**: Different semantics from relational; harder to compose with the rest of the engine.
- **Key paper**: Akidau et al. 2013 (MillWheel, *VLDB*); Wu et al. on CEP (SASE).

### Recommendation
**Windowed CQL (A) as the foundational `CONTINUOUS QUERY` semantics**, with a coalgebraic IR (B) used *internally* to fuse and minimize the dataflow graph — the user never sees the coalgebra, but the engine benefits from principled fusion. CEP (C) is an extension layered on top for temporal-pattern queries. This matches Dataflow/MillWheel's industrial precedent.

---

# SUMMARY OF RECOMMENDATIONS

| ID | Problem | Recommended solution | Key axes |
|---|---|---|---|
| M-01 | MDL schema | Greedy + closed-form fast path; LP offline | Low energy, fast |
| M-02 | NUMA partitioning | METIS + spectral fallback | Fast, low energy |
| M-03 | Cost model | Calibrated analytic + learned residual | Interpretable |
| M-04 | (ε,δ) propagation | McDiarmid; union-bound fallback | Tight, cheap |
| M-05 | AGM joins | Leapfrog (cyclic) + binary (acyclic) | Practical |
| M-06 | Tensor train | TT-SVD (hot) + TT-Cross (streaming) | Ratio/speed split |
| M-07 | Source coding | rANS (cold) + LZ4 (hot) | Tier-aware |
| M-08 | Tier replacement | LRU + WFA on top tier | Cheap + safe |
| M-09 | Schema migration | Custom DSL + CQL test oracle | Owned proofs |
| M-10 | Distributed consistency | Cosheaf (agg) + sheaf radius (monitor) | Cheap + correct |
| M-11 | PAC SQL | Empirical-Bernstein + sequential stopping | Tightest, low-E |
| M-12 | Sketch composition | Per-sketch native; McDiarmid deep | Tight |
| M-13 | Join ordering | Greedy; LP for n>15 | Right-sized |
| M-14 | Index selection | Greedy; continuous submodular under budget | (1−1/e) cheap |
| M-15 | Linear-logic types | Rust affine types + external checker | Zero runtime |
| Q-01 | Approximate queries | Sketch when available; seq. e-Bernstein else | Lowest energy |
| Q-02 | Tier hints | Soft hint + `STRICT` opt-in | Robust |
| Q-03 | Similarity | Brute VPOPCNTDQ ≤10⁶; LSH above | AVX-native |
| Q-04 | Consistency | Per-txn + downgrade-only overrides | Spanner-style |
| Q-05 | Protocol scope | Static analysis + runtime detection | Calvin-style |
| Q-06 | Sketch aggregations | Hint (advisory) + auto default | Planner-authoritative |
| Q-07 | Memory budget | Soft hint + `STRICT` opt-in | Pairs w/ M-14 |
| Q-08 | Energy budget | Model-predict + RAPL-enforce | Trustworthy |
| Q-09 | Continuous queries | Windowed CQL + coalgebraic fusion IR | Familiar + optimal |

## Cross-cutting observations

1. **The cost model (M-03) is the keystone.** Q-02, Q-06, Q-07, Q-08, M-13, M-14 all defer to it. Build the calibrated analytic model first, layer the learned residual second.
2. **RAPL measurement (Q-08) underwrites every energy claim.** The (ε,δ), sketch, and tier analyses all cite energy numbers derived from the RAPL baseline; the energy-budget hint is the only way to make these accountable to the user.
3. **Sketch infrastructure (M-12, Q-01, Q-06) is shared.** Build one sketch layer — maintained on ingest, mergeable, with native error composition — and reuse it across approximate queries, aggregations, and joins.
4. **AVX-512 VPOPCNTDQ (Q-03, M-05) is the engine's signature instruction.** Both similarity search and leapfrog join benefit; ensure the kernel table exposes a typed popcount primitive.
5. **Category-theoretic machinery (M-09, M-10, Q-09) is most valuable *inside* the engine, not as user-facing surface.** Functorial migration correctness, sheaf consistency radius, and coalgebraic stream fusion all pay off as compiler/optimizer internals rather than query syntax.

## Next actions

1. **Prototype the calibrated cost model (M-03 A)** — 2 months; unblocks Q-02/Q-06/Q-07/Q-08.
2. **Stand up the RAPL energy-accounting harness (Q-08 A)** — 1 month; unblocks every energy-axis claim.
3. **Implement greedy MDL + closed-form fast paths (M-01 A)** — 1.5 months; immediate ingest benefit.
4. **Build the sketch layer with native error composition (M-12 + Q-01 B/C)** — 3 months; unblocks `APPROXIMATE`, `USING HYPERLOGLOG`.
5. **Prototype leapfrog triejoin (M-05 A) and AVX-512 popcount primitive (Q-03 A)** in parallel — 3 months each; these are the engine's headline capabilities.
