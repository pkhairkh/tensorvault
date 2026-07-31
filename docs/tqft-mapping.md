# TQFT-Inspired Database Optimization — Concept Mapping

## Overview

This document maps concepts from **Topological Quantum Field Theory (TQFT)** to
database query execution patterns. The goal is to use TQFT's mathematical
framework — which has been highly successful in physics for describing
"topology-preserving" computations — as a lens for designing database operators
that are **invariant to data layout** and **compose via gluing**.

## TQFT Primer (Atiyah-Segal Axioms)

A TQFT is a symmetric monoidal functor:

```
Z : nCob → Vect_k
```

It assigns:
- To each closed (d-1)-manifold Σ (a "boundary"): a vector space Z(Σ) (the "state space")
- To each d-manifold M with ∂M = Σ₀* ∪ Σ₁ (a "cobordism" from Σ₀ to Σ₁): a linear map Z(M): Z(Σ₀) → Z(Σ₁)

Subject to axioms:
1. **Functoriality**: Z(M₁ ∘ M₂) = Z(M₁) ∘ Z(M₂) — gluing cobordisms composes their maps
2. **Monoidality**: Z(Σ₀ ⊔ Σ₁) = Z(Σ₀) ⊗ Z(Σ₁) — disjoint union maps to tensor product
3. **Involutivity**: Z(Σ*) = Z(Σ)* — orientation reversal gives the dual space
4. **Multiplicativity**: Z(∅) = k — empty boundary maps to the ground field
5. **Hermitian**: Z(M*) = Z(M)† — reversed cobordism is the adjoint

In 2D, a TQFT is equivalent to a **commutative Frobenius algebra** (A, μ, η, Δ, ε) where:
- μ: A⊗A → A (multiplication / "pair-of-pants" cobordism)
- η: k → A (unit / "cap" cobordism)
- Δ: A → A⊗A (comultiplication / "upside-down pair-of-pants")
- ε: A → k (counit / "trace" / "cup")

## Concept Mapping: TQFT → Database

| TQFT Concept | Database Analog | turboGP Implementation |
|---|---|---|
| **Cobordism M: Σ₀→Σ₁** | Query operator (input schema → output schema) | `FilterStage`, `HashJoinProbeStage`, `AggregateStage` |
| **Boundary Σ** (state space) | Morsel = batch of rows at operator boundary | `Morsel { columns, mask, row_count }` (8K rows) |
| **Gluing M₁∘M₂** (functoriality) | Pipeline composition — no intermediate materialization | Push-based pipeline: `source → filter → probe → aggregate` |
| **Frobenius μ: A⊗A→A** (multiplication) | **Hash Join** — merge two relations on key | `hash_join_with_keys` (build+probe = pair-of-pants) |
| **Frobenius Δ: A→A⊗A** (comultiplication) | **Join fan-out / OR-split** — one row → multiple matches | Q19: split OR branches into 3 sub-joins, union results |
| **Counit ε: A→k** (trace) | **Scalar aggregate** — reduce column to single value | `SUM`, `COUNT`, `AVG` via VNNI/BF16 kernels |
| **Unit η: k→A** (broadcast) | **Literal broadcast** — scalar → column | `_mm512_set1_epi64` / `_mm512_set1_pd` |
| **Wilson loop** (holonomy) | **Bloom filter semi-join** — cyclic key membership test | Build bloom on join key, probe-skip before hash table |
| **Topological invariance** | **Data-layout-independent scan** — skip irrelevant data | Zone maps (per-page min/max) with AVX-512 range check |
| **Monoidal ⊗** (tensor) | **Columnar SoA** — parallel column arrays | `Vec<Arc<Vec<u64>>>` (already implemented) |
| **Partition function Z(M)** | **Aggregate over full scan** — invariant of the data | Fused per-group aggregation (W14) |
| **Locality axiom** | **NUMA-aware morsel scheduling** — data-local computation | Pin morsels to data-local NUMA nodes |
| **Frobenius condition** μ∘Δ = id | **Join-project pushdown** — don't materialize unused columns | Project before join, not after |

## Wave Applications

