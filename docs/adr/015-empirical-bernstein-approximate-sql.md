# ADR-015: Empirical Bernstein + sequential stopping for (ε,δ) approximate SQL

## Status
Accepted

## Confidence
85%

## Context

The `APPROXIMATE WITHIN ε CONFIDENCE 1-δ` SQL extension needs a statistical method that:
1. Provides formal (ε, δ) guarantees
2. Minimizes sample size (to save energy and time)
3. Works without knowing the data distribution in advance

Options:
- **Hoeffding**: $n \ge \frac{1}{2ε^2} \ln \frac{2}{δ}$ — distribution-free but loose
- **Bernstein**: tighter if variance is known, but variance is usually unknown
- **Empirical Bernstein**: estimates variance from the sample, gives tighter bounds
- **Sequential stopping (Wald SPRT)**: stop sampling when confidence is high enough

## Decision

**Use empirical Bernstein inequality with sequential stopping for approximate aggregates.**

Algorithm:
1. Sample in batches of 1024 cells
2. After each batch, compute the sample mean and variance
3. Apply the empirical Bernstein bound: $\bar{X} \pm \sqrt{\frac{2 \hat{\sigma}^2 \ln(2/δ)}{n}} + \frac{7 \ln(2/δ)}{3(n-1)}$
4. If the confidence interval is within ε, stop and return the estimate
5. Otherwise, sample another batch

For `COUNT DISTINCT`: use HyperLogLog (RSE = 1.04/√m, where m = 2^precision registers).
For `SUM`/`AVG`: use empirical Bernstein with sequential stopping.
For `PERCENTILE`: use t-Digest.

## Consequences

### Positive
- **Tighter bounds than Hoeffding**: empirical Bernstein uses the actual variance, which is much smaller than the worst case for low-variance data
- **50–80% sample savings** vs fixed-sample Hoeffding (Maurer-Pontil 2009)
- **Formal guarantee**: P(|estimate - true| ≤ ε) ≥ 1-δ, provably
- **Adaptive**: stops early on low-variance data, samples more on high-variance data

### Negative
- Slightly more complex than fixed-sample Hoeffding (need to track running variance)
- The sequential stopping rule introduces a slight bias (mitigated by the correction term in the bound)
- Doesn't work for non-i.i.d. data (correlated rows) — need McDiarmid for those (future ADR)

## Alternatives considered

1. **Hoeffding only** — simpler but 2–3× more samples needed. Rejected as primary; kept as fallback for non-i.i.d. data.
2. **Bayesian posterior** — tightest but requires a prior (subjective). Deferred to research.
3. **Exact computation** — no error but 10–100× slower. The whole point of `APPROXIMATE` is to avoid this.

## Compatibility

- Compatible with ADR-007 (1024-cell batch): sequential stopping checks after each batch
- Compatible with ADR-017 (VPOPCNTDQ): the count kernel produces exact counts for HLL
- Compatible with ADR-018 (morsel executor): each morsel contributes to the running estimate

## References
- Maurer & Pontil, "Empirical Bernstein Bounds and Sample Variance Penalization" COLT 2009
- Hoeffding, "Probability Inequalities for Sums of Bounded Random Variables" JASA 1963
- Flajolet et al., "HyperLogLog" AOFA 2007
- Dunning, "t-Digest" 2019
