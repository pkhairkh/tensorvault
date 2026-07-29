# Mathematical Problems

> Open mathematical problems that, if solved, would directly enhance the engine.
> Each problem is tied to one of the five pillars from `docs/math_foundations.md`
> and has a concrete engineering payoff.
>
> **Pillar legend**: I = information theory, II = spectral graph theory,
> III = probability & sketching, IV = optimization, V = category theory.

---

## P-05-01: MDL schema selection — closed-form or LP? 🟡

**Layer**: Math (Pillar I)
**Status**: 🟡 partial (greedy selector exists in `src/schema/mdl.rs`)
**Math**: I (MDL, Kolmogorov complexity)
**Effort**: M
**Impact**: high

### Problem

The MDL schema selector currently enumerates all candidate type
interpretations and picks the one with minimum description length. This is
O(|T|) per column where |T| is the number of candidate types.

Can we do better? The MDL objective is:

$$
\mathcal{L}(\tau) = \underbrace{L(\tau)}_{\text{model cost}} + \underbrace{L(\text{data} \mid \tau)}_{\text{data cost}}
$$

For a fixed set of candidate types, this is a discrete optimization
problem. For continuous parameters (e.g., the threshold for "is this column
f64 or i32?"), it becomes an LP or NLP.

### Open questions

- Is there a closed-form solution for common type-detection problems
  (e.g., "is this column f64 or i32?")?
- Can we formulate multi-column MDL (correlated columns) as an LP?
- How does this connect to the Kolmogorov complexity of the data (which is
  uncomputable but approximated by MDL)?

### Success criteria

- A formal proof that the greedy selector is optimal for single-column MDL.
- An LP formulation for multi-column MDL.

---

## P-05-02: Spectral partitioning for NUMA placement 🔴

**Layer**: Math (Pillar II)
**Status**: 🔴 open
**Math**: II (Cheeger's inequality, spectral sparsification)
**Effort**: L
**Impact**: high

### Problem

Given a table and its access pattern (which rows are accessed together),
partition the table across NUMA nodes to minimize cross-node traffic.

Cheeger's inequality relates the graph's expansion (how well it can be
partitioned) to the second eigenvalue of the Laplacian:

$$
\frac{1}{2}\lambda_2 \le \phi \le \sqrt{2\lambda_2}
$$

The spectral partitioning algorithm:
1. Build the access graph (nodes = rows, edges = co-accessed)
2. Compute the Laplacian L = D - A
3. Find the Fiedler vector (eigenvector of λ₂)
4. Partition by the sign of the Fiedler vector

### Open questions

- How do we build the access graph efficiently? (From query logs? From
  static analysis of the schema?)
- Can we use Spielman-Srivastava spectral sparsification (see
  `docs/research/spectral_db_research.md` §7) to make the Laplacian solve
  faster?
- How does this interact with the region migration policy (P-02-04)?

### Success criteria

- A `SpectralPartitioner` that takes an access graph and returns a NUMA
  assignment.
- Benchmark: cross-NUMA traffic reduced by > 50% vs round-robin placement.

---

## P-05-03: Closed-form cost model (Kingman + AVX-512) 🔴

**Layer**: Math (Pillars III + I)
**Status**: 🔴 open
**Math**: III (Kingman's formula), I (instruction throughput)
**Effort**: XL (6+ months)
**Impact**: critical

### Problem

This is the **must-solve** math problem. The planner needs a cost model
that predicts query latency from:
1. The kernel's instruction throughput (cells/cycle)
2. The memory tier's queueing latency (Kingman's formula)
3. The CPU's clock frequency and SIMD width

The combined cost model:

$$
T_{\text{query}} = \sum_{\text{kernels}} \left( \frac{n_{\text{cells}}}{\text{throughput}(\text{kernel}, \text{tier}) \cdot f_{\text{cpu}}} + W_{\text{Kingman}}(\text{tier}) \right)
$$

where:
- `throughput(kernel, tier)` depends on the AVX-512 instructions used and
  the tier's bandwidth
- `W_Kingman(tier)` is the predicted queueing delay from Kingman's formula
- `f_cpu` is the CPU clock frequency (which may be throttled by AVX-512
  license levels — see P-01-03)

### Open questions

- Is the cost model additive across kernels, or are there pipeline effects?
- How do we calibrate the model? (Measure on real hardware, fit parameters.)
- Can we derive a closed-form for common query shapes (scan, filter,
  aggregate, join)?

### Success criteria

- A `CostModel` struct that predicts query latency from the plan.
- Predicted latency within 20% of measured on TPC-H queries.
- The planner uses the cost model to pick the optimal plan.

---

## P-05-04: (ε, δ) propagation through the operator DAG 🔴

**Layer**: Math (Pillar III)
**Status**: 🔴 open
**Math**: III (concentration inequalities — Hoeffding, McDiarmid)
**Effort**: L
**Impact**: critical

### Problem

This is the **must-solve** problem for approximate queries (see
`06-query-syntax-approach.md`). When the user requests
`SELECT AVG(price) APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99`, the planner
must propagate the (ε, δ) guarantee through the operator DAG.

For example:
- Scan: no error introduced
- Filter: changes the sample size, updates δ
- Aggregate (AVG): Hoeffding bound gives ε from sample size and δ
- Join: union bound on δ

The propagation rules:

| Operator | ε propagation | δ propagation |
|----------|--------------|---------------|
| Scan | ε (passthrough) | δ (passthrough) |
| Filter | ε / √(selectivity) | δ (passthrough) |
| Aggregate (AVG) | √(ln(2/δ) / (2n)) | δ (input) |
| Join | ε₁ + ε₂ | δ₁ + δ₂ (union bound) |
| Sort | ε (passthrough) | δ (passthrough) |

### Open questions

- Are there tighter bounds than union bound for joins?
- How do we handle correlated errors (e.g., the same sample used in multiple
  aggregates)?
- Can we use McDiarmid's inequality for complex operator DAGs?

### Success criteria

- A `ConfidencePropagator` that computes (ε, δ) for each node in the plan.
- The planner picks the minimal-cost sketch whose theorem matches the
  requested (ε, δ).
- Formal proof that the propagated (ε, δ) is correct.

---

## P-05-05: Worst-case-optimal join via AGM bound 🔴

**Layer**: Math (Pillars II + I)
**Status**: 🔴 open
**Math**: I (AGM bound), II (spectral — fractional cover LP)
**Effort**: L
**Impact**: high

### Problem

This is **Enhancement 3** from `docs/math_enhancements.md`. The AGM bound
(Atserias-Grohe-Marx 2008) gives the worst-case size of a join result:

$$
|\Join_{i=1}^{m} R_i| \le \prod_{i=1}^{m} |R_i|^{f_i}
$$

where $(f_1, \ldots, f_m)$ is a fractional cover of the query hypergraph.
The leapfrog join (Veldhuizen 2014) achieves this bound.

The planner must:
1. Build the query hypergraph
2. Solve the fractional-cover LP (minimize $\prod |R_i|^{f_i}$)
3. Pick the join order based on the LP solution
4. Execute via leapfrog join

### Open questions

- Can we solve the fractional-cover LP fast enough for online query
  planning? (It's a small LP, but still.)
- How does leapfrog join interact with the tier-aware scan kernels?
- What's the practical overhead on uniform data (where AGM is loose)?

### Success criteria

- A `LeapfrogJoinAvx512` kernel in the kernel table.
- A `FractionalCoverLp` solver in the planner.
- Benchmark: 10–100× over hash join on skewed data, parity on uniform.

---

## P-05-06: Tensor train decomposition for multi-column compression 🔴

**Layer**: Math (Pillar II)
**Status**: 🔴 open
**Math**: II (tensor decomposition — Oseledets 2011)
**Effort**: XL
**Impact**: medium

### Problem

Multi-column data can be modeled as a tensor (one mode per column). The
tensor train (TT) decomposition represents an $d$-dimensional tensor with
$O(d \cdot n \cdot r^2)$ parameters instead of $O(n^d)$:

$$
A[i_1, \ldots, i_d] = G_1[i_1] \cdot G_2[i_2] \cdots G_d[i_d]
$$

where $G_k$ are $r \times r$ matrices (r = TT-rank).

For a table with 10 columns each of cardinality 100, the full tensor has
$100^{10} = 10^{20}$ entries; the TT decomposition with rank 10 has
$10 \cdot 100 \cdot 100 = 10,000$ entries — a $10^{16}$× compression.

### Open questions

- How do we compute the TT decomposition incrementally as data arrives?
- How do we query a TT-compressed table? (Reconstruction is expensive.)
- What's the practical TT-rank for real database columns?

### Success criteria

- A `TensorTrainColumn` type for multi-column compression.
- A scan kernel that queries the TT directly without full reconstruction.
- Benchmark: 100× compression on highly correlated columns.

---

## P-05-07: Universal source coding for schema-on-read 🔴

**Layer**: Math (Pillar I)
**Status**: 🔴 open
**Math**: I (universal coding, Lempel-Ziv)
**Effort**: L
**Impact**: medium

### Problem

Schema-on-read can be formalized as universal source coding: the engine
stores data in a universal 64-bit format, and the "schema" is the
decompression rule chosen at read time.

Lempel-Ziv coding achieves the entropy rate of the source without knowing
the source distribution. Can we apply this to database columns?

### Open questions

- Is there a "universal" column format that adapts to any data distribution?
- How does this relate to the MDL schema selector (P-05-01)?
- Can we prove a redundancy bound for schema-on-read?

### Success criteria

- A formal model of schema-on-read as universal source coding.
- A column format that achieves entropy rate without prior knowledge of the
  distribution.

---

## P-05-08: Online algorithm for tier replacement (k-server) 🔴

**Layer**: Math (Pillar IV)
**Status**: 🔴 open
**Math**: IV (online algorithms — k-server problem)
**Effort**: L
**Impact**: high

### Problem

This is the theoretical foundation for P-02-04 (tier migration policy). The
k-server problem: k servers (cache slots) must serve a sequence of requests
at different points in a metric space (the tiers). The goal is to minimize
total movement.

Known results:
- LRU is k-competitive for paging (Sleator-Tarjan 1985)
- The Work Function Algorithm is (2k-1)-competitive for general k-server
  (Koutsoupias-Papadimitriou 1995)
- No deterministic algorithm can beat k-competitive

### Open questions

- Can we do better than LRU for the multi-tier case (k > 2)?
- How does the Work Function Algorithm perform in practice?
- Is there a randomized algorithm with better competitive ratio?

### Success criteria

- A formal proof that our migration policy is k-competitive.
- Benchmark: the policy achieves ≤ 2× the offline optimal cost.

---

## P-05-09: Functorial schema migration correctness 🟡

**Layer**: Math (Pillar V)
**Status**: 🟡 partial (documented in `docs/research/category_theory_topology_db.md`)
**Math**: V (category theory — Kan extensions)
**Effort**: XL
**Impact**: medium

### Problem

This is **Enhancement 4** from `docs/math_enhancements.md`. Schema evolution
should be a functor application with mathematical guarantees. The three
adjoint functors $\Sigma_F \dashv \Delta_F \dashv \Pi_F$ give three migration
modes.

### Open questions

- Can we prove that a specific migration (e.g., "split a table into two") is
  information-preserving?
- How do we handle migrations that lose information (e.g., "drop a column")?
- Can we use univalence (HoTT) to prove that two schemas are equivalent?

### Success criteria

- A formal proof that Δ_F (copy migration) is information-preserving.
- A `migrate/` module that implements Σ, Δ, Π functors.

---

## P-05-10: Sheaf-theoretic distributed consistency 🔴

**Layer**: Math (Pillar V)
**Status**: 🔴 open
**Math**: V (sheaf theory)
**Effort**: XL
**Impact**: medium

### Problem

Distributed consistency can be formalized via sheaves: each node has a local
section; consistency is the gluing condition (the sections agree on overlaps).

This gives a coordinate-free way to reason about consistency models
(strong, eventual, causal) as sheaf conditions on different topologies.

### Open questions

- Can we formalize CXL coherence as a sheaf on the rack topology?
- Does this give us new consistency models (between strong and eventual)?
- Can we use sheaf cohomology to detect inconsistency?

### Success criteria

- A formal sheaf model of the engine's consistency levels.
- A cohomology-based inconsistency detector.

---

## P-05-11: PAC guarantees for approximate SQL 🔴

**Layer**: Math (Pillar III)
**Status**: 🔴 open
**Math**: III (PAC learning — Valiant 1984)
**Effort**: L
**Impact**: high

### Problem

The `(ε, δ)` approximate SQL surface (see `06-query-syntax-approach.md`)
needs formal PAC guarantees. For a query like:

```sql
SELECT AVG(price) APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99 FROM sales;
```

We need to prove: with probability ≥ 0.99, the returned average is within
0.01 of the true average.

The sample size bound (from Hoeffding):

$$
n \ge \frac{1}{2\varepsilon^2} \ln \frac{1}{\delta} = \frac{1}{2 \cdot 0.0001} \ln \frac{1}{0.01} \approx 23,026
$$

### Open questions

- How do we handle correlated subqueries (where the sample isn't i.i.d.)?
- Can we use Bernstein's inequality for tighter bounds on bounded-variance
  data?
- How do we verify the guarantee empirically?

### Success criteria

- A formal proof that each sketch kernel provides the claimed (ε, δ).
- An empirical validator that checks the guarantee on real data.

---

## P-05-12: Concentration bounds for sketch composition 🔴

**Layer**: Math (Pillar III)
**Status**: 🔴 open
**Math**: III (McDiarmid's inequality, bounded differences)
**Effort**: M
**Impact**: medium

### Problem

When sketches are composed (e.g., a Count-Min sketch inside a HyperLogLog),
the error bounds compose non-trivially. McDiarmid's inequality handles
functions of independent random variables with bounded differences:

$$
P(f(X) - E[f] \ge t) \le \exp\left(-\frac{2t^2}{\sum c_i^2}\right)
$$

### Open questions

- What are the bounded-difference constants for common sketch compositions?
- Can we automate the bound computation via a "sketch algebra"?

### Success criteria

- A `SketchAlgebra` that composes sketches and computes the resulting (ε, δ).
- Formal proofs for the composed bounds.

---

## P-05-13: LP relaxation for join ordering 🟡

**Layer**: Math (Pillar IV)
**Status**: 🟡 partial (Selinger DP is the standard, but LP relaxation is unexplored)
**Math**: IV (LP relaxation, randomized rounding)
**Effort**: L
**Impact**: medium

### Problem

The join ordering problem is NP-hard in general. Selinger DP is O(3^n).
For large n (> 15 joins), we need a relaxation.

LP relaxation + randomized rounding (Raghavan-Thompson 1987):
1. Formulate join ordering as an integer program
2. Relax to LP, solve in polynomial time
3. Round the LP solution to an integer solution
4. Prove the rounded solution is within a factor of the optimal

### Open questions

- What's the integrality gap for the join ordering LP?
- Can we use semidefinite programming (Goemans-Williamson) for better
  approximation?

### Success criteria

- An LP-based join ordering solver for n > 15.
- Benchmark: within 2× of the DP-optimal plan.

---

## P-05-14: Submodular index selection 🔴

**Layer**: Math (Pillar IV)
**Status**: 🔴 open
**Math**: IV (submodular maximization — Nemhauser-Wolsey-Fisher)
**Effort**: M
**Impact**: medium

### Problem

Index selection: given a workload and a storage budget, which indexes to
build? The objective (query speedup) is submodular (diminishing returns),
so the greedy algorithm gives a (1 - 1/e) ≈ 0.632 approximation.

### Open questions

- How do we handle the matroid constraint (an index can only be on one
  column combination)?
- Can we use continuous submodular optimization for finer-grained index
  choice?

### Success criteria

- An `IndexSelector` that uses greedy submodular maximization.
- Benchmark: within 63% of the optimal index set.

---

## P-05-15: Linear-logic type system for memory safety 🔴

**Layer**: Math (Pillar V)
**Status**: 🔴 open
**Math**: V (linear logic — Girard 1987)
**Effort**: L
**Impact**: high

### Problem

This is the theoretical foundation for P-04-01 (protocol boundary type
safety). We need a linear-logic type system that enforces:
- CXL references are used exactly once (linear)
- Raft references are used at most once (affine)
- Local references are unrestricted

### Open questions

- Can we encode this in Rust's type system (via `PhantomData` and `Drop`),
  or do we need an external type checker?
- How do we handle borrowing (a CXL reference can be read by multiple
  threads, but only one can write)?

### Success criteria

- A formal type system specification.
- A prototype implementation in Rust.

---

## Summary

| # | Problem | Status | Pillar | Effort | Impact |
|---|---------|--------|--------|--------|--------|
| 01 | MDL schema selection — closed-form or LP? | 🟡 | I | M | high |
| 02 | Spectral partitioning for NUMA placement | 🔴 | II | L | high |
| 03 | Closed-form cost model (Kingman + AVX-512) | 🔴 | III+I | XL | critical |
| 04 | (ε, δ) propagation through the DAG | 🔴 | III | L | critical |
| 05 | Worst-case-optimal join via AGM bound | 🔴 | II+I | L | high |
| 06 | Tensor train decomposition for multi-column | 🔴 | II | XL | medium |
| 07 | Universal source coding for schema-on-read | 🔴 | I | L | medium |
| 08 | Online algorithm for tier replacement (k-server) | 🔴 | IV | L | high |
| 09 | Functorial schema migration correctness | 🟡 | V | XL | medium |
| 10 | Sheaf-theoretic distributed consistency | 🔴 | V | XL | medium |
| 11 | PAC guarantees for approximate SQL | 🔴 | III | L | high |
| 12 | Concentration bounds for sketch composition | 🔴 | III | M | medium |
| 13 | LP relaxation for join ordering | 🟡 | IV | L | medium |
| 14 | Submodular index selection | 🔴 | IV | M | medium |
| 15 | Linear-logic type system for memory safety | 🔴 | V | L | high |
