# Wave 16 — Adaptive Execution / Eddies

**Agent**: z-ai-code
**Date**: 2026-07-31
**Status**: Complete
**Baseline**: 476 tests (460 lib + 7 integration + 9 doc-tests; 1 ignored) — Wave 15
**After Wave 16**: 514 tests (498 lib + 7 integration + 9 doc-tests; 1 ignored)

## Summary

Implemented adaptive query execution via two complementary mechanisms grounded
in the eddy routing literature (Avnur & Hellerstein, SIGMOD 2000) and the
morsel-driven executor (Leis et al., SIGMOD 2014):

1. **`Eddy`** — adaptively routes each morsel through a set of operators,
   picking the next operator by observed selectivity (most selective first =
   principle of least work). Stops early if a filter empties the morsel.
2. **`AdaptiveExecutor`** — monitors observed vs. estimated cardinality at
   each pipeline stage and triggers a plan switch when divergence exceeds a
   threshold.

Both coexist with the fixed-order `Pipeline` as alternative execution modes.

## Tasks Completed

### 16-1: `src/executor/eddy.rs` — Adaptive Tuple Routing

Implements the `Eddy` struct and `EddyOperator` wrapper per the task spec.

#### Public API

```rust
pub const DEFAULT_LEARNING_RATE: f64 = 0.1;

pub struct EddyOperator {
    pub operator: Operator,
    pub selectivity: f64,
    pub morsels_processed: u64,
    pub cells_output: u64,
}

impl EddyOperator {
    pub fn new(operator: Operator) -> Self;
    pub fn observe(&mut self, input_cells: u64, output_cells: u64, lr: f64);
    pub fn reset(&mut self);
}

pub struct Eddy {
    operators: Vec<EddyOperator>,
    applied: Vec<bool>,
    learning_rate: f64,
}

impl Eddy {
    pub fn new(operators: Vec<Operator>, learning_rate: f64) -> Self;
    pub fn process_morsel(&mut self, morsel: &Morsel, kernel_table: &KernelTable, params: &KernelParams) -> Vec<KernelResult>;
    pub fn selectivities(&self) -> Vec<f64>;
    pub fn routing_order(&self) -> Vec<usize>;
    pub fn reset(&mut self);
    pub fn operator_count(&self) -> usize;
    pub fn operator(&self, idx: usize) -> Option<&EddyOperator>;
}
```

#### Routing algorithm

Per `process_morsel` call:

