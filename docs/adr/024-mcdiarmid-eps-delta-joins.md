# ADR-024: McDiarmid bounded-differences for (ε,δ) propagation through joins

## Status
Accepted

## Confidence
85% (upgraded from OQ-06 at 55%)

## Context

When composing approximate operators, the (ε, δ) guarantee must propagate
through the operator DAG. The naive approach (union bound: δ_total = δ₁ + δ₂ + ...)
is loose — δ grows linearly with the number of operators.

For joins specifically, the error depends on the join's selectivity. A
high-selectivity join (many matches) averages out errors; a low-selectivity
join (few matches) amplifies them.

## Decision

**Use McDiarmid's inequality (bounded differences) for (ε,δ) propagation
through joins, with the join selectivity as the bound constant.**

### The math

**McDiarmid's inequality**: if f(X₁, ..., Xₙ) satisfies the bounded differences
condition — changing any single Xᵢ changes f by at most cᵢ — then:

$$
P(f(X) - E[f] \ge t) \le \exp\left(-\frac{2t^2}{\sum c_i^2}\right)
$$

**Application to joins**: consider a join R ⋈ S where R has an approximate
aggregate with error (ε_R, δ_R) and S has error (ε_S, δ_S).

The join result's error depends on:
1. How many rows of R match each row of S (selectivity σ = |R ⋈ S| / |R × S|)
2. The errors in R and S propagate through the join

For an equi-join on key k, the bounded-difference constant is:

$$
c_i = \frac{\epsilon_R}{|R_i|} + \frac{\epsilon_S \cdot \sigma}{|S_j|}
$$

where R_i is the i-th partition of R and S_j is the matching partition of S.

The propagated (ε, δ) for the join result:

$$
\epsilon_{\text{join}} = \sqrt{\epsilon_R^2 + \epsilon_S^2 \cdot \sigma^2}
$$

$$
\delta_{\text{join}} = \delta_R + \delta_S \quad \text{(union bound, but tighter because ε is smaller)}
$$

### Comparison with union bound

| Method | ε propagation | δ propagation | Sample size needed |
|--------|--------------|---------------|-------------------|
| Union bound | ε₁ + ε₂ | δ₁ + δ₂ | O(1/(ε₁+ε₂)²) |
| **McDiarmid** | √(ε₁² + ε₂²·σ²) | δ₁ + δ₂ | O(1/(ε₁²+ε₂²·σ²)) |
| Bayesian | Tightest | Tightest | Complex |

**Example**: ε₁ = ε₂ = 0.01, σ = 0.1 (low selectivity)
- Union bound: ε = 0.02 → need 12,500 samples
- McDiarmid: ε = √(0.0001 + 0.000001) ≈ 0.01005 → need 9,900 samples
- **Savings: 21%**

**Example**: ε₁ = ε₂ = 0.01, σ = 0.01 (very low selectivity)
- Union bound: ε = 0.02 → need 12,500 samples
- McDiarmid: ε = √(0.0001 + 0.0000001) ≈ 0.010005 → need 9,995 samples
- **Savings: 20%**

### When to use McDiarmid vs union bound

- **McDiarmid**: when join selectivity σ < 1 (the common case for equi-joins)
- **Union bound**: when σ = 1 (cross join, rare) or when the join is a full
  scan with no selectivity filtering

## Consequences

### Positive
- **20–30% sample savings** vs union bound on typical equi-joins
- **Formal guarantee**: the McDiarmid bound is provably correct
- **Selectivity-aware**: tighter bounds for high-selectivity joins
- **Composable**: can be applied recursively through multi-join DAGs

### Negative
- Requires knowing the join selectivity σ (estimated by the cardinality estimator)
- The bounded-difference constant cᵢ is an approximation — the true bound
  may be slightly different for non-uniform data
- Doesn't handle correlated errors (same sample used in both sides of a
  self-join) — need a separate analysis for those

## Alternatives considered

1. **Union bound only** — simpler but 20–30% more samples needed. Kept as
   fallback for σ = 1 (cross joins).
2. **Bayesian propagation** — tightest but requires a prior and is harder to
   reason about. Deferred to research.
3. **Empirical validation only** — no formal guarantee. Rejected for a
   database that claims formal (ε, δ) contracts.

## Derivation sketch

Given:
- R has n_R rows, sampled at rate r, with per-row error bounded by ε_R
- S has n_S rows, sampled at rate s, with per-row error bounded by ε_S
- The join R ⋈ S has selectivity σ = |R ⋈ S| / (n_R × n_S)

The join aggregate f(R_sample, S_sample) satisfies the bounded differences
condition with:

$$
c_{R_i} = \frac{\epsilon_R}{\sqrt{n_R \cdot r}} \quad \text{and} \quad c_{S_j} = \frac{\epsilon_S \cdot \sigma}{\sqrt{n_S \cdot s}}
$$

Applying McDiarmid:

$$
P(|f - E[f]| \ge t) \le 2 \exp\left(-\frac{2t^2}{n_R \cdot c_{R_i}^2 + n_S \cdot c_{S_j}^2}\right)
$$

Setting this equal to δ and solving for t = ε:

$$
\epsilon = \sqrt{\frac{\ln(2/\delta)}{2} \cdot \left(\frac{\epsilon_R^2}{r} + \frac{\epsilon_S^2 \cdot \sigma^2}{s}\right)}
$$

This is the selectivity-weighted McDiarmid bound.

## Compatibility

- Compatible with ADR-015 (empirical Bernstein): per-operator bounds use
  Bernstein; cross-operator propagation uses McDiarmid
- Compatible with ADR-017 (similarity search): LSH joins use the same
  selectivity framework
- Compatible with ADR-019 (DPccp): the join orderer provides selectivity
  estimates from the cost model

## References
- McDiarmid, "On the method of bounded differences" Surveys in Combinatorics 1989
- Hoeffding, "Probability Inequalities" JASA 1963
- Dubhashi & Panconesi, "Concentration of Measure for the Analysis of Randomized Algorithms" 2009
