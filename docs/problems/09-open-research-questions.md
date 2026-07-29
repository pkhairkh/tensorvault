# Open Research Questions

> The hardest unsolved problems — the ones that, if solved, would constitute
> a publishable research contribution. These are the questions a PhD student
> could build a thesis around.
>
> Each question is framed as a formal problem statement, with context from
> the research documents and a sketch of why it's hard.

---

## P-09-01: Is there a tight lower bound on energy-per-query? 🔴

**Layer**: Research
**Status**: 🔴 open
**Math**: I (information theory), III (probability)
**Effort**: XL (PhD-thesis-scale)
**Impact**: critical (theoretical foundation)

### Problem statement

Given a query Q over data D stored in a memory hierarchy with tiers
{T₁, ..., Tₖ} (each with latency Lᵢ, bandwidth Bᵢ, energy Eᵢ), what is the
minimum energy required to answer Q?

Formally:

$$
E^*(Q, D, \mathcal{T}) = \min_{\text{plans } P} \sum_{\text{ops in } P} E(\text{op}, \text{tier}(\text{op}))
$$

### Why it's hard

- The energy of an operation depends on the CPU's microarchitecture (which
  instructions are used, cache hit rates, branch prediction)
- The optimal plan depends on the data distribution (cardinality, skew)
- The tiers interact (spilling to CXL saves DDR5 energy but costs CXL energy)

### Why it matters

A tight lower bound would:
1. Tell us how far we are from optimal (the "energy gap")
2. Guide kernel design (which instructions to use)
3. Provide a principled objective for the planner

### Approach

1. Model each kernel as a function: (input size, tier) → (latency, energy)
2. Model the memory hierarchy as a queueing network (Kingman + M/M/c)
3. Formulate as an LP: minimize total energy subject to latency constraints
4. Prove the LP solution is a lower bound (via duality)

### Related work

- Tiwari et al. 1994 (instruction-level energy modeling)
- Hahnel et al. 2012 (RAPL measurement)
- `docs/cpu_energy_kb.md` (our knowledgebase)

---

## P-09-02: Can we derive a closed-form cost model for common query shapes? 🔴

**Layer**: Research
**Status**: 🔴 open
**Math**: III (Kingman), I (instruction throughput)
**Effort**: XL
**Impact**: critical

### Problem statement

For common query shapes (scan, filter, aggregate, join), derive a
closed-form expression for the query latency as a function of:
- Data size n
- Tier parameters (latency L, bandwidth B, utilization ρ, variability c_a, c_s)
- Kernel throughput t (cells/cycle)
- CPU frequency f

For example, for a scan over n cells in tier T:

$$
T_{\text{scan}}(n, T) = \frac{n}{t \cdot f} + W_{\text{Kingman}}(\rho, c_a, c_s, \mu)
$$

### Why it's hard

- The two terms (compute and queueing) interact: high utilization slows
  both
- Pipeline effects (scan → filter → aggregate) don't compose additively
- Cache effects (L3 hit rate depends on working set size) are nonlinear

### Why it matters

A closed-form cost model would make the planner O(1) per plan evaluation
(vs current O(n) simulation). This unlocks:
- Exhaustive plan enumeration for large queries
- Real-time adaptive execution
- Formal optimality proofs

### Approach

1. Calibrate per-kernel throughput via measurement (P-01-07)
2. Fit Kingman parameters per tier via saturation benchmarks (P-08-07)
3. Derive composition rules for pipelines (additive? max? harmonic mean?)
4. Validate against TPC-H queries

### Related work

- Kingman 1961 (queueing approximation)
- Selinger 1979 (cost-based optimization)
- `docs/tpcc_math.md` (our cost model derivation)

---

## P-09-03: What is the competitive ratio of multi-tier paging? 🔴

**Layer**: Research
**Status**: 🔴 open
**Math**: IV (online algorithms)
**Effort**: L
**Impact**: high

### Problem statement

The k-server problem has a (2k-1)-competitive algorithm (WFA, Koutsoupias-
Papadimitriou 1995) for general metric spaces. But our setting is a
**layered metric space** (L3 → DDR5 → CXL → NVMe), where:
- Distances are asymmetric (L3 → DDR5 is cheap; DDR5 → L3 requires copying)
- Capacities are tier-dependent (L3 is small; NVMe is large)
- Costs are mixed (latency + energy + bandwidth)

What is the optimal competitive ratio for this structured metric space?

### Why it's hard

