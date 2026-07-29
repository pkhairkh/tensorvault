# ADR-022: RAPL + external meter for energy benchmarking

## Status
Accepted

## Confidence
85%

## Context

Energy efficiency is a key differentiator (the TPC-C consolidation story claims 11× better tpmC/W vs PolarDB). To make this claim defensible, we need accurate energy measurement.

Options:
- **Intel RAPL** (Running Average Power Limit): on-die energy counters, accurate for ≥10 ms windows (Hahnel 2012). Available on all Intel CPUs since Sandy Bridge.
- **AMD RAPL**: modeled, not measured — less accurate (Schöne 2021)
- **External meter (Hioki, Watts Up?)**: ground truth but coarse (1–10 Hz sampling) and can't attribute per-query
- **Analytical model**: per-instruction energy from `cpu_energy_kb.md`, predictive but ±20–40% error

## Decision

**Use a three-tier energy measurement approach:**
1. **Daily benchmarks**: Intel RAPL (via `perf stat -e power/energy-pkg/`) — accurate for ≥10 ms queries on Intel
2. **Cross-vendor (AMD, ARM)**: analytical model from `cpu_energy_kb.md`, calibrated against RAPL on Intel
3. **Calibration anchor**: external Hioki meter on one test machine, used to validate RAPL and the model quarterly

Report: joules per query, queries per joule, tpmC per watt.

## Consequences

### Positive
- **Intel RAPL is free and standard** — no extra hardware for daily benchmarks
- **±5% accuracy** on Intel for ≥10 ms windows (Hahnel 2012)
- **External meter** provides ground-truth calibration
- **Analytical model** fills the gap on AMD/ARM where RAPL is inaccurate

### Negative
- **AMD RAPL is a model, not a measurement** (Schöne 2021) — energy claims on AMD are less defensible
- **Sub-10 ms queries** can't be attributed accurately (RAPL counter refresh lag)
- **External meter** is coarse (1–10 Hz) — can't measure individual queries, only batches

## Alternatives considered

1. **External meter only** — ground truth but can't attribute per-query. Rejected as primary.
2. **Analytical model only** — portable but ±20–40% error. Rejected as primary.
3. **Intel RAPL only** — accurate on Intel but not on AMD/ARM. Rejected as sole method.

## Compatibility

- Compatible with ADR-021 (TPC-H): TPC-H queries are the energy measurement workload
- Compatible with ADR-003 (CPUID dispatch): the analytical model uses the kernel table's energy estimates
- Compatible with ADR-018 (morsel executor): RAPL measures the whole socket during morsel execution

## References
- Hahnel et al., "Measuring Energy Consumption for Short Code Paths" GreenMetrics 2012
- Schöne et al., "Energy Efficiency Aspects of the AMD Zen 2 Architecture" Cluster 2021
- Tiwari et al., "Power Analysis of Embedded Software" DAC 1994
- `docs/architecture/cpu-energy-kb.md` (the analytical model)
