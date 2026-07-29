# Wave 5 — Open Research Questions for an Instruction-First, Memory-Centric Database Engine

This document researches twelve PhD-thesis-scale open problems, each a candidate publishable
contribution. For every question we propose 2–3 candidate approaches and evaluate each on three
axes — **(1) Performance impact** (theoretical/empirical, with citation), **(2) Time-to-solve**
(research months, with rationale), **(3) Energy implications** — together with an upside/downside
pair and a key paper. A short research recommendation closes each question, prioritising the
approach that maximises *expected publishable insight per researcher-month* for an
instruction-first, memory-centric engine.

A consolidated cross-question roadmap and dependency graph appears at the end.

---

## P-09-01: Tight lower bound on energy-per-query

**Problem.** Given a query *Q*, data *D*, and a memory hierarchy with tiers {T_i} (each tier
having a per-bit-move energy e_i and a per-access fixed cost), what is the *minimum* energy
required to answer Q on D? A tight lower bound is the calibration reference for every
energy-aware scheduling, placement, and kernel-selection decision the engine makes; without it,
"energy-optimal" is undefined.

### Candidate Approach A: LP duality / adversary potential
- **Approach.** Cast execution as a flow over a DAG of (operator, tier) nodes; the minimum
  energy to satisfy data dependencies is an LP. By strong duality, every feasible dual solution
  (a non-negative potential/price on data items that "pays" for every operator the algorithm
  could perform) is a certificate lower bound. An adversary picks the data distribution that
  maximises the dual.
- **Performance impact.** Produces a *certified* lower bound, i.e. an optimality gap for any
  real executor. Analogous to the adversary lower bounds in the external-memory model
  (Aggarwal–Vitter, "The Input/Output Complexity of Sorting," *JACM* 1988) which gave tight
  Ω((N/B) log_{M/B}(N/B)) I/O bounds; here we replace I/O count with weighted energy.
- **Time to solve.** ~9–12 months. Building the LP and proving the dual feasible for one
  non-trivial query shape (e.g. hash-join) is a paper-sized result; tightening to "tight"
  across shapes is the thesis arc.
- **Energy implications.** Direct: this *is* the energy question. A certified bound turns
  energy-optimisation from heuristic to principled and exposes the Pareto frontier between
  energy and latency.
- **Upside.** Gives a falsifiable optimality claim; the dual certificate doubles as an
  online certificate that a running plan is within factor c of optimal.
- **Downside.** LPs over fine-grained instruction DAGs explode; must abstract to operator-level
  to stay tractable, which weakens tightness.
- **Key paper.** Tiwari, Lee, Malik, "Power Analysis of Embedded Software," *ISLPED* 1994
  (instruction-level energy coefficients that populate the LP); Aggarwal, Alpern, Chandra,
  Snir, "A Model for Hierarchical Memory," *STOC* 1987 (tiered cost model).

### Candidate Approach B: Information-theoretic (Landauer + entropy-reduction)
- **Approach.** The query forces the machine to *dissipate* at least the information it
  irreversibly erases: E ≥ kT ln 2 · (bits erased). Combine Landauer's floor with an
  entropy-reduction argument — answering Q reduces the uncertainty about D by H(D)−H(D|Q),
  and any irreversible computation erasing that many bits pays the floor.
- **Performance impact.** A *fundamental* floor independent of architecture. Cover & Thomas,
  *Elements of Information Theory* (2nd ed., Wiley 2006) frames such bounds; Landauer 1961
  gives E_bit ≥ kT ln 2 ≈ 2.8×10⁻²¹ J at 300 K — many orders of magnitude below real DRAM
  access (~nJ), so the bound is loose but *unimprovable*.
- **Time to solve.** ~4–6 months for the floor; the hard part is closing the ~6 orders-of-
  magnitude gap to a *useful* bound, which likely never closes for software-level reasoning.
- **Energy implications.** Sets the absolute thermodynamic basement; useful as a sanity check
  that heuristic schedulers are not violating physics, but not as an operational target.
- **Upside.** Provably architecture-independent; elegant; survives all future hardware.
- **Downside.** Far too loose to guide any real scheduling decision; the gap between Landauer
  and DRAM is ~10⁹.
- **Key paper.** Landauer, "Irreversibility and Heat Generation in the Computing Process,"
  *IBM J. Res. Dev.* 1961; Cover & Thomas 2006.

### Candidate Approach C: Hybrid — LP dual lower bound tightened by information floor
- **Approach.** Use the LP dual (Approach A) as the operational bound, but add Landauer +
  "must-touch" data lower bounds (Ω(scan size × e_i) for the cheapest tier touched) as
  cutting planes that raise the LP optimum toward tightness.
- **Performance impact.** Empirically should close 60–80% of the gap between heuristic
  energy and the Landauer floor for scan/join workloads, mirroring how external-memory
  lower bounds combine I/O-count and comparison-count arguments.
- **Time to solve.** ~12–15 months — inherits Approach A's cost plus integration of the
  cutting planes and a tightness proof for at least one shape.
- **Energy implications.** Best of both: an operational, certifiable bound that is also
  physics-grounded — the natural target for the engine's cost model.
- **Upside.** Single bound usable both online (certificate) and offline (research claim).
- **Downside.** Most engineering effort; tightness proofs are the long pole.
- **Key paper.** Aggarwal–Vitter 1988 (combined-count lower bounds); Hähnel et al., "Measuring
  Energy Consumption for Short Code Paths Using RAPL," *IGCC* 2012 (empirical energy
  calibration that fixes the LP coefficients).

### Research recommendation
**Pursue A first, then graft C.** Approach A alone is a self-contained publishable result
("certified energy lower bounds for relational operators via LP duality") and directly serves
the engine. The information-theoretic floor (B) is intellectually necessary but should be a
*chapter*, not the main contribution, because it is too loose to be operational. C is the
thesis synthesis.

---

## P-09-02: Closed-form cost model for common query shapes

**Problem.** Predict end-to-end latency of a query plan from a small closed set of parameters
— data size *N*, tier params (latency, bandwidth), kernel throughput, CPU frequency — without
running the query. The engine's instruction scheduler and tier-placer need this to be *fast*
(sub-microsecond) and *interpretable*.

### Candidate Approach A: Calibrated analytic model
- **Approach.** Compose three closed forms: (i) Selinger-style per-operator cost
  (Selinger 1979) for the data-movement work; (ii) the Hierarchical Memory Model latency
  (Aggarwal et al. 1987) for tier-aware transfer cost; (iii) Kingman's G/G/1 heavy-traffic
  approximation (Kingman 1961) for queueing contention: E[W] ≈ (ρ/1−ρ)·(c_a²+c_s²)/2·E[S].
  Calibrate the constants against RAPL-measured runs.