- The layered structure may allow better than (2k-1)-competitive
- But the asymmetry may make it worse
- No existing theory covers this exact setting

### Why it matters

A better competitive ratio means our migration policy (P-02-04) has a
tighter performance guarantee. This translates directly to better p99
latency under variable workloads.

### Approach

1. Formalize the layered metric space
2. Check if WFA specializes nicely
3. Look for a tailored algorithm (e.g., "promote on k-th access" might be
   better than LRU)
4. Prove lower bounds via adversarial constructions

### Related work

- Sleator-Tarjan 1985 (LRU is k-competitive)
- Koutsoupias-Papadimitriou 1995 (WFA is (2k-1)-competitive)
- `docs/research/optimization_theory_db.md` §12

---

## P-09-04: Can functorial data migration be made practical? 🔴

**Layer**: Research
**Status**: 🔴 open
**Math**: V (category theory — Kan extensions)
**Effort**: XL
**Impact**: medium

### Problem statement

Spivak's functorial data migration (Σ ⊣ Δ ⊣ Π) gives a mathematically
principled way to migrate data between schemas. But the existing
implementation (CQL) is slow and doesn't scale to production databases.

Can we make it practical for real-world schema evolution?

### Why it's hard

- Computing Π_F (right Kan extension) requires a colimit, which can be
  expensive
- Real schemas have constraints (foreign keys, uniqueness) that don't
  fit cleanly into the categorical framework
- Incremental migration (without scanning the whole database) is unclear

### Why it matters

Schema evolution is a pain point in every production database. A
principled, correct, fast migration tool would be a significant contribution.

### Approach

1. Restrict to a useful subclass of schemas (e.g., acyclic foreign key graphs)
2. Use incremental computation (differential dataflow) for the migration
3. Compile migrations to SQL DDL + DML for execution on existing engines
4. Prove correctness via the adjunction laws

### Related work

- Spivak 2012 (functorial data migration)
- CQL implementation (CategoricalData/CQL on GitHub)
- `docs/research/category_theory_topology_db.md` §1-2

---

## P-09-05: Is there a tight bound for (ε, δ) propagation through joins? 🔴

**Layer**: Research
**Status**: 🔴 open
**Math**: III (concentration inequalities)
**Effort**: L
**Impact**: high

### Problem statement

When composing approximate operators (see P-05-04), the naive bound is the
union bound: δ_total = δ₁ + δ₂ + ... + δₙ. This is loose.

For joins specifically, the error depends on the join's selectivity. A
high-selectivity join (many matches) averages out errors; a low-selectivity
join (few matches) amplifies them.

What is the tight bound for (ε, δ) propagation through a join?

### Why it's hard

- The join's selectivity depends on the data, not just the query
- Correlated errors (the same sample used in both sides of the join)
  violate the independence assumption
- Skewed data breaks the averaging argument

### Why it matters

A tighter bound means we can use smaller samples (faster queries) for the
same (ε, δ) guarantee. This directly improves the engine's approximate
query performance.

### Approach

1. Model the join as a function of two approximate inputs
2. Use McDiarmid's inequality (bounded differences) with the join's
   selectivity as the bound constant
3. Validate empirically on TPC-H joins
4. Look for a closed-form bound for equi-joins

### Related work

- Hoeffding 1963, McDiarmid 1989 (concentration inequalities)
- `docs/research/probability_sketching_for_db.md` §1, §12

---

## P-09-06: Can we compute the tensor train rank of a database table? 🔴

**Layer**: Research
**Status**: 🔴 open
**Math**: II (tensor decomposition)
**Effort**: XL
**Impact**: medium

### Problem statement

The tensor train (TT) decomposition compresses a d-dimensional tensor from
O(nᵈ) to O(d·n·r²) where r is the TT-rank. For a database table with d
columns, the TT-rank determines the compression ratio.

But computing the TT-rank of a given table is NP-hard in general. Can we:
1. Estimate it efficiently?
2. Find tables where TT compression is beneficial?
3. Compute the TT decomposition incrementally as data arrives?

### Why it's hard

- TT-rank depends on the data's correlation structure
- Computing it exactly is NP-hard
- Incremental TT (for streaming data) is an open problem

### Why it matters

A practical TT compression for multi-column data would give 10–100×
compression on highly correlated columns (common in real databases).

### Approach

1. Use randomized SVD (Halko-Martinsson-Tropp) to estimate the rank
2. Fit a TT decomposition on a sample, validate on the full table
3. Measure the rank distribution on real datasets (TPC-H, TPC-C, real logs)