1. Reset `applied` flags to all-`false`.
2. Loop:
   a. Pick the unapplied operator with the **lowest** selectivity (most
      selective = filters most rows). Ties broken by index (deterministic).
   b. If the morsel is already empty (`current_cells == 0`), stop.
   c. Apply the operator's kernel to the morsel (via `kernel_table.select`
      on `MemoryTier::L3`, same as `Pipeline::execute_morsel`).
   d. Compute `output_cells` from the `KernelResult`:
      - For filter-like operators (`ScanEqU64`, `ScanRangeU64`,
        `ScanMultiPredicate`, `SimilarityHamming`, `HashProbe`,
        `LeapfrogJoin`): `output = result.count` (matching cells).
      - For non-filtering operators (`AggregateSumF64`,
        `AggregateCountDistinct`, `HashBuild`): `output = input_cells`
        (selectivity stays at 1.0 — aggregates don't filter rows).
   e. Update the operator's selectivity via the EMA:
      `selectivity = (1 - lr) * selectivity + lr * (output / input)`,
      where `input` is the current (possibly reduced) cell count and
      `output` is the operator's output cell count. The ratio is clamped
      to `[0, 1]` (a filter cannot amplify).
   f. Update `current_cells` for the next iteration:
      - For filters: `current_cells = result.count` (the morsel shrinks).
      - For non-filters: `current_cells` is unchanged (the morsel passes
        through).
   g. Mark the operator as applied.
   h. If `result.count == 0` (the morsel is now empty), break early.
3. Return the collected `Vec<KernelResult>`.

#### Selectivity model: cascaded

The eddy uses a **cascaded** selectivity model: the "input" to operator `i`
is the output of operator `i-1` (or `morsel.len()` for the first operator).
This matches how a real eddy works — the morsel shrinks as it passes through
each filter. The kernel API returns a count (not a filtered morsel), so the
eddy treats `result.count` as the "logical" output size even though the kernel
ran on the original morsel data. This is documented in the module docstring
under "Selectivity model".

The cascaded model has a known limitation: when the kernel runs on the full
morsel (as it does in the current "simple pipeline" model), the observed
`output` for operator `i` is based on the original morsel, not the filtered
one. This means the selectivity estimate can be inaccurate for chained
filters. The `selectivity_output_cells` ratio is clamped to `[0, 1]` to
handle the case where `output > input` (which happens when the kernel sees
more cells than the cascaded "input" would suggest). A future version that
threads the filtered morsel through stages would fix this; the current
implementation is sufficient to demonstrate the principle of least work and
the early-termination benefit (see benchmark results below).

#### Early termination

If an operator produces zero output cells (`result.count == 0` for a filter),
the eddy stops — in a real eddy, the morsel would be empty and downstream
operators would have no work to do. This is the eddy's main work-saving
mechanism in the current simple-pipeline model.

The eddy also checks `current_cells == 0` *before* applying an operator, to
avoid dragging an operator's selectivity toward 0 by observing it on an empty
morsel (which would be misleading — the operator isn't selective, it just had
no input).

### 16-2: `src/executor/adaptive.rs` — Adaptive Plan Switching

Implements the `AdaptiveExecutor` monitor per the task spec.

#### Public API

```rust
pub struct AdaptiveExecutor {
    plan: LogicalPlan,
    estimated_cardinalities: Vec<usize>,
    observed_cardinalities: Vec<usize>,
    has_observed: Vec<bool>,
    switch_threshold: f64,
    switched: bool,
}

impl AdaptiveExecutor {
    pub fn new(plan: LogicalPlan, estimated: Vec<usize>, threshold: f64) -> Self;
    pub fn observe(&mut self, stage: usize, observed: usize) -> bool;
    pub fn should_switch(&self) -> bool;
    pub fn max_divergence(&self) -> f64;
    pub fn switch_threshold(&self) -> f64;
    pub fn stage_count(&self) -> usize;
    pub fn estimated(&self, stage: usize) -> Option<usize>;
    pub fn observed(&self, stage: usize) -> Option<usize>;
    pub fn plan(&self) -> &LogicalPlan;
}
```

#### Divergence metric

```
divergence_i = |observed_i - estimated_i| / max(estimated_i, 1)
max_divergence = max over all *observed* stages of divergence_i
```

The `max(estimated, 1)` guard prevents divide-by-zero when the estimate is 0.
For `estimated == 0`:
- `observed == 0` → divergence = 0 (correct).
- `observed == N > 0` → divergence = `N` (the estimate missed every row).

#### Unobserved-stage handling

A `has_observed: Vec<bool>` field tracks which stages have been observed.
`max_divergence()` only considers stages where `has_observed[i]` is `true`.
This prevents false positives: an unobserved stage has `observed = 0`, which
would otherwise register as a 100% underestimate and trigger a spurious
switch. The `observed()` accessor returns `None` for unobserved stages
(instead of `Some(0)`).

This was a bug found during testing: the initial implementation initialized
`observed_cardinalities` to `vec![0; n]` and considered all stages in
`max_divergence()`. The test `adaptive_exact_match_is_zero_divergence` failed
because observing stage 0 with an exact match still left stages 1 and 2
"observed = 0", giving `max_divergence = 1.0 > 0.5` → spurious switch. The
`has_observed` flag fixes this.

#### Stickiness

Once `switched` is set to `true` (by `observe` when `max_divergence >
threshold`), it stays `true` for the executor's lifetime. This prevents
flapping when divergence oscillates around the threshold. Callers that want
to reset must construct a new `AdaptiveExecutor` (typically after re-planning
with the observed cardinalities as the new estimates).

### 16-3: Pipeline integration

In `src/executor/pipeline.rs`, added:

```rust
pub fn execute_with_eddy(
    &mut self,
    morsel: &Morsel,
    eddy: &mut Eddy,
    kernel_table: &KernelTable,
    params: &KernelParams,
) -> Result<()>
```

This delegates to `eddy.process_morsel(...)` and extends the pipeline's
`results` accumulator with the eddy's output. The pipeline's own `stages`
field is **not** used — the eddy's operators are applied instead. This makes
the eddy an alternative execution mode that coexists with
`execute_morsel` (fixed-order).

The docstring explains when to use each:
- `execute_morsel` — when selectivities are known and the stage order is
  already optimal.
- `execute_with_eddy` — when selectivities are unknown or skewed, and the
  eddy's adaptive routing can save work by reordering filters per-morsel.

### 16-4: `benches/bench_eddy.rs` — Benchmark

Three benchmark groups:

1. **`eddy/uniform`** — 3 filters with equal selectivity (0.5 each) on
   alternating 0/1 data. All three operators match the 0s. No early
   termination. The eddy should match the fixed pipeline.

2. **`eddy/skewed`** — 3 filters where the last (in fixed order) is
   `ScanMultiPredicate(Eq(0), Eq(1), count=2)` — a contradictory predicate
   that matches no cells (selectivity 0.0). The eddy learns this on the
   first morsel, then on subsequent morsels applies it first → zero output →
   early termination → skips the other two filters. The fixed pipeline
   always runs all 3.

3. **`eddy/adaptive_switching`** — measures the `AdaptiveExecutor`'s
   `observe()` + `max_divergence()` overhead. Three variants:
   `observe_10x_misestimate`, `observe_accurate`, `observe_5_stages`.

Each benchmark includes a sanity-check assertion at setup time to verify the
eddy's behavior (e.g., the skewed benchmark asserts that the second morsel
early-terminates after 1 operator).

