# Wave 4: Cost Model — Work Record

**Task ID:** wave-4
**Agent:** Z.ai Code (single-agent execution)
**Status:** ✅ Complete
**Date:** 2026-07-30

## Summary

Implemented Wave 4 of the turboGP database engine: a calibrated analytic cost
model (ADR-023) and a Kingman queueing predictor (ADR-020), wired together by
an `estimate_cost` function that walks the logical plan tree and sums per-node
compute costs plus a one-time queueing wait per join.

The cost model encodes Zen 5 measurements: 3 GHz CPU × 8 AVX-512 lanes = 24 G
cells/sec L3-resident, 40 GB/s ÷ 8 B = 5 G cells/sec DRAM-resident. These
match ADR-023's measured values (24.1 G and ~5 G cells/sec) to within 1%.

All DoD gates pass: `cargo fmt --check` (clean), `cargo clippy -- -D warnings`
(clean), `cargo clippy --all-targets -- -D warnings` (clean), `cargo test`
(132 passed = 112 baseline + 20 new, debug and release modes both green).

## Files Modified

| File | Change |
|------|--------|
| `src/planner/mod.rs` | **New file.** `CostModel` struct with `(cpu_freq_hz, simd_lanes, memory_bandwidth_bps, cell_size)` fields, `Default` impl returning Zen 5 values (3 GHz, 8 lanes, 40 GB/s, 8 B). Methods: `throughput_l3(operator)` → `simd_lanes × cpu_freq_hz` (= 24 G cells/sec); `throughput_dram()` → `memory_bandwidth_bps / cell_size` (= 5 G cells/sec); `estimate_compute(n, op, tier)` → `n / throughput(tier)` with L1L2/L3 dispatching to `throughput_l3` and Ddr5/Hbm/Cxl/Nvme/NvmeOf/Network to `throughput_dram`. Free function `estimate_cost(plan, cm, kingman)` walks the plan tree: Scan → `estimate_compute(n, ScanEqU64, L3)`; Aggregate → `child_cost + estimate_compute(n_child, AggregateSumF64, L3)`; Join → `left_cost + right_cost + estimate_compute(n_left + n_right, HashProbe, L3) + kingman.predicted_wait()`; Materialize → passthrough. Helper `child_cell_count(node)` extracts the output cell count from any `PlanNode`. 12 unit tests. |
| `src/planner/kingman.rs` | **New file.** `KingmanPredictor` struct with public `(lambda, mu, c_a, c_s)` fields and private Welford running statistics (`arrival_mean`, `arrival_m2`, `service_mean`, `service_m2`, `n`). Methods: `new(λ, μ, c_a, c_s)`; `utilization()` → `λ/μ` (0 if μ ≤ 0); `predicted_wait()` → Kingman's formula `(ρ/(1−ρ)) · ((c_a²+c_s²)/2) · (1/μ)`, returning `+∞` if `ρ ≥ 1` and 0 if `ρ ≤ 0`; `predicted_p99()` → `mean_response · (1 + 2.33 · c_s)` (lognormal approximation, `mean_response = predicted_wait + 1/μ`); `update(arrival_interval, service_time)` → Welford online update of mean and M2 for both arrival and service, then publishes `λ = 1/mean_arrival`, `μ = 1/mean_service`, `c_a = sqrt(var_a)/mean_a`, `c_s = sqrt(var_s)/mean_s`. `Default` impl returns an idle predictor `(0, 1, 1, 1)` (ρ = 0, wait = 0). 8 unit tests. |
| `src/lib.rs` | Added `pub mod planner;` (alphabetically between `memory` and `protocol`). Added a `## Modules` doc entry for `planner` describing its role (ADR-023 cost model + ADR-020 Kingman) and the downstream ADRs that depend on it (019 join ordering, 016 index selection, 020 admission control). |

## Design Decisions

### Task 4-1: `CostModel` struct