### Related work

- Oseledets 2011 (tensor train decomposition)
- `docs/research/spectral_db_research.md` §3

---

## P-09-07: What is the right LSH parameters for arbitrary column types? 🔴

**Layer**: Research
**Status**: 🔴 open
**Math**: III (LSH theory)
**Effort**: L
**Impact**: medium

### Problem statement

LSH (locality-sensitive hashing) gives sublinear approximate nearest
neighbor search. The theory is well-developed for Hamming, Euclidean, and
Jaccard distances.

But for arbitrary column types (dates, UUIDs, JSON blobs), the right LSH
family is unclear. We're using Hamming LSH on the raw 64-bit bit pattern,
but is this optimal?

### Why it's hard

- The "distance" on arbitrary types is not always a metric
- The LSH collision probability depends on the distance distribution
- The optimal LSH family depends on the query distribution

### Why it matters

Better LSH means faster similarity joins (P-08-04). If we can prove a
better-than-Hamming LSH for, say, UUIDs, we'd get 10× speedup on UUID
similarity joins.

### Approach

1. Define the distance on each column type (Hamming on bits? Edit distance
   on strings? Cosine on embeddings?)
2. Apply the Andoni-Indyk framework to find the optimal LSH family
3. Validate empirically on real data

### Related work

- Andoni-Indyk 2008 (near-optimal LSH)
- `docs/research/probability_sketching_for_db.md` §3

---

## P-09-08: Can we formalize distributed consistency as a sheaf? 🔴

**Layer**: Research
**Status**: 🔴 open
**Math**: V (sheaf theory)
**Effort**: XL
**Impact**: medium

### Problem statement

Model the engine's distributed state as a sheaf over the topology of
nodes/racks/regions. Consistency levels (strong, eventual, causal)
correspond to sheaf conditions (gluing, local-to-global).

### Why it's hard

- Sheaf theory is abstract; mapping it to concrete protocols is non-trivial
- The topology changes as nodes fail and recover
- Sheaf cohomology (for detecting inconsistency) may be computationally
  expensive

### Why it matters

A sheaf-theoretic model would give a unified framework for reasoning about
consistency across tiers and protocols. It could reveal new consistency
models (between strong and eventual) and provide formal proofs of
correctness.

### Approach

1. Define the sheaf: each node's local state is a section
2. Define the topology: racks, regions, the global view
3. Map consistency levels to sheaf conditions
4. Use sheaf cohomology to detect inconsistency

### Related work

- Mac Lane-Moerdijk 1992 (sheaves in geometry and logic)
- Robinson 2014 (sheaves of data)
- `docs/research/category_theory_topology_db.md` §10

---

## P-09-09: Can the cost model be learned (not derived)? 🔴

**Layer**: Research
**Status**: 🔴 open
**Math**: IV (online optimization, learning theory)
**Effort**: L
**Impact**: high

### Problem statement

Instead of deriving a closed-form cost model (P-09-02), can we learn it
from observed query executions? The learned model would predict query
latency from plan features (kernel mix, data sizes, tier utilization).

### Why it's hard

- The feature space is large (kernels, tiers, data distributions, hardware)
- The model must be online (adapt as the workload changes)
- The model must be calibrated (not just accurate on average, but
  uncertainty-aware)

### Why it matters

A learned cost model could outperform a hand-derived one (which is
necessarily approximate). It also adapts to new hardware automatically.

### Approach

1. Collect (plan, latency) pairs from query execution
2. Train a gradient-boosted model (or Bayesian neural network for
   uncertainty)
3. Use the model in the planner
4. Validate against the hand-derived model (P-05-03)

### Related work

- Marcus et al. 2019 (Neo — learned query optimizer)
- Kipf et al. 2019 (learned cardinality estimation)
- `docs/research/optimization_theory_db.md` §13

---

## P-09-10: What is the right composition rule for sketch errors? 🔴

**Layer**: Research
**Status**: 🔴 open
**Math**: III (concentration inequalities, sketch theory)
**Effort**: L
**Impact**: medium

### Problem statement

When two sketches are composed (e.g., HLL inside Count-Min), the error
bound is not simply the sum. What is the right composition rule?

For example:
- HLL has RSE 1.04/√m
- Count-Min has error ε with probability 1-δ
- HLL inside Count-Min: what's the combined (ε, δ)?

### Why it's hard