#### Measured performance (1s measurement time, dev box)

| Benchmark                        | Time          | Throughput       |
|----------------------------------|---------------|------------------|
| `eddy/uniform/fixed_pipeline`    | 410.8 µs      | 243.5 Kelem/s    |
| `eddy/uniform/eddy`              | 411.1 µs      | 243.3 Kelem/s    |
| `eddy/skewed/fixed_pipeline`     | 415.2 µs      | 240.9 Kelem/s    |
| `eddy/skewed/eddy`               | 35.0 µs       | 2.856 Melem/s    |
| `eddy/adaptive_switching/observe_10x_misestimate` | 56.8 ns | 17.61 Melem/s |
| `eddy/adaptive_switching/observe_accurate`        | 56.9 ns | 17.57 Melem/s |
| `eddy/adaptive_switching/observe_5_stages`        | 98.0 ns | 10.21 Melem/s |

**Key results**:
- **Uniform**: eddy matches fixed pipeline (411 µs vs 411 µs) — the routing
  overhead is negligible when there's no reordering benefit. ✓
- **Skewed**: eddy is **~12× faster** (35 µs vs 415 µs) — far exceeding the
  "~2× faster" target. The speedup comes from early termination: the eddy
  runs 1 operator on 99 of 100 morsels, while the fixed pipeline runs 3 on
  every morsel (300 vs 102 kernel executions). ✓
- **Adaptive switching**: divergence detection is sub-100ns — the overhead
  of monitoring is negligible compared to query execution. ✓

### 16-5: Tests

Added 38 new tests (17 eddy + 18 adaptive + 3 pipeline-with-eddy), bringing
the total from 476 to 514.

#### `src/executor/eddy.rs` (17 tests)