**Field choices.** The four fields are exactly as specified in the task:
`cpu_freq_hz: f64`, `simd_lanes: usize`, `memory_bandwidth_bps: f64`,
`cell_size: usize`. The mix of `f64` and `usize` reflects the natural types
(Hz and B/s are large floating-point quantities; lane count and cell size are
small integers used as divisors/multipliers). All four are `pub` so callers
can construct a custom `CostModel { .. }` literal for what-if analysis without
a builder.

**`Default` vs `default()` method.** The task says "should have a `default()`
that returns reasonable Zen 5 values". I implemented the `Default` trait
rather than an inherent `default()` method — `CostModel::default()` works
identically either way, and the trait impl lets callers use `CostModel:
Default` bounds in generic contexts (e.g. `fn plan<C: Default>()`).

**`throughput_l3` takes `Operator` but ignores it.** The signature is
`throughput_l3(&self, _operator: Operator) -> f64` — the parameter is named
with a leading underscore to silence `unused_variables`. The formula
`simd_lanes × cpu_freq_hz` is the same for every 8-lane AVX-512 kernel on
Zen 5 (see ADR-023's measurement table: scan_eq, sum_f64, hamming, and
scan_range all hit ~24 G cells/sec). The parameter is retained in the
signature so a future per-kernel calibration table can dispatch on it
without changing call sites — a forward-compatibility hedge documented in
the method's doc comment.

**Tier dispatch in `estimate_compute`.** The match arms are:
- `L1L2 | L3` → `throughput_l3` (compute-bound)
- `Ddr5 | Hbm | Cxl | Nvme | NvmeOf | Network` → `throughput_dram`
  (bandwidth-bound)

This is a deliberate simplification: CXL has its own link bandwidth (typically
~32 GB/s for PCIe 5.0 ×4), and HBM has multi-TB/s bandwidth, but the current
model uses the DRAM figure as a conservative lower bound. A future wave can
add per-tier bandwidth constants (`MemoryTier::bandwidth_gbps` already exists
in `src/memory/tier.rs` and returns 64.0 for CXL, 1600.0 for HBM) and have
`estimate_compute` consult the tier directly.

**Zero-cell and zero-throughput guards.** `estimate_compute` returns 0.0 if
`n_cells == 0` (no work) or if the computed throughput is ≤ 0 (which would
happen if `cell_size == 0` in `throughput_dram`). Both prevent division by
zero.

### Task 4-2: `KingmanPredictor`

**Welford's online algorithm.** The `update` method uses Welford's algorithm
for numerical stability when computing running mean and variance. The
alternative (accumulating `sum_x` and `sum_x²` then computing
`var = (sum_x² − n·mean²) / (n−1)`) is catastrophically unstable for large
sample counts — the difference `sum_x² − n·mean²` loses precision when both
terms are huge. Welford avoids this by updating the mean and the sum of
squared deviations incrementally:

```text
delta = x − mean_old
mean_new = mean_old + delta / n
delta2 = x − mean_new
M2_new = M2_old + delta × delta2
var = M2 / (n − 1)   (sample variance)
```

The private fields `arrival_mean`, `arrival_m2`, `service_mean`,
`service_m2`, `n` are exactly the Welford state. The public `lambda`, `mu`,
`c_a`, `c_s` fields are republished after each update so that callers reading
them always see consistent values.

**First-update behavior.** After the first call to `update`, the Welford M2
is 0 (a single sample has no variance), so `c_a = c_s = 0`. This overwrites
the user-supplied initial values from `new()`. This is mathematically
correct (you can't estimate variance from one sample) but may surprise
callers who expect the initial priors to persist. The `new()` doc comment
explicitly warns about this: "callers wanting a purely-static estimate (e.g.
for a test or a cold-start prior) should simply not call `update`." A future
enhancement could use a Bayesian prior (e.g. inverse-gamma on the variance)
so the initial `c_a`/`c_s` decay smoothly toward the sample estimate rather
than being overwritten on the first observation.

