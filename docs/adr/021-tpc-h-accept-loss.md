# ADR-021: TPC-H — run as-is, accept 1.2–1.5× loss

## Status
Accepted

## Confidence
95%

## Context

TPC-H is the standard OLAP benchmark. Our architecture (64-bit-everywhere, ADR-001) is structurally worse than DuckDB's type-stable columns for this workload:
- DuckDB packs `DECIMAL(15,2)` into 8 bytes; we use 8 bytes for everything (including `CHAR(1)`)
- DuckDB's columns are type-stable → no per-row dispatch; ours are too (but we pay the 8-byte-per-cell tax on narrow types)
- DuckDB has 20 years of optimizer tuning; we're starting from scratch

No amount of kernel tuning closes this gap. The 1.2–1.5× loss is structural.

## Decision

**Run TPC-H as-is, accept the 1.2–1.5× loss, and be honest about it.**

- Implement all 22 TPC-H queries
- Benchmark at SF=1, SF=10, SF=100
- Publish honest results comparing to DuckDB
- Document WHERE we lose (narrow columns, complex joins) and WHERE we're close (Q1 aggregation, Q6 range filter)
- Optimize the top 3–4 queries to narrow the gap, but don't chase parity

## Consequences

### Positive
- **Credibility**: honest benchmarks are more credible than cherry-picked ones
- **Feeds the energy benchmark** (ADR-022): TPC-H queries give us joules/query data
- **Identifies weak spots**: the loss pattern tells us where to improve
- **Sets expectations**: no one can claim we're hiding a bad result

### Negative
- **Marketing-negative**: "we lose to DuckDB" is not a great headline
- May discourage adoption by OLAP-focused users (but they're not our target market anyway)

## Alternatives considered

1. **Optimize TPC-H aggressively** — could close the gap to 1.1× but takes 6+ months of engineering for a benchmark we'll never win. Rejected.
2. **Skip TPC-H entirely** — controls the narrative but loses credibility. Rejected.
3. **Create a "TPC-H-modified" benchmark** — change the rules to favor us. Dishonest. Rejected.

## Compatibility

- Compatible with ADR-022 (RAPL energy benchmarking): TPC-H queries are the energy measurement workload
- Compatible with the overall strategy: we win on schema-fluid analytics (5–10×) and TPC-C consolidation (11× energy), not on TPC-H

## References
- TPC-H specification, v3.0.1
- DuckDB benchmark reports (duckdb.org)
- Leis et al., "How Good Are Query Optimizers, Really?" VLDB 2015