- `eddy_processes_morsel_through_three_operators_collects_all_results` (DoD #1)
- `eddy_most_selective_operator_applied_first` (DoD #2)
- `eddy_selectivity_updates_after_observation` (DoD #3)
- `eddy_empty_morsel_after_first_filter_stops_early` (DoD #4)
- `eddy_routing_order_on_fresh_eddy_is_declaration_order`
- `eddy_routing_order_picks_lowest_selectivity_first`
- `eddy_reset_clears_selectivities_and_counters`
- `eddy_learning_rate_zero_keeps_selectivity_at_one`
- `eddy_empty_operator_list_returns_no_results`
- `eddy_learning_rate_clamped_to_unit_interval`
- `eddy_exponential_weighting_converges_to_true_selectivity`
- `eddy_operator_observe_handles_empty_input`
- `eddy_operator_observe_clamps_ratio_above_one`
- `eddy_debug_format_works`
- `eddy_operator_count_matches_construction`
- `eddy_operator_accessor_returns_some_for_valid_index`
- `eddy_aggregate_operator_selectivity_stays_at_one`

#### `src/executor/adaptive.rs` (18 tests)

- `adaptive_divergence_triggers_switch_at_threshold` (DoD #5)
- `adaptive_no_switch_when_estimates_accurate` (DoD #6)
- `adaptive_switch_is_sticky_once_triggered`
- `adaptive_max_divergence_takes_max_across_stages`
- `adaptive_zero_estimated_zero_observed_is_zero_divergence`
- `adaptive_zero_estimated_nonzero_observed_is_large_divergence`
- `adaptive_observe_out_of_bounds_stage_is_noop`
- `adaptive_underestimate_triggers_switch`
- `adaptive_exact_match_is_zero_divergence`
- `adaptive_threshold_zero_triggers_on_any_misestimate`
- `adaptive_threshold_large_never_triggers`
- `adaptive_stage_count_matches_estimated_length`
- `adaptive_estimated_and_observed_accessors`
- `adaptive_plan_accessor_returns_original_plan`
- `adaptive_switch_threshold_accessor`
- `adaptive_debug_format_works`
- `divergence_function_handles_typical_cases`
- `adaptive_10x_misestimate_triggers_switch_at_threshold_1`

#### `src/executor/pipeline.rs` (3 new tests)

- `pipeline_with_eddy_produces_same_results_as_fixed_pipeline` (DoD #7)
- `pipeline_with_eddy_accumulates_across_morsels`
- `pipeline_with_eddy_reset_clears_results`

#### DoD #8: Benchmark compiles

Verified via `cargo build --benches` — all benchmarks (including
`bench_eddy`) compile cleanly in the `bench` profile.

## Cargo.toml changes

- Added `[[bench]] name = "bench_eddy"` entry.

## Other modifications

- `src/executor/mod.rs` — added `pub mod adaptive;`, `pub mod eddy;`,
  `pub use adaptive::AdaptiveExecutor;`, `pub use eddy::{Eddy, EddyOperator,
  DEFAULT_LEARNING_RATE};`, and a "Wave 16" section in the module docstring.
- `src/executor/pipeline.rs` — added `use crate::executor::eddy::Eddy;`
  import, the `execute_with_eddy` method, and 3 new tests.
- `src/kernel/mod.rs` — added `PartialEq` to `KernelResult`'s derive list
  (needed for the pipeline+eddy correctness test to compare sorted result
  vectors with `assert_eq!`). `f64` implements `PartialEq` (not `Eq`, due to
  NaN), so the derive adds `PartialEq` but not `Eq` — this is the correct
  choice for a struct containing `f64`.

## DoD Verification

| Criterion                                                        | Status |
|------------------------------------------------------------------|--------|
| `cargo test` passes (476 existing + new tests)                   | ✅ 514 tests pass (498 lib + 7 integration + 9 doc; 1 ignored) |
| `cargo clippy -- -D warnings` passes                             | ✅ clean (all targets) |
| `cargo build --benches` compiles (including bench_eddy)          | ✅ compiles in `bench` profile |
| Eddy routes most-selective-first (principle of least work)       | ✅ `eddy_most_selective_operator_applied_first` + `eddy_routing_order_picks_lowest_selectivity_first` |
| AdaptiveExecutor detects cardinality divergence at threshold     | ✅ `adaptive_divergence_triggers_switch_at_threshold` + `adaptive_10x_misestimate_triggers_switch_at_threshold_1` |
| Pipeline with eddy produces correct results (same as fixed)      | ✅ `pipeline_with_eddy_produces_same_results_as_fixed_pipeline` |

## Files Created / Modified

### Created

- `src/executor/eddy.rs` (716 lines, 17 tests)
- `src/executor/adaptive.rs` (459 lines, 18 tests)
- `benches/bench_eddy.rs` (325 lines, 3 benchmark groups)

### Modified

- `Cargo.toml` — added `[[bench]]` entry for `bench_eddy`.
- `src/executor/mod.rs` — added module declarations, re-exports, docstring.
- `src/executor/pipeline.rs` — added `execute_with_eddy` method + 3 tests.
- `src/kernel/mod.rs` — added `PartialEq` to `KernelResult` derive.

## Design Notes

### Why cascaded selectivity (not independent)?

The task spec says `selectivity = (1 - lr) * selectivity + lr * (output /
input)`. The "input" and "output" are the operator's actual input and output.
In a real eddy, the input to operator `i` is the output of operator `i-1`
(the morsel shrinks as it passes through filters). The cascaded model captures
this: `input = current_cells` (the running cell count, reduced by each
filter).

The alternative (independent model: `input = morsel.len()` always) would give
the per-operator selectivity on the original data, which is useful for routing
but doesn't capture the compounding effect of chained filters. The cascaded
model is more aligned with the "principle of least work" — applying a
selective filter first reduces the input to downstream filters, so the
"observed" selectivity of a downstream filter should be relative to the
reduced input.

The cascaded model has a known limitation in the current simple-pipeline
implementation: the kernel runs on the *original* morsel (not the filtered
one), so `result.count` is based on the full morsel. This means the observed
`output / input` ratio can exceed 1.0 (when the previous filter reduced
`current_cells` below `result.count`). The ratio is clamped to `[0, 1]` to
handle this. A future version that threads the filtered morsel through stages
would make the cascaded model exact.

### Why `has_observed` flag (not initializing observed = estimated)?

The initial implementation initialized `observed_cardinalities` to
`vec![0; n]` and considered all stages in `max_divergence()`. This caused
false positives: an unobserved stage has `observed = 0`, which registers as a
100% underestimate (divergence = 1.0 for any non-zero estimate). The test
`adaptive_exact_match_is_zero_divergence` caught this — observing stage 0 with
an exact match still triggered a switch because stages 1 and 2 were
"observed = 0".

Two fixes were considered:
1. Initialize `observed = estimated` (so unobserved stages have 0 divergence).
2. Track `has_observed` and exclude unobserved stages from `max_divergence`.

Option 2 was chosen because it preserves the semantics of `observed()` —
returning `None` for an unobserved stage is more honest than returning the
estimated value (which would be misleading). It also makes the "I haven't
observed this stage yet" state explicit.

### Why `PartialEq` on `KernelResult`?

The pipeline+eddy correctness test (`pipeline_with_eddy_produces_same_results_as_fixed_pipeline`)
needs to compare two `Vec<KernelResult>` after sorting. This requires
`KernelResult: Ord` (for `sort_by_key`) and `KernelResult: PartialEq` (for
`assert_eq!`). `KernelResult` already had `Debug, Clone, Copy, Default`.

Adding `PartialEq` is safe: `f64` implements `PartialEq` (via bitwise
comparison), so the derive works. `Eq` is not derivable (because `f64` is not
`Eq` due to NaN), but `PartialEq` is sufficient for `assert_eq!`.

For sorting, the test uses `sort_by_key(|r| (r.count, r.sum.to_bits(),
r.mask))` — `f64::to_bits()` converts the `f64` to a `u64` which is `Ord`,
so the sort key is `Ord`. This handles NaN correctly (NaN has a specific bit
pattern that sorts consistently).

### Why not use the eddy to replace the pipeline?

The task explicitly says "The eddy does NOT replace the pipeline — it's an
alternative execution mode." The two coexist:
- `Pipeline::execute_morsel` — fixed-order, simpler, faster when selectivities
  are known.
- `Pipeline::execute_with_eddy` — adaptive, better when selectivities are
  unknown or skewed.

A real query optimizer would choose between them based on the plan's
selectivity confidence (e.g., use the fixed pipeline when the cardinality
estimator has high confidence, use the eddy when confidence is low). This
routing decision is left to the caller — the executor provides both paths.

## Future Work

- **Filtered morsel threading**: the current eddy runs each operator on the
  *original* morsel (the kernel API returns a count, not a filtered morsel).
  A future version could use the kernel's `mask` field (for `ScanEq`, the
  first 64 cells' match bits) or re-run the filter in scalar code to produce
  a filtered morsel for the next stage. This would make the cascaded
  selectivity model exact and unlock the full speedup of "most selective
  first" (currently, the speedup comes only from early termination).

- **Per-operator params**: the eddy currently takes a single `KernelParams`
  shared by all operators. This limits the eddy to operators that can be
  parameterized by the same `KernelParams` (e.g., `ScanEq` and
  `ScanMultiPredicate` share `target_u64`). A future version could store
  per-operator params in `EddyOperator`, allowing multiple `ScanEq` operators
  with different targets.

- **Eddy + AdaptiveExecutor integration**: the `AdaptiveExecutor` detects
  divergence but doesn't automatically switch to the eddy. A future version
  could wire them together: if `AdaptiveExecutor::should_switch()` returns
  true, switch from `execute_morsel` to `execute_with_eddy` for the remaining
  morsels. This would make the adaptation fully automatic.

- **Parallel eddy**: the eddy is currently single-threaded (per worker). A
  parallel version could have multiple workers share selectivity estimates
  (via atomic operations) and adapt the routing order collectively.