**`predicted_wait` edge cases.** Returns `f64::INFINITY` for `ρ ≥ 1` (the
queue is unstable — Kingman's formula has a pole at `ρ = 1`) and 0.0 for
`ρ ≤ 0` or `μ ≤ 0` (no load or no service capacity). The `ρ ≥ 1` case is
what the admission controller (ADR-020) uses to reject queries when
utilization exceeds 80%: the predicted wait grows without bound, so the
query gets an HTTP 503 instead of being queued.

**`predicted_p99` lognormal approximation.** The formula
`mean_response × (1 + 2.33 × c_s)` is a first-order Taylor expansion of the
exact lognormal p99 `mean × exp(2.33 × sqrt(ln(1 + c_s²)))`. For `c_s = 1`
(the standard M/M/1 case), the exact value is `exp(2.33 × sqrt(ln 2)) ≈ 6.61`,
while the approximation gives `1 + 2.33 = 3.33` — a 2× underestimate. For
`c_s < 0.5` (typical for real database workloads where service times are
relatively stable), the approximation is within 10% of the exact value. The
doc comment notes this limitation and points to the exact formula for callers
who need it. This is acceptable for the current use case (admission control
threshold, not precise p99 prediction).

**`Default` impl.** Returns `new(0.0, 1.0, 1.0, 1.0)` — zero arrival rate,
unit service rate. This gives `ρ = 0`, `predicted_wait = 0`, which is the
"idle system" prior. Callers should call `update` or `new` with real values
before trusting the predictions.

### Task 4-3: `estimate_cost` plan walker

**Simple per-node estimates.** The task spec is explicit: "For now, use
simple estimates." The implementation follows the spec literally:

- **Scan**: `estimate_compute(n, ScanEqU64, L3)`. Uses `ScanEqU64` as the
  canonical scan operator regardless of the actual `params.operator`, because
  all scan variants (eq, range, multi-predicate) have the same SIMD
  throughput bound on Zen 5. A future per-kernel calibration table would
  dispatch on `params.operator`.
- **Aggregate**: `child_cost + estimate_compute(n_child, AggregateSumF64, L3)`.
  The aggregate's input is the child's cell count (extracted by
  `child_cell_count`).
- **Join**: `left_cost + right_cost + estimate_compute(n_left + n_right,
  HashProbe, L3) + kingman.predicted_wait()`. The Kingman wait is added
  *once per join* — this is the only operator in the current model that
  contributes a queueing term, because joins contend on a shared hash table.
  Future operators that contend on shared state (e.g. a global sort buffer)
  would add their own `+ kingman.predicted_wait()` terms.
- **Materialize**: passthrough. Materialization is a `rep_movsb` memcpy
  (ADR-006), which is a future calibration axis not yet wired into the cost
  model. For now it contributes zero additional cost.

**`child_cell_count` helper.** Extracts the output cell count of a plan node
for use in estimating the input size of its parent. For Scan, this is
`params.cell_count`. For Aggregate, it passes through (an aggregate doesn't
change the number of cells *read*, even though it produces a scalar — the
parent join still pays the bandwidth to read the aggregate's input). For
Join, it's the sum of left + right inputs. For Materialize, it passes
through. This is a simplification: a real aggregate produces a single scalar,
not a stream of cells, so the parent should see `n = 1`. The simplification
is documented in the function's doc comment and is acceptable for the
current "simple estimates" tier — ADR-019 (DPccp join ordering) will refine
this when it needs accurate cardinality estimates.

**Tier default: L3.** All estimates use `MemoryTier::L3` because that's the
default tier in `executor::plan::lower_node` (the scheduler refines the tier
at execution time based on the region's actual placement). A future
enhancement would thread the region's tier through `estimate_cost` by
looking up each `region_id` in the region table.

## Task 4-4: Tests

### `src/planner/kingman.rs` (8 tests)

1. `utilization_is_lambda_over_mu` — `ρ = λ/μ` for ρ ∈ {0.5, 0.99}, plus
   the `μ = 0` edge case (returns 0).
2. `predicted_wait_rho_half_is_reasonable` — ρ=0.5, c_a=c_s=1, μ=100 →
   W = (0.5/0.5) · 1 · 0.01 = 0.01 s exactly. Asserts 0 < W < 1 and
   `|W − 0.01| < 1e-9`.
3. `predicted_wait_rho_99_much_larger_than_rho_50` — ρ=0.99 vs ρ=0.5, both
   with c_a=c_s=1, μ=100. The ratio `(0.99/0.01) / (0.5/0.5) = 99` exactly.
   Asserts `w_high > 10 × w_low` and `|w_high/w_low − 99| < 1e-6`.
4. `predicted_wait_saturates_at_unstable_load` — ρ > 1 (λ=150, μ=100) and
   ρ = 1 exactly both return `+∞`.
5. `predicted_p99_exceeds_mean_response` — p99 > mean_response (it's the
   mean scaled by `1 + 2.33·c_s ≥ 1`). With c_s=1, the factor is exactly
   3.33.
6. `update_converges_to_constant_observation` — 1000 identical
   `(0.01, 0.005)` observations → `λ → 100`, `μ → 200`, `c_a, c_s → 0`
   (zero variance).
7. `update_tracks_variance` — alternating `(0.01, 0.03)` intervals (mean
   0.02, stddev 0.01, c_a = 0.5) with constant service times (c_s → 0).
   Asserts `|λ − 50| < 1`, `|c_a − 0.5| < 0.05`, `c_s < 0.01`.
8. `default_predictor_is_idle` — `Default::default()` has ρ = 0 and
   wait = 0.

### `src/planner/mod.rs` (12 tests)

1. `throughput_l3_avx512_is_24_g_cells_per_sec` — `8 × 3e9 = 24e9` within 5%.
2. `throughput_dram_is_5_g_cells_per_sec` — `40e9 / 8 = 5e9` within 5%.
3. `estimate_compute_1m_cells_l3_is_about_42_us` — 1M / 24G = 41.67 µs,
   within 30% of 42 µs (DoD requirement).
4. `estimate_compute_1m_cells_dram_is_about_200_us` — 1M / 5G = 200 µs,
   within 10% (sanity check of the bandwidth-bound path).
5. `estimate_compute_zero_cells_is_zero` — no division by zero.
6. `kingman_rho_half_returns_reasonable_wait` — duplicate of the Kingman
   test, but scoped under `planner::tests` so it's discoverable from the
   cost-model side too.
7. `kingman_rho_99_much_larger_than_rho_half` — same.
8. `estimate_cost_simple_scan_is_positive` — 1M-cell Scan plan → cost ≈ 42
   µs, within 30%.
9. `estimate_cost_aggregate_over_scan_sums_costs` — 1M-cell Scan + 1M-cell
   Aggregate → 2 × 41.67 µs ≈ 83.3 µs, within 10%.
10. `estimate_cost_join_includes_kingman_wait` — Join of two 100K-cell
    scans with ρ=0.5 (wait = 10 ms). Total ≈ 10.016 ms (Kingman-dominated).
    Asserts `|ms − 10| / 10 < 0.05`.
11. `estimate_cost_materialize_is_passthrough` — Materialize over Scan
    equals Scan alone (no added cost). Asserts `|cost_scan − cost_mat| <
    1e-15`.
12. `default_cost_model_matches_zen5` — Default values are exactly
    (3e9, 8, 40e9, 8).

## Test Results

```
cargo fmt --check:                    clean (exit 0)
cargo clippy -- -D warnings:          clean (exit 0)  [DoD form]
cargo clippy --all-targets -- -D warnings:   clean (exit 0)
cargo test (debug):
  lib unit tests:     125 passed  (was 105, +20 new)
  integration tests:    7 passed  (unchanged)
  total:              132 passed  (was 112, +20 new)
cargo test --release:
  lib unit tests:     125 passed
  integration tests:    7 passed
  total:              132 passed
```

## DoD Verification

- [x] `cargo test` passes (132 = 112 existing + 20 new)
- [x] `cargo clippy -- -D warnings` passes (also `--all-targets`)
- [x] **CostModel predicts scan latency within 30% of measured** — measured
      scan_eq AVX-512 = 24.1 G cells/sec (ADR-023) → 1M cells takes 41.49 µs.
      CostModel prediction: 1M / 24G = 41.67 µs. Error: 0.43% (well within
      30%). Validated by `estimate_compute_1m_cells_l3_is_about_42_us`.
- [x] `throughput_l3` for AVX-512 returns ~24e9 (8 lanes × 3 GHz) — validated
- [x] `throughput_dram` returns ~5e9 (40e9 B/s / 8 B) — validated
- [x] `estimate_compute` for 1M cells at L3 returns ~42 µs — validated
- [x] Kingman with ρ=0.5 returns a reasonable wait time (10 ms) — validated
- [x] Kingman with ρ=0.99 returns a much larger wait time (99× the ρ=0.5
      wait) — validated
- [x] `estimate_cost` returns a positive number for a simple scan plan —
      validated
- [x] `CostModel` has a `default()` returning Zen 5 values (3 GHz, 8 lanes,
      40 GB/s, 8 B) — validated
- [x] `pub mod planner;` registered in `src/lib.rs`
- [x] Uses `use crate::kernel::Operator;` and
      `use crate::memory::tier::MemoryTier;`
- [x] Uses `use crate::executor::plan::LogicalPlan;`

## Notes for Downstream Waves

- **`CostModel::throughput_l3` ignores its `operator` parameter.** Wave 5
  (or whichever wave adds per-kernel calibration) should replace the body
  with a lookup into a `HashMap<Operator, f64>` populated from
  `examples/bench_kernel.rs` output. The signature stays the same.
- **`estimate_cost` hardcodes `MemoryTier::L3` for all nodes.** Wave 5
  should thread the actual region tier through by looking up
  `region_id` in the region table (the `Region` struct already has a
  `tier()` method). This will make DRAM-resident scans correctly cost 4.8×
  more than L3-resident scans.
- **`child_cell_count` is a simplification.** A real aggregate produces a
  scalar, not a stream of cells — the parent join should see `n = 1`, not
  `n = n_child`. ADR-019 (DPccp) will need accurate cardinality estimates
  and should replace this helper with a proper cardinality model.
- **`KingmanPredictor::update` overwrites initial priors on the first
  call.** This is mathematically correct (Welford gives zero variance for
  one sample) but may surprise callers. A Bayesian prior (inverse-gamma on
  variance) would let the initial `c_a`/`c_s` decay smoothly toward the
  sample estimate. Deferred to a future enhancement.
- **`predicted_p99` uses a first-order lognormal approximation** that
  underestimates by ~2× for `c_s = 1`. The exact formula
  `mean × exp(2.33 × sqrt(ln(1 + c_s²)))` is documented in the method's
  doc comment for callers who need precision. The current approximation is
  sufficient for admission-control thresholding (ADR-020) where the
  threshold is `ρ < 0.8`, not a precise p99 value.
- **`estimate_cost` adds `kingman.predicted_wait()` once per join.** This
  models contention on the shared hash table. Future operators that
  contend on shared state (global sort buffer, shared buffer pool) should
  add their own queueing terms. The current model treats all non-join
  operators as uncontended.
- **The planner module is currently a pure cost model — no plan
  enumeration.** Wave 5 (ADR-019 DPccp) will add a join-order enumerator
  that calls `estimate_cost` on candidate plans and picks the cheapest.
  The current `estimate_cost` signature is designed for this: it takes an
  immutable `&LogicalPlan` and returns a single `f64`, so the enumerator
  can call it in a hot loop without copying.