### W29: Frobenius Join with Wilson-Loop Bloom Filter
**TQFT basis**: The multiplication μ (pair-of-pants) merges two state spaces.
The Wilson loop is a cyclic observable that tests membership without full traversal.

**Database application**: Before probing the hash table, build a **bloom filter**
on the build-side join keys. During probe scan, check the bloom filter first —
if the key is definitely not in the build set, skip the hash table probe entirely.
This is the "Wilson loop" — a topological pre-check that avoids the full holonomy
(hash table lookup).

**Target**: Q5 (5 joins × 6M probe rows), Q3 (2 joins)

### W30: Cobordism Pipeline (Gluing Axiom)
**TQFT basis**: Functoriality Z(M₁∘M₂) = Z(M₁)∘Z(M₂) means we can compose
cobordisms by gluing their shared boundary. The state space at the gluing
boundary is the morsel.

**Database application**: Replace the materialize-then-filter model with a
push-based pipeline where each operator processes an 8K-row morsel and pushes
it to the next operator. No intermediate tables are materialized. The morsel
is the "state space" that flows through the "cobordism" (pipeline).

**Target**: Q3 (2 joins + GROUP BY), Q18 (join + GROUP BY)

### W31: VNNI/BF16 Counit (Deep Integration)
**TQFT basis**: The counit ε: A→k is the "trace" or "dimension" map that
reduces a state space to a scalar. In the Frobenius algebra, this is the
aggregate operator.

**Database application**: Wire the VNNI (`_mm512_dpbusd_epi32`) and BF16
(`_mm512_dpbf16_ps`) kernels directly into the fused per-group aggregation
path (`try_fused_grouped_agg`). The "counit" is the SUM/COUNT/AVG reduction.

**Target**: Q1 (10 aggregates), Q6 (sum), Q14 (sum + case)

### W32: Correctness via Frobenius Algebra
**TQFT basis**: The Frobenius condition requires μ and Δ to be compatible
algebra homomorphisms. Q19's OR-conditions violate this — they try to do
μ (join) and Δ (split) simultaneously.

- **Q15 float bug**: The counit ε (aggregate) must be consistent across
  recomputation. Use epsilon comparison (topological invariance — the exact
  bit pattern shouldn't matter, only the value).
- **Q4 EXISTS**: Semi-join = μ followed by ε (join then test existence).
- **Q19 OR-split**: Use Δ (comultiplication) to split the OR into 3 independent
  cobordisms, then μ (union) the results.

### W33: Topological Invariance via Zone Maps
**TQFT basis**: A topological invariant is metric-independent — it doesn't
care about the specific geometry, only the topology.

**Database application**: **Zone maps** store per-page min/max values. During
scan, if the query's filter range doesn't overlap the page's [min,max], skip
the entire page. The scan is "topologically invariant" — it skips data that
can't possibly match, regardless of row-level values.

**Target**: Q6 (date range), Q1 (date range), Q5 (date range on orders)

## Mathematical Justification

The TQFT framework provides three key insights for database optimization:

1. **Gluing = no materialization**: The functoriality axiom Z(M₁∘M₂) = Z(M₁)∘Z(M₂)
   means we can compute the composite map directly without ever materializing
   the intermediate state space Z(Σ₁). This is the theoretical basis for
   push-based morsel pipelines.

2. **Frobenius = join/aggregate duality**: In a Frobenius algebra, the
   multiplication μ (join) and counit ε (aggregate) are part of the same
   algebraic structure. This means joins and aggregates can be fused —
   the "join-aggregate" cobordism M∘ε can be computed as a single pass
   without materializing the join output.

3. **Wilson loop = bloom filter**: The Wilson loop observable tests whether
   a path is contractible (trivial holonomy) without computing the full
   holonomy. Analogously, a bloom filter tests whether a key is definitely
   absent without probing the full hash table. Both are "topological
   pre-checks" that exploit structure to avoid expensive computation.

## References
- Atiyah, M. (1988). "Topological quantum field theories". Publications Mathématiques de l'IHÉS.
- Kock, J. (2003). "Frobenius Algebras and 2D Topological Quantum Field Theories". Cambridge.
- Segal, G. (2004). "The definition of conformal field theory".