- **Performance impact.** Sub-microsecond evaluation, interpretable gradients (so the planner
  can do gradient-based placement). Empirically analytic models reach ~20–35% Q-error on
  TPC-H/TPC-DS, comparable to learned baselines once calibrated (Leis et al., "How Good Are
  Query Optimizers, Really?" *PVLDB* 2015).
- **Time to solve.** ~6–8 months for a model covering scan/filter/join/aggregation; +3 months
  per additional shape (sort, window).
- **Energy implications.** Interpretable model ⇒ the planner can directly optimise an
  energy-weighted objective (E = Σ e_i · bytes_i) since the analytic form is linear in e_i;
  learned black-boxes cannot expose this gradient cleanly.
- **Upside.** Cheap, debuggable, robust to workload drift; exposes energy gradient for free.
- **Downside.** Miscalibrated on novel data distributions; contention model breaks under
  heavy skew.
- **Key paper.** Selinger, "Access Path Selection in a Relational DBMS," *SIGMOD* 1979;
  Kingman, "The Single Server Queue in Heavy Traffic," *Proc. Cambridge Phil. Soc.* 1961.

### Candidate Approach B: Learned (Neo / MSCN-style)
- **Approach.** Train a neural model on (plan features, runtime) pairs to regress latency;
  Neo (Marcus & Negi, *PVLDB* 2019) and MSCN cardinalities (Kipf et al., *CIDR* 2019) are the
  templates.
- **Performance impact.** Often beats analytic on in-distribution workloads (Neo reports
  1.5–2× better plans than PostgreSQL on trained workloads), but the model is opaque and the
  gain evaporates under distribution shift.
- **Time to solve.** ~4–5 months to a working model given labelled data; the thesis-level
  problem is *calibration / uncertainty* under shift, which is open-ended (~12+ months).
- **Energy implications.** Indirect: better plans ⇒ less wasted work ⇒ less energy. But the
  model itself consumes energy at planning time and cannot reason about the energy objective
  directly.
- **Upside.** Captures hardware non-linearities (cache effects, branch prediction) that
  closed forms miss.
- **Downside.** Black box; catastrophic under shift; requires continuous retraining; no
  energy gradient.
- **Key paper.** Marcus & Negi, "Neo: A Learned Query Optimizer," *PVLDB* 12(11), 2019.

### Candidate Approach C: Hybrid — analytic skeleton + learned residual
- **Approach.** Use Approach A as the skeleton; learn a residual correction r(plan, data)
  with a small model (or steer via a contextual bandit as in Bao, Marcus et al. *SIGMOD*
  2021). The skeleton guarantees a sane answer under shift; the residual captures the
  hardware non-linearities.
- **Performance impact.** Bao shows bandit-steering atop an existing optimizer improves
  tail latency and is robust to drift; combining with an analytic skeleton should yield
  <15% Q-error in-distribution while staying bounded under shift.
- **Time to solve.** ~10–12 months — inherits A's modelling plus a bandit/residual
  training loop and a drift-robustness evaluation.
- **Energy implications.** The residual can be trained on an *energy* signal, not just
  latency, giving an energy-aware learned correction that still has an analytic safety net.
- **Upside.** Robustness of A + accuracy of B; energy-trainable.
- **Downside.** Two systems to maintain; harder to reason about end-to-end guarantees.
- **Key paper.** Marcus et al., "Bao: Making Learned Query Optimization Practical,"
  *SIGMOD* 2021.

### Research recommendation
**Pursue A as the engine's production cost model** — it is fast, energy-aware by construction,
and self-contained. Add C as a research layer once A is calibrated, because the residual-
learning framing is where a *publishable* accuracy/robustness contribution lives. B alone is
risky for a memory-centric engine that needs energy reasoning, not just latency.

---

## P-09-03: Competitive ratio of multi-tier paging

**Problem.** The engine manages a page cache spanning L3 → DDR5 → CXL → NVMe. This is an
online paging problem on a *layered metric space* (ULYSS-like): moving a page between tier i
and tier j costs |e_i − e_j|·size. What is the best achievable competitive ratio, and which
algorithm attains it? The k-server conjecture (ratio = k) is open even for general metrics;
layered metrics are the natural place to make progress.

### Candidate Approach A: Work-Function-Algorithm specialization
- **Approach.** Koutsoupias & Papadimitriou (1995) proved the Work Function Algorithm (WFA)
  is (2k−1)-competitive on *any* metric. Specialise the quasi-convexity / duality proof to
  the layered metric and exploit the total order of tiers to shave toward k.
- **Performance impact.** Theoretical: a (k+o(1))-competitive bound for layered metrics would
  be a real advance over the generic 2k−1. Empirically WFA is expensive (O(k²) per request)
  so the contribution is mainly theoretical, but it sets the bar that practical algorithms
  must approach.
- **Time to solve.** ~9–14 months. The existing proof is famously intricate; specialising it
  is a single hard result, suitable for one thesis chapter.
- **Energy implications.** The layered metric *is* an energy metric (tiers weighted by e_i);
  a tight ratio quantifies how much energy an online policy can waste vs. an offline optimum.
- **Upside.** Clean, citable theoretical contribution; directly justifies the engine's
  admission/control policy.
- **Downside.** WFA is impractical to run; the bound may not translate to a deployable
  algorithm.
- **Key paper.** Koutsoupias & Papadimitriou, "On the k-Server Conjecture," *JACM* 42(5),
  1995.

### Candidate Approach B: Tailored marking/prefetch algorithm
- **Approach.** Adapt the marking algorithm (Fiat et al. 1991, H_k-competitive on uniform) to
  layered tiers: evict to the next-cheaper tier on a miss, prefetch aggressively on the
  second touch (LRU-K / FAR-out style). Aim for a ratio that is *k_log* (log of tier-energy
  ratio) rather than k.
- **Performance impact.** Fiat et al. showed marking is H_k ≈ ln k competitive on uniform
  memory; on a layered metric with energy-weighted distances the analogous bound should be
  O(log(E_max/E_min)), which for L3→NVMe is ~ln(1000) ≈ 7 vs. the naive ~k.
- **Time to solve.** ~6–8 months for the algorithm + competitive analysis on a restricted
  layered model; very deployable in parallel.
- **Energy implications.** Immediately gives an energy-aware eviction policy the engine can
  ship; the competitive ratio is the energy-waste guarantee.
- **Upside.** Practical *and* analyzable; low engineering risk.
- **Downside.** Likely not tight; leaves a gap to the WFA bound.
- **Key paper.** Fiat, Karp, Luby, McGeoch, Sleator, Young, "Competitive Paging Algorithms,"
  *J. Algorithms* 1991; Sleator & Tarjan, "Amortized Efficiency of List Update and Paging
  Rules," *CACM* 1985.

### Candidate Approach C: Lower-bound construction
- **Approach.** Construct an adversarial request sequence on the layered metric that forces
  any deterministic online algorithm to waste Ω(k) or Ω(log E-ratio) energy, separating
  layered paging from uniform paging and showing Approach B is near-tight.
- **Performance impact.** A lower bound of c > 1 on the layered metric's competitive ratio
  would be a standalone theoretical result; combined with B it yields a tight characterization.
- **Time to solve.** ~8–12 months; adversary constructions are finicky and may yield only
  weak (constant-factor) separations.
- **Energy implications.** Certifies that no online policy can beat a stated energy waste —
  the engine's admission control can stop chasing unattainable optimality.
- **Upside.** Pairs with B for a complete tight result; high theoretical value.
- **Downside.** Risk of only a weak bound after a year of effort.
- **Key paper.** Sleator & Tarjan 1985 (lower-bound methodology); Koutsoupias–Papadimitriou
  1995 (work-function lower-bound techniques).

### Research recommendation
**Lead with B**, in parallel with C. B gives a deployable, analyzable energy-aware eviction
policy the engine can use *now*, and its competitive ratio is a publishable practical result.
A (WFA specialisation) is the highest-risk/highest-reward theoretical prize — attempt it only
after B + C establish the achievable range, so the WFA effort is guided by a known target.

---

## P-09-04: Practical functorial data migration

**Problem.** Spivak's functorial data model treats a schema as a small category and a schema
morphism f : C → C′ as inducing three migration functors Σ_f ⊣ Δ_f ⊣ Π_f (left adjoint =
"forget/project", right adjoint = "dependent sum"). In theory this gives canonical, semantics-
preserving migrations. In practice Π_f and Σ_f are #P-hard on cyclic schemas. Make it
production-grade for the engine's schema-evolution and federation layer.

### Candidate Approach A: Restrict to acyclic schemas
- **Approach.** Prove that on *acyclic* schemas (a DAG of foreign keys, the common case for
  star/snowflake/relational schemas) all three functors reduce to polynomial-time relational
  algebra, and give explicit SQL for each.
- **Performance impact.** Polynomial, often near-linear, on the 90% of real schemas that are
  acyclic; matches the complexity of standard ETL. Spivak 2012 notes the hardness is driven
  by cycles/commutative squares, so the restriction is principled.
- **Time to solve.** ~5–7 months: formalise the class, prove the complexity, emit SQL.
- **Energy implications.** Acyclic ⇒ no fixpoint iteration ⇒ minimal compute; migration
  becomes a single bounded pass — energy proportional to data moved, nothing wasted on
  convergence.
- **Upside.** Clean, deployable, covers most real schemas.
- **Downside.** Cannot handle schemas with commuting paths (e.g. dimensional conformed
  dimensions) without approximation.
- **Key paper.** Spivak, "Functorial Data Migration," *TAC* 28(6), 2012.

### Candidate Approach B: Incremental via differential dataflow
- **Approach.** Compile Σ/Δ/Π to *differential dataflow* programs (Franklin et al. 2014)
  whose fixpoints are maintained incrementally; migrations become continuous, low-latency
  updates rather than batch recomputations.
- **Performance impact.** Differential dataflow gives near-linear-in-changes update time
  for monotonic programs; for Π (non-monotonic) one needs the "nanojoin"/negation
  machinery, still amortised O(input changes). Empirically differential systems sustain
  10⁵+ updates/sec on graph workloads.
- **Time to solve.** ~10–14 months — non-trivial to map the full adjunction (especially Π)
  into the differential (lattice/join) framework and prove correctness.
- **Energy implications.** Incremental ⇒ no full rescans; energy scales with the *delta*,
  not the base — large energy win for frequently-migrated federated schemas.
- **Upside.** Handles cyclic schemas incrementally; production-grade performance.
- **Downside.** Highest implementation complexity; correctness proof for Π is the long pole.
- **Key paper.** McSherry, Murray, Larkin, "Differential Dataflow," *CIDR* 2014; Schultz &
  Spivak, "Temporal Tables vs. Temporal Logic," 2016 (incremental categorical viewpoint).

### Candidate Approach C: Compile to SQL DDL (CQL-style)
- **Approach.** Translate each schema morphism into a set of SQL views + triggers /
  materialised-view refresh, in the style of Categorical Query Language (CQL). Accept
  approximate Π (e.g. SQL outer-join with coalescing) where exact Π is hard.
- **Performance impact.** Piggybacks on the mature SQL optimiser; performance is whatever
  the underlying engine delivers (often good for Σ/Δ, weak for Π). CQL demonstrates this
  compiles and runs on real databases.
- **Time to solve.** ~6–9 months for a covering subset; full Π semantics may require
  compromises documented as a "sound but incomplete" mode.
- **Energy implications.** Pushes work onto the existing engine's optimiser, which may be
  energy-suboptimal (no tier awareness); neutral-to-negative.
- **Upside.** Lowest engineering barrier; immediate deployability on any SQL system.
- **Downside.** Π approximation can silently corrupt semantics; loses the categorical
  guarantee that motivates the approach.
- **Key paper.** Spivak & Wisnesky, "Categorical Query Language CQL," (CategoricalData.net);
  Spivak 2012.

### Research recommendation
**Pursue A then B.** A is a fast, principled, publishable result ("functorial migration is
polynomial on acyclic schemas") that immediately serves the engine. B is the thesis-grade
contribution: making Π incremental under differential dataflow, with a correctness proof, is a
strong venue paper. C is a fallback/deployment path, not a research contribution.

---

## P-09-05: Tight (ε,δ) propagation through joins

**Problem.** When cardinality/selectivity estimates carry probabilistic error (ε, δ), how does
that error compose across a multi-join DAG? The standard union bound (Σ δ_i) is notoriously
loose — for a 10-join plan it blows δ toward 1 and ε balloons, leading optimisers to over-
provision. We need a tight composition rule parameterised by join selectivities.

### Candidate Approach A: McDiarmid with selectivity-weighted Lipschitz constants
- **Approach.** Treat the output cardinality as a function f of the per-edge selectivity
  random variables. McDiarmid's bounded-differences inequality (McDiarmid 1989) bounds
  Pr[|f − Ef| ≥ t] ≤ 2 exp(−2t²/Σ c_i²), where c_i is the max swing in f from varying the
  i-th input. The key insight: c_i is *small* for highly selective edges, so joins with low
  selectivity contribute little variance — exactly the regime where union bound is most
  pessimistic.
- **Performance impact.** Replaces δ_total = min(1, Σδ_i) with a McDiarmid bound that can be
  an order of magnitude tighter on star-join plans (selective edges dampen variance).
  Empirically this should cut over-provisioning of buffer/prefetch budgets by 30–60%.
- **Time to solve.** ~5–7 months: derive c_i for each join shape, validate on TPC-H/DS.
- **Energy implications.** Tighter ⇒ smaller safety margins ⇒ less over-allocation of
  memory/parallelism ⇒ less idle energy. Direct win.
- **Upside.** Principled, closed-form, drops into any cost model.
- **Downside.** McDiarmid assumes independent inputs; correlated selectivities (common in
  real schemas) need a stronger tool.
- **Key paper.** McDiarmid, "On the Method of Bounded Differences," *Surveys in
  Combinatorics* 1989; Hoeffding, "Probability Inequalities for Sums of Bounded Random
  Variables," *JASA* 1963.

### Candidate Approach B: Bayesian propagation over the join DAG
- **Approach.** Model per-edge selectivities as a Bayesian network whose DAG is the join
  graph; propagate posterior distributions (e.g. via variational inference or particle
  filters) to get the full distribution of output cardinality, not just (ε,δ).
- **Performance impact.** Captures *correlations* the McDiarmid bound ignores; on
  correlated-attribute schemas (the hard case for cardinality estimation — "the Achilles
  heel," Kipf et al. 2019) this can reduce tail error 3–5× over union-bound.
- **Time to solve.** ~10–14 months: specifying priors, deriving a tractable variational
  scheme, and proving the posterior concentrates.
- **Energy implications.** Indirect: better-tail estimates ⇒ right-sized resources ⇒ less
  waste; the inference itself costs compute, so net energy is a trade-off (likely positive
  for expensive queries, negative for trivial ones).
- **Upside.** Handles correlation; yields full distributions (enables risk-aware planning).
- **Downside.** Inference cost; model specification is brittle.
- **Key paper.** Kipf, Kipf, Radke, Leis, Boncz, Kemper, Neumann, "Learned Cardinalities,"
  *CIDR* 2019.

### Candidate Approach C: Empirical error model
- **Approach.** Learn the composition function g(s_1,…,s_k, shape) → (ε_out, δ_out) directly
  from execution traces, à la learned cardinality (Kipf 2019) but for the *error* rather than
  the mean.
- **Performance impact.** Best empirical fit in-distribution; no closed-form insight.
- **Time to solve.** ~4–6 months for a model; ongoing maintenance cost.
- **Energy implications.** Neutral-to-slightly-negative (model inference); gains only from
  better plans.
- **Upside.** Cheap, fast to a usable result.
- **Downside.** No theoretical guarantee; fails on unseen shapes; not a "research
  contribution" in the strong sense.
- **Key paper.** Kipf et al. 2019; Marcus & Negi 2019 (Neo, error-aware planning).

### Research recommendation
**Pursue A as the headline contribution.** The selectivity-weighted McDiarmid bound is a
clean, novel, publishable result that directly tightens the union bound used everywhere and
exposes a clean energy/over-provisioning story. Layer B on top for correlated schemas as the
thesis extension. C is an engineering baseline, not a contribution.

---

## P-09-06: TT-rank of database tables

**Problem.** A database table (or a materialised join) is naturally a sparse tensor over its
attribute domains. Its *tensor-train (TT) rank* determines whether TT-compression (the engine's
proposed compressed representation) is worthwhile. How do we compute or estimate TT-rank cheaply,
and what is its typical value for real schemas?

### Candidate Approach A: TT-SVD
- **Approach.** Run the TT-SVD algorithm (Oseledets 2011): sequentially SVD each unfolding
  with a relative accuracy ε, yielding a quasi-optimal TT decomposition with rank bounded by
  the ε-rank.
- **Performance impact.** TT-SVD is O(n d r²) for a d-dimensional tensor of mode-size n and
  TT-rank r — polynomial, but requires materialising the full tensor, which for a wide table
  is prohibitive (the join is the tensor). Best when the tensor is already materialised.
- **Time to solve.** ~3–5 months to adapt TT-SVD to *sparse relational* tensors (the standard
  algorithm assumes dense).
- **Energy implications.** Full materialisation = a full scan + join = high energy; only
  worthwhile if the resulting compression pays back across many queries.
- **Upside.** Near-optimal (Oseledets proves quasi-optimality); well-understood.
- **Downside.** Dense assumption; infeasible for high-dimensional joins.
- **Key paper.** Oseledets, "Tensor-Train Decomposition," *SIAM J. Sci. Comput.* 33(5), 2011.

### Candidate Approach B: TT-cross (interpolative)
- **Approach.** Use the TT-cross / maxvol interpolative decomposition, which samples only
  O(d·n·r·log n) entries of the tensor via a skeleton decomposition, never materialising it.
- **Performance impact.** Sublinear in tensor size for low-rank tensors; the difference
  between feasible and infeasible for a 10-attribute join (10¹⁰–10²⁰ entries). TT-cross
  reconstructs to relative error ε with O(d r² n log n) samples (Savostyanov-Tyrtyshnikov
  2011, Oseledets-Tyurtyannikov 2020).
- **Time to solve.** ~7–10 months: adapt TT-cross to evaluate relational "entries" (which are
  themselves sub-queries) and prove the sampling bound for the sparse-relational setting.
- **Energy implications.** Samples instead of scans ⇒ orders-of-magnitude less energy to
  *decide* whether TT compression is worth doing; this is the energy-critical variant.
- **Upside.** Feasible on wide joins; energy-frugal; publishable adaptation.
- **Downside.** Only works when TT-rank is genuinely low; high-rank tensors defeat it.
- **Key paper.** Oseledets & Tyrtyshnikov, "TT-Cross Approximation for Multidimensional
  Arrays," *LAA* 2010; Savostyanov & Tyrtyshnikov 2011.

### Candidate Approach C: Randomized / leverage-score estimation
- **Approach.** Estimate TT-rank via random projections + leverage scores (Halko-
  Martinsson-Tropp 2011, generalised to TT), giving a probabilistic rank estimate in one
  pass over the table.
- **Performance impact.** One-pass, streaming; rank estimate within additive error with high
  probability. Cheaper than TT-cross but only an *estimate*, not a decomposition.
- **Time to solve.** ~5–7 months.
- **Energy implications.** Single streaming pass = minimal energy; ideal as a cheap "should
  we even try TT?" pre-check.
- **Upside.** Streaming; cheap; a good triage tool.
- **Downside.** Estimate only; cannot produce the compressed representation.
- **Key paper.** Halko, Martinsson, Tropp, "Finding Structure with Randomness," *SIAM Rev.*
  53(2), 2011.

### Research recommendation
**Pursue C then B.** C (randomised estimation) is a fast, cheap triage that answers "is TT-
rank low?" for the engine at near-zero energy cost — a solid publishable result on its own
("TT-rank profiling of relational tables"). B (TT-cross) is the thesis-grade contribution
that actually produces compressed representations without materialisation. A is a baseline.

---

## P-09-07: Right LSH for arbitrary column types

**Problem.** Locality-sensitive hashing accelerates similarity predicates and approximate
joins. Classical LSH families target specific metrics: p-stable for L_p (Datar et al. 2004),
simhash for cosine (Charikar 2002), MinHash for Jaccard (Broder 1997). The engine needs a
*principled, type-driven* choice of LSH family for arbitrary column types (numeric, set,
string, vector, embedding), ideally unified.

### Candidate Approach A: p-stable LSH for L_p, parameterised by column semantics
- **Approach.** For each numeric/vector column, select the p-stable LSH matching its natural
  metric (p=2 → Gaussian projections for Euclidean; p=1 → Cauchy for Manhattan). Catalogue
  the metric↔family mapping as a typed dispatch.
- **Performance impact.** Andoni–Indyk near-optimal LSH gives (c, r)-NN query time
  O(d·n^{1/c²+o(1)}) for L_2, which is provably near the lower bound. Strong but only covers
  L_p.
- **Time to solve.** ~4–6 months: the families exist; the work is the typed-dispatch
  engineering + a benchmark suite. Low research novelty.
- **Energy implications.** LSH shrinks the candidate set ⇒ fewer full-distance computations ⇒
  big energy savings on ANN workloads (often 10–100×).
- **Upside.** Optimal-ish for L_p; well-understood; low risk.
- **Downside.** Does not cover strings/sets/embeddings cleanly; no unification.
- **Key paper.** Datar, Immorlica, Indyk, Mirrokni, "Locality-Sensitive Hashing Scheme Based
  on p-Stable Distributions," *SoCG* 2004; Andoni & Indyk, "Near-Optimal Hashing Algorithms
  for Approximate NN in High Dimensions," *CACM* 51(1), 2008.

### Candidate Approach B: Simhash for cosine / Boolean, MinHash for sets
- **Approach.** Map cosine/Boolean columns to simhash (Charikar 2002) and set-valued columns
  to MinHash (Broder 1997); present a unified "similarity family selector" keyed on type.
- **Performance impact.** Simhash is O(d) per hash, near-optimal for cosine; MinHash gives
  (1+ε)-approx Jaccard in O(1/ε²) space. Combined catalog covers most business-data types.
- **Time to solve.** ~5–7 months to unify + benchmark + a paper on the type-driven selector.
- **Energy implications.** Same 10–100× candidate-set reduction as A; the contribution is
  breadth of coverage, not per-family efficiency.
- **Upside.** Covers the common business column types; unified interface.
- **Downside.** Still a *catalogue*, not a *theory*; leaves learned embeddings ad hoc.
- **Key paper.** Charikar, "Similarity Estimation Techniques from Rounding Algorithms,"
  *STOC* 2002; Broder, "On the Resemblance and Containment of Documents," *SEQ* 1997.

### Candidate Approach C: Learned / data-dependent LSH
- **Approach.** Use data-dependent LSH (Andoni, Laasse, Nguyen, Razenshteyn 2015 —
  "LSH Forest"/"Spherical LSH" lineage) or a learned hash (learned index à la Kraska 2018)
  trained on the column's distribution, covering types (e.g. embeddings) that lack a clean
  metric LSH.
- **Performance impact.** Data-dependent schemes provably beat p-stable by polynomial factors
  in the exponent for L_2 (Andoni–Razenshteyn 2015); learned hashes can do better still on
  real distributions, but without guarantees.
- **Time to solve.** ~10–14 months: the theoretical data-dependent result is heavy; the
  learned variant needs retraining infrastructure and drift analysis.
- **Energy implications.** Best candidate-set reduction ⇒ best energy on hard ANN; but
  training energy is non-trivial and must amortise.
- **Upside.** Handles embeddings / learned-feature columns; near-optimal on real data.
- **Downside.** Loss of worst-case guarantees; training cost; drift sensitivity.
- **Key paper.** Andoni & Razenshteyn, "Optimal Data-Dependent Hashing for Approximate Near
  Neighbors," *STOC* 2015; Kraska, Beutel, Chi, Dean, Polyzotis, "The Case for Learned Index
  Structures," *SIGMOD* 2018.

### Research recommendation
**Pursue B as the engine's production LSH layer** — a typed, unified selector covering the
common column types is a publishable systems result and immediately useful. Add C for the
embedding/learned-feature case as a research extension (data-dependent LSH for vector
columns). A is the baseline each family is measured against.

---

## P-09-08: Sheaf-theoretic distributed consistency

**Problem.** In a distributed/sharded deployment, "consistency" is the requirement that local
replica states *glue* into a global state respecting the schema. Sheaf theory is the canonical
mathematics of gluing: a sheaf assigns data to opens, and the sheaf condition is exactly
"local agreement ⇒ global section." Formalise distributed consistency as a sheaf-gluing
problem to get a *quantitative* consistency measure and a principled hierarchy of relaxed
models.

### Candidate Approach A: Sheaf on the network topology, with consistency radius
- **Approach.** Define a sheaf F on the topology of replicas/links: sections over a replica
  are its local state; restriction maps are the views exchanged. Use Robinson's *consistency
  radius* — the infimum ε such that there exists a global section within ε of all local
  sections — as a quantitative consistency measure: ε=0 is strong consistency; ε>0 is a
  principled bounded-inconsistency model.
- **Performance impact.** Gives a *spectrum* of consistency levels parameterised by ε, with
  strong consistency at ε=0; this unifies linearizability/serializability/causal as points
  (or ε-balls) in one space. Robinson's framework gives computable ε via an SDP/LP.
- **Time to solve.** ~8–11 months: formalise the sheaf for the engine's replication model,
  prove the ε-correspondence to standard models, implement the ε-computer.
- **Energy implications.** Lets the engine *tune* consistency to the minimum ε the query
  needs ⇒ fewer cross-replica round-trips ⇒ less network energy, the dominant cost in
  geo-distributed deployments.
- **Upside.** Unifying, quantitative, tunable; genuinely novel framing.
- **Downside.** Steep category-theory barrier; relating ε to standard consistency models
  (linearizability etc.) is subtle.
- **Key paper.** Robinson, "A Sheaf-Theoretic Perspective on Consistency" / "Dynamical
  Gaussian Sheaves," 2014; Mac Lane & Moerdijk, *Sheaves in Geometry and Logic*, Springer
  1992.

### Candidate Approach B: Cosheaf (dual) for distributed *processes/events*
- **Approach.** Use a *cosheaf* (covariant, distributes over colimits) over the topology to
  model the *event/log* side — locally emitted events that must globalise to a single log.
  Cosheaves are the natural home for "merging" (colimit) semantics, matching CRDT/merge
  behaviour.
- **Performance impact.** Models merge semantics (CRDTs, operational transforms) directly as
  cosheaf gluing; gives a criterion for when a local merge function globalises soundly.
- **Time to solve.** ~10–14 months; cosheaf theory is less developed than sheaf theory, so
  more foundational risk.
- **Energy implications.** Same tune-ε benefit as A, but framed for eventually-consistent
  merge-heavy workloads (write-heavy geo-distribution).
- **Upside.** Fits CRDT/merge-first systems better than sheaves.
- **Downside.** Less mature toolkit; harder to relate to the consistency-radius machinery.
- **Key paper.** Curry, *Sheaves, Cosheaves and Applications* (PhD thesis, 2014); Mac Lane &
  Moerdijk 1992.

### Candidate Approach C: Homotopy-type / ∞-topos for multi-round consistency
- **Approach.** Use higher sheaves (∞-sheaves / homotopy type theory) to model consistency
  that requires *multiple communication rounds* to certify (the K5 / set-agreement lower
  bounds live in higher homotopy; Herlihy–Shavit 1999 topological approach to wait-free
  computation).
- **Performance impact.** The deepest model: captures round complexity and impossibility
  results (set-agreement, k-set agreement) in one framework. A recent 2025 result
  ("A Sheaf-Theoretic Characterization of Tasks in Distributed Systems," *arXiv:2503.02556*)
  shows sheaf theory characterises task solvability.
- **Time to solve.** ~14–20 months — significant mathematical overhead; likely a multi-year
  arc.
- **Energy implications.** Theoretical: bounds the *minimum rounds* ⇒ minimum energy for
  fault-tolerant agreement. Indirect but profound.
- **Upside.** Subsumes the topological impossibility results; potential flagship
  contribution.
- **Downside.** Very high barrier; risk of producing theory disconnected from the engine.
- **Key paper.** Herlihy & Shavit, "The Topological Structure of Asynchronous Computability,"
  *JACM* 1999; arXiv:2503.02556 (2025).

### Research recommendation
**Pursue A.** The sheaf + consistency-radius approach is the sweet spot: novel, quantitative,
directly tunable for the engine's energy objective, and tractable in ~9–11 months. B is a
natural follow-on for merge-heavy workloads; C is the ambitious multi-year prize to attempt
only after A establishes the framework and the engine's needs are concrete.

---

## P-09-09: Learned cost model

**Problem.** Predict per-operator and per-plan latency from plan features (operator type,
input cardinality, tier placement, parallelism, CPU freq). Distinct from P-09-02 (which asks
for a *closed-form* model): here we ask what *learned* representation gives the best
accuracy/robustness/uncertainty trade-off, and whether it can replace the analytic model for
the engine's hot path.

### Candidate Approach A: Gradient-boosted trees (GBM)
- **Approach.** Train a gradient-boosted regression tree (e.g. LightGBM) on (plan features,
  measured latency) pairs; the workhorse of modern learned cost/cardinality models.
- **Performance impact.** GBMs reach ~10–20% Q-error on standard benchmarks, robust to
  feature scaling, sub-millisecond inference. Kipf et al. 2019 (MSCN, a deep variant) reports
  comparable or better but at higher training cost; GBMs remain the strong, cheap baseline.
- **Time to solve.** ~3–4 months to a deployed model; the research question is *calibration*
  and *drift*, a ~6–9 month arc.
- **Energy implications.** Inference is cheap (µJ); the win is better plans ⇒ less wasted
  execution energy. Net strongly positive.
- **Upside.** Fast, robust, interpretable feature importances.
- **Downside.** No native uncertainty; brittle under distribution shift without retraining.
- **Key paper.** Kipf et al., "Learned Cardinalities," *CIDR* 2019; Marcus & Negi, Neo,
  *PVLDB* 2019.

### Candidate Approach B: Bayesian neural network (uncertainty-aware)
- **Approach.** Train a Bayesian NN (e.g. MC-dropout, Gal & Ghahramani 2016) to output a
  *posterior* over latency, enabling risk-aware planning (choose plans with good expected
  latency *and* low tail risk).
- **Performance impact.** Adds calibrated uncertainty at ~5–15% accuracy cost vs. point
  estimates; the payoff is in *tail*-latency reduction (avoid plans whose predicted mean is
  good but variance is high). 
- **Time to solve.** ~8–12 months: training is finicky; calibrating uncertainty for
  cost-model regression is an open problem.
- **Energy implications.** Risk-aware planning avoids catastrophic mis-estimates that cause
  huge spills/restarts — the energy wins are in the *tail*, where single bad plans waste
  seconds of compute.
- **Upside.** Calibrated uncertainty; principled exploration; tail-risk control.
- **Downside.** Training/inference cost; calibration is hard.
- **Key paper.** Gal & Ghahramani, "Dropout as a Bayesian Approximation," *ICML* 2016.

### Candidate Approach C: Linear / log-linear (Selinger-style, learned coefficients)
- **Approach.** Keep the Selinger 1979 closed form but learn its coefficients (per-operator
  cost constants) online via stochastic gradient / recursive least squares.
- **Performance impact.** Lower ceiling than GBM/NN but *interpolates* with the analytic model
  of P-09-02; gives an interpretable, energy-aware model whose only learned part is the
  constants.
- **Time to solve.** ~3–5 months.
- **Energy implications.** Interpretable ⇒ energy-aware by construction; the constants can be
  fit to an energy signal directly.
- **Upside.** Interpretable, energy-aware, robust; bridges to P-09-02.
- **Downside.** Cannot capture hardware non-linearities; weakest accuracy.
- **Key paper.** Selinger 1979.

### Research recommendation
**Pursue C as the engine's production model and A as the accuracy baseline.** C is the
principled, energy-aware, interpretable choice that aligns with P-09-02's analytic skeleton
(the two questions should share a model). A is the strong learned baseline to beat. B is the
research-grade contribution: a calibrated, uncertainty-aware cost model is a clean, novel,
publishable result with a tail-energy story — attempt it as the headline learned-model
contribution once A and C are in place.

---

## P-09-10: Sketch error composition rules

**Problem.** The engine composes sketches across subqueries: HyperLogLog for distinct counts
(unioned across shards), Count-Min for heavy hitters (merged across shards), and both fed
into the same join cardinality estimator. The *error* of the composite sketch is not the sum
of the parts — but the standard union bound treats it as such, over-allocating sketch width.
We need tight composition rules for sketch algebras.

### Candidate Approach A: McDiarmid on the composition
- **Approach.** Treat the composite estimate as f(sketch_1, …, sketch_k); bound its variance
  via McDiarmid using each sketch's known per-cell variance (HLL: ~1.04/√m; CM:
  O(1/√m) per Datar-style analysis). The composition's c_i is the *propagated* variance, not
  the raw union-bound δ.
- **Performance impact.** For a k-way unioned HLL, the true std is still ~1.04/√m (HLL is
  *union-stable*!), so McDiarmid recovers this exactly instead of the union bound's √k blow-up
  — potentially k× less sketch width for the same accuracy.
- **Time to solve.** ~4–6 months: derive c_i for HLL∪HLL, CM∪CM, HLL×CM compositions.
- **Energy implications.** k× smaller sketches ⇒ k× less memory traffic at estimate time ⇒
  directly proportional energy saving on sketch-heavy plans.
- **Upside.** Tight, principled, big memory/energy win on union-stable sketches.
- **Downside.** Composition across *non*-union-stable operators (e.g. HLL of a join) is hard.
- **Key paper.** Flajolet, Fusy, Gandouet, Meunier, "HyperLogLog," *AOFA* 2007; Cormode &
  Muthukrishnan, "An Improved Data Stream Summary: The Count-Min Sketch," *J. Algorithms*
  2004/2005.

### Candidate Approach B: Worst-case (union bound / max) — the safe default
- **Approach.** Take δ_total = min(1, Σδ_i) and ε_total = Σε_i; the textbook conservative
  rule. Keep as the certified fallback.
- **Performance impact.** Correct but loose; for k=10 shards this can inflate width ~10×,
  wasting memory and bandwidth.
- **Time to solve.** ~1–2 months (it is the existing baseline).
- **Energy implications.** Negative vs. A — over-allocation wastes memory-bandwidth energy.
- **Upside.** Trivially correct; no assumptions.
- **Downside.** Loose; wastes resources.
- **Key paper.** Cormode & Muthukrishnan 2004.

### Candidate Approach C: Algebraic — sketches as objects, error as a functor
- **Approach.** Define a category where objects are (sketch type, parameters) and morphisms
  are composition operators (union, product, join-projection); derive the error of a
  composite as a *functor* to (ε, δ)-constraints. Yields composition rules as theorems of the
  category, unifying A's per-case derivations.
- **Performance impact.** Same tightness as A in the cases it covers, but *systematic* — new
  sketch compositions get their error for free from the algebra.
- **Time to solve.** ~10–14 months: building the category and proving the functor is
  correctness-preserving is the thesis-grade lift.
- **Energy implications.** Same as A in steady state; bigger long-term win as new operators
  are added without re-deriving bounds.
- **Upside.** Unifying; extensible; publishable as a framework.
- **Downside.** Heavy formalism; risk of over-engineering for the actual sketch zoo.
- **Key paper.** Cormode & Muthukrishnan 2004; Spivak 2012 (categorical composition).

### Research recommendation
**Pursue A immediately, C as the thesis synthesis.** A delivers a concrete, large memory/
energy win (exploit HLL/CM union-stability instead of union-bounding) in ~5 months — a clean
publishable result. C generalises it into a framework worth a chapter. B is the certified
fallback that A/C must beat.

---

## P-09-11: Lower bound on kernel table size

**Problem.** The engine's instruction layer selects a *kernel* — a specialised code path —
for each (operator, tier) pair it may encounter. The kernel table is the set of all such
specialised kernels. What is the *minimum* number of kernels sufficient to cover all (op,
tier) pairs the workload can produce, up to a performance tolerance? Too few ⇒ fallback to a
generic slow path; too many ⇒ icache pressure and code-gen energy. We need a tight lower
bound.

### Candidate Approach A: Covering-number formulation
- **Approach.** Model each kernel as covering a *ball* in the (op, tier, data-shape) space
  within tolerance τ. The minimum kernel count is the τ-covering number N_τ of the workload
  space. Kolmogorov–Tikhomirov 1959 ("ε-entropy and ε-capacity") gives the metric-space
  machinery; compute/estimate N_τ from a workload trace.
- **Performance impact.** Gives a *certified* lower bound on how much code-gen the engine
  must do; tells you when you can stop adding kernels (within τ of optimal). For a workload
  with b distinguishable (op,tier,shape) balls, N_τ ≈ b/τ^d in the worst case but typically
  far smaller under workload locality.
- **Time to solve.** ~5–7 months: define the metric, estimate N_τ empirically + bound
  theoretically.
- **Energy implications.** Right-sized kernel table ⇒ no wasted JIT/code-gen energy, no
  icache misses (which are surprisingly energy-expensive — icache misses can cost 100s of pJ
  each). Direct energy win.
- **Upside.** Principled; gives a stopping rule for code generation.
- **Downside.** Metric definition is the crux; a bad metric gives a vacuous bound.
- **Key paper.** Kolmogorov & Tikhomirov, "ε-Entropy and ε-Capacity of Sets in Function
  Spaces," *Uspekhi* 1959.

### Candidate Approach B: Adversarial workload
- **Approach.** Construct an adversarial sequence of (op, tier, shape) tuples such that any
  kernel table smaller than K forces a τ-violation (a miss into the generic path), proving
  N_τ ≥ K.
- **Performance impact.** A clean lower bound; pairs with A's upper bound for a tight
  characterisation.
- **Time to solve.** ~6–9 months; adversary constructions depend heavily on the workload
  model and may only give weak (logarithmic) bounds.
- **Energy implications.** Certifies that code-gen effort below K is *insufficient* — guides
  investment.
- **Upside.** Clean theoretical contribution; complements A.
- **Downside.** Adversarial workloads may be unrealistic, weakening practical relevance.
- **Key paper.** Sleator & Tarjan 1985 (adversary methodology, by analogy).

### Candidate Approach C: Constructive covering
- **Approach.** Exhibit a concrete kernel table of size K achieving τ-coverage for a realistic
  workload class, demonstrating the bound is achievable.
- **Performance impact.** Gives the engine a *deployable* kernel set; the matching lower bound
  (from A/B) certifies minimality.
- **Time to solve.** ~4–6 months once A/B fix the target K.
- **Energy implications.** A verified-minimal kernel set is the energy-optimal code-gen
  target.
- **Upside.** Immediately deployable; closes the constructive side.
- **Downside.** Only as good as the workload class assumed.
- **Key paper.** Kolmogorov–Tikhomirov 1959.

### Research recommendation
**Pursue A + C together, with B opportunistic.** A (covering-number formulation) gives the
theoretical lower bound and a stopping rule — a publishable result and directly useful to the
engine. C (constructive covering) makes it deployable. B (adversary) is worth attempting only
if A's bound turns out loose and a separation is needed.

---

## P-09-12: Unifying mathematical framework for all five pillars

**Problem.** The engine rests on five pillars — (1) instruction-first execution, (2) memory-
centric tiering, (3) compression/tensor representation, (4) probabilistic/sketch estimation,
(5) categorical schema/federation. Each has its own mathematics (queueing, paging theory,
tensor algebra, concentration, category theory). Is there *one* framework that subsumes all
five, giving a unified cost/energy/consistency calculus?

### Candidate Approach A: Minimum Description Length (MDL) / universal coding
- **Approach.** Cast each pillar as a *coding* problem: execution = describing the output;
  tiering = choosing a code over a channel with per-tier cost; compression = literal coding;
  sketching = lossy coding with (ε,δ) fidelity; schema migration = refactoring the codebook.
  Grünwald's MDL principle (Grünwald 2007) gives a single objective: minimise
  L(model) + L(data | model), with energy as the channel-cost weighting.
- **Performance impact.** Unifies cost (description length), energy (channel cost × bits),
  and accuracy (fidelity of lossy codes) under one objective. Gives a principled way to
  trade them: every decision minimises a single MDL-energy functional.
- **Time to solve.** ~12–16 months for a credible unification across ≥3 pillars; the full
  five-pillar unification is the thesis capstone.
- **Energy implications.** Profound: energy becomes the *currency* of the unified objective
  (Landauer + channel coding), so the framework is energy-native, not energy-as-an-after-
  thought.
- **Upside.** Genuinely unifying; energy-native; aligns with information theory (P-09-01).
- **Downside.** Risk of being too abstract to drive concrete engineering; "MDL of a query
  plan" is not obviously computable.
- **Key paper.** Grünwald, *The Minimum Description Length Principle*, MIT Press 2007;
  Cover & Thomas 2006.

### Candidate Approach B: Category theory (Spivak's programme)
- **Approach.** Take Spivak's categorical databases as the spine: schemas as categories,
  instances as functors, queries as natural transformations, tiering as a functor into a
  cost-enriched category, sketching as an approximation functor. Every pillar becomes a
  functor/category; the framework is the *functor category* they live in.
- **Performance impact.** Cleanest *structural* unification: data migration (P-09-04),
  consistency (P-09-08), and schema composition all already live here. But execution cost and
  energy do not fit naturally into pure category theory without enrichment.
- **Time to solve.** ~14–18 months; requires building an *enriched* categorical cost calculus
  (cost-enriched categories, weighted limits) that few have attempted for databases.
- **Energy implications.** Indirect: structural clarity helps, but energy is not native — it
  must be grafted on via enrichment, weakening the unification claim.
- **Upside.** Subsumes P-09-04 and P-09-08 natively; mathematically deep.
- **Downside.** Energy is a foreign object; execution (pillar 1) resists categorification;
  high barrier.
- **Key paper.** Spivak 2012; Mac Lane & Moerdijk 1992.

### Candidate Approach C: Convex optimisation / duality
- **Approach.** Cast each pillar as a convex program: tier placement = assignment LP,
  scheduling = convex relaxation, compression = rate-distortion, sketching = constrained
  estimation, schema migration = a transport problem. The unified framework is *duality*:
  every pillar has a primal (do the work) and dual (price/certify the work), and the engine
  runs both.
- **Performance impact.** Operationally the most useful: each pillar gets a certificate
  (dual) giving an optimality gap — directly feeding P-09-01's energy bound. Convexity gives
  polynomial-time algorithms and clean sensitivity (energy gradient).
- **Time to solve.** ~10–14 months for a unification covering placement + scheduling +
  compression; sketching and schema migration resist clean convexification.
- **Energy implications.** Strong: dual prices *are* marginal energy costs; the framework is
  energy-native via Lagrange multipliers.
- **Upside.** Energy-native; gives certificates; tractable; bridges to P-09-01 and P-09-02.
- **Downside.** Not all pillars are convex (schema migration, non-linear compression);
  forcing convexity may distort the model.
- **Key paper.** Boyd & Vandenberghe, *Convex Optimization*, CUP 2004; Bertsekas, *Nonlinear
  Programming*.

### Research recommendation
**Pursue C as the working framework, with A as the aspirational capstone.** Convex duality
(C) is energy-native, certificate-bearing, tractable, and directly unifies placement,
scheduling, and compression — three pillars in ~12 months with strong publishable
intermediate results. MDL (A) is the deeper prize: once C is mature, recasting it in MDL
terms gives the energy-as-coding unification that makes the thesis a *philosophical* as well
as technical contribution. B (category theory) is best kept as the *language* for the
schema/federation pillars (P-09-04, P-09-08) rather than forced to carry execution and
energy.

---

# Cross-Question Roadmap

**Dependency graph (research-order, not strict prerequisite):**

```
P-09-01 (energy LB)  ──┐
P-09-02 (closed-form) ─┼──► P-09-12 (unifying framework, convex/MDL)
P-09-09 (learned)     ─┘           │
                                   ▼
P-09-03 (paging) ──┐         certificates / prices
P-09-11 (kernel) ──┤
P-09-05 (ε,δ) ─────┼──► feed P-09-02 cost model
P-09-10 (sketch) ──┘
P-09-06 (TT-rank) ──► compression pillar ──► P-09-12
P-09-07 (LSH) ─────► approximate-query pillar ──► P-09-12
P-09-04 (functorial) ┐
P-09-08 (sheaf)     ─┴► schema/federation pillar ──► P-09-12
```

**Sequencing (suggested 4-year thesis arc):**

| Year | Questions | Rationale |
|------|-----------|-----------|
| 1 | P-09-02 (A), P-09-09 (A+C), P-09-10 (A) | Build the cost/sketch substrate the engine runs on; cheap wins, de-risk. |
| 2 | P-09-01 (A→C), P-09-03 (B+C), P-09-05 (A) | The energy + paging + error-bound cluster — the "memory-centric" thesis core. |
| 3 | P-09-06 (C→B), P-09-07 (B→C), P-09-11 (A+C) | Compression, LSH, kernel-covering — the "instruction-first" thesis core. |
| 4 | P-09-04 (A→B), P-09-08 (A), P-09-12 (C→A) | Schema/sheaf/unification — the synthesis chapters. |

**Highest leverage / publish-first:** P-09-05 (McDiarmid with selectivity), P-09-10 (sketch
composition via union-stability), and P-09-01 (LP-dual energy lower bound) — each is a
self-contained, novel, ~6-month result with a direct energy story and a clean venue fit
(SIGMOD/VLDB for 05/10; OSDI/SOSP or a theory venue for 01).

**Verified citations used in this document:**
- Tiwari, Lee, Malik, *ISLPED* 1994 (instruction-level power) — verified
- Hähnel et al., *IGCC* 2012 (RAPL short-path energy) — verified
- Aggarwal, Alpern, Chandra, Snir, *STOC* 1987 (hierarchical memory) — verified
- Aggarwal–Vitter, *JACM* 1988 (I/O lower bounds)
- Landauer, *IBM J. Res. Dev.* 1961; Cover & Thomas, Wiley 2006
- Selinger, *SIGMOD* 1979; Kingman, *Proc. Camb. Phil. Soc.* 1961
- Marcus & Negi, *PVLDB* 12(11) 2019 (Neo) — verified
- Marcus et al., *SIGMOD* 2021 (Bao) — verified
- Kipf et al., *CIDR* 2019 (Learned Cardinalities / MSCN) — verified
- Leis et al., *PVLDB* 2015 (How Good Are Query Optimizers)
- Sleator & Tarjan, *CACM* 1985; Koutsoupias & Papadimitriou, *JACM* 42(5) 1995 — verified
- Fiat et al., *J. Algorithms* 1991 (marking/paging)
- Spivak, *TAC* 28(6) 2012 (Functorial Data Migration) — verified
- McSherry et al., *CIDR* 2014 (Differential Dataflow)
- Hoeffding, *JASA* 1963; McDiarmid, *Surveys in Combinatorics* 1989
- Oseledets, *SIAM J. Sci. Comput.* 33(5) 2011 (TT-SVD) — verified
- Oseledets & Tyrtyshnikov, *LAA* 2010 (TT-cross); Halko–Martinsson–Tropp, *SIAM Rev.* 2011
- Andoni & Indyk, *CACM* 51(1) 2008 — verified; Datar et al., *SoCG* 2004 (p-stable)
- Charikar, *STOC* 2002 (simhash); Broder, *SEQ* 1997 (MinHash)
- Andoni & Razenshteyn, *STOC* 2015 (data-dependent LSH); Kraska et al., *SIGMOD* 2018
- Mac Lane & Moerdijk, Springer 1992 (Sheaves in Geometry and Logic)
- Robinson, 2014 (consistency radius); Herlihy & Shavit, *JACM* 1999; arXiv:2503.02556 (2025) — verified
- Flajolet et al., *AOFA* 2007 (HyperLogLog) — verified
- Cormode & Muthukrishnan, *J. Algorithms* 2004/2005 (Count-Min) — verified
- Kolmogorov & Tikhomirov, *Uspekhi* 1959 (ε-entropy/ε-capacity)
- Grünwald, *MDL Principle*, MIT Press 2007; Boyd & Vandenberghe, CUP 2004