- The errors are not independent (the same hash function may be used)
- The composition depends on the order (HLL(CM(x)) ≠ CM(HLL(x)))
- Some compositions are exact (HLL of HLL = HLL); others are lossy

### Why it matters

A sketch algebra with tight composition rules would let the planner
automatically pick the minimal sketch for a complex query.

### Approach

1. Catalog common sketch compositions
2. Derive bounds for each (using McDiarmid, union bound, or tailored analysis)
3. Build a "sketch algebra" that composes bounds automatically
4. Validate empirically

### Related work

- Cormode-Muthukrishnan 2004 (Count-Min)
- Flajolet et al. 2007 (HyperLogLog)
- `docs/research/probability_sketching_for_db.md` §2, §12

---

## P-09-11: Can we prove a lower bound on the kernel table size? 🔴

**Layer**: Research
**Status**: 🔴 open
**Math**: I (information theory — lower bounds)
**Effort**: M
**Impact**: medium

### Problem statement

The kernel table has one entry per (operator, CPU, tier) tuple. As we add
operators, CPUs, and tiers, the table grows. Is there a lower bound on how
small it can be while still covering all (operator, tier) combinations?

### Why it's hard

- Some kernels are reusable across tiers (the scalar fallback works
  everywhere, just slowly)
- Some kernels are tier-specific (the CXL kernel needs different prefetching)
- The right tradeoff between table size and kernel generality is unclear

### Why it matters

A lower bound would tell us the minimum engineering effort to support a new
CPU or tier. It would also guide the kernel table's design (when to add a
new kernel vs reuse an existing one).

### Approach

1. Model the kernel table as a covering problem
2. Prove a lower bound via adversarial construction (for each missing
   kernel, construct a workload where it's needed)
3. Check if the bound is tight (construct a kernel table achieving it)

### Related work

- Covering problems in combinatorics
- `docs/cpu_energy_kb.md` (per-instruction analysis)

---

## P-09-12: Is there a unifying mathematical framework for the engine? 🔴

**Layer**: Research
**Status**: 🔴 open
**Math**: all five pillars
**Effort**: XL (PhD-thesis-scale)
**Impact**: critical (theoretical unification)

### Problem statement

The engine uses techniques from 5 mathematical pillars (info theory,
spectral, probability, optimization, category theory). Is there a single
framework that unifies them?

The hypothesis (from `docs/math_foundations.md`): the unifying framework is
**universal coding length minimization** on a **tiered queueing system**,
typed by **linear logic**, migrated by **Kan-extension functors**.

Can this be formalized?

### Why it's hard

- The pillars use different mathematical languages (probability vs algebra
  vs category theory)
- Unifying them requires finding a common abstraction
- The unification must be useful (not just philosophical)

### Why it matters

A unifying framework would:
1. Make the engine's design principled (not ad-hoc)
2. Reveal connections between seemingly unrelated problems
3. Provide a foundation for proving global properties (e.g., "the engine
   is correct" or "the engine is optimal")

### Approach

1. Identify the common structure (coding length? optimization? types?)
2. Formalize the engine as an instance of that structure
3. Prove that each pillar is a specialization
4. Use the framework to derive new results

### Related work

- `docs/math_foundations.md` (our synthesis)
- The entire research corpus

---

## Summary

| # | Problem | Status | Math | Effort | Impact |
|---|---------|--------|------|--------|--------|
| 01 | Tight lower bound on energy-per-query | 🔴 | I+III | XL | critical |
| 02 | Closed-form cost model for query shapes | 🔴 | III+I | XL | critical |
| 03 | Competitive ratio of multi-tier paging | 🔴 | IV | L | high |
| 04 | Practical functorial data migration | 🔴 | V | XL | medium |
| 05 | Tight (ε, δ) propagation through joins | 🔴 | III | L | high |
| 06 | TT-rank of database tables | 🔴 | II | XL | medium |
| 07 | Right LSH for arbitrary column types | 🔴 | III | L | medium |
| 08 | Sheaf-theoretic distributed consistency | 🔴 | V | XL | medium |
| 09 | Learned cost model | 🔴 | IV | L | high |
| 10 | Sketch error composition rules | 🔴 | III | L | medium |
| 11 | Lower bound on kernel table size | 🔴 | I | M | medium |
| 12 | Unifying mathematical framework | 🔴 | all | XL | critical |

**These 12 questions define the research frontier of the engine.** Each is
publishable; together they would constitute a comprehensive theory of
instruction-first, memory-centric databases.
