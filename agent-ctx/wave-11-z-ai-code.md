# Wave 11: Join Ordering + Planner (ADR-019)

**Agent**: z-ai-code
**Date**: 2026-07-14
**Status**: COMPLETE

## Summary

Implemented Wave 11 of the turboGP database engine: the DPccp join ordering
algorithm, a simple cardinality estimator, and a cost-aware plan lowerer.
All 4 tasks (11-1 through 11-4) are complete.

## Files Created

| File | Lines | Purpose |
|------|-------|---------|
| `src/planner/dpccp.rs` | ~400 | DPccp left-deep join ordering (ADR-019) |
| `src/planner/cardinality.rs` | ~210 | Simple per-table cardinality + selectivity estimator |
| `src/planner/lowerer.rs` | ~260 | Cost-aware `LogicalPlan → KernelInvocation` lowering |

## Files Modified

| File | Change |
|------|--------|
| `src/planner/mod.rs` | Registered 3 new submodules; added `pub use` re-exports for `dpccp`, `JoinRelation`, `JoinTree`, `CardinalityEstimator`, `PlanLowerer`; expanded the module-level docstring. |
| `src/sketch/hll.rs` | Fixed a pre-existing clippy `manual_range_contains` warning (line 227) that was blocking `cargo clippy --all-targets -- -D warnings`. The baseline was already failing this check before Wave 11. |

## Design Decisions

### DPccp (Task 11-1)

- **Left-deep simplification**: at each DP step, we expand a connected subset
  `S` by joining it with a single relation `j`. This is O(n²·2ⁿ) vs full
  DPccp's O(3ⁿ), and is sufficient for the n ≤ 15 limit (ADR-019). Bushy
  plans are supported by the `JoinTree` data structure for future extension
  but not produced by `dpccp()`.

- **Bitmask DP**: subsets are represented as `u16` bitmasks (n ≤ 15 ⇒ 16
  bits). The DP table is a `HashMap<u16, (JoinTree, f64)>`.

- **Cost formula** (per spec): `cost(S ⋈ j) = cost(S) + cost(j) + |S|·|j|`,
  where `|·|` is the subtree's output cardinality. Leaves have cost 0
  (table scans are costed separately by `CostModel`).

- **Cardinality of join**: `|R ⋈ S| = max(|R|, |S|)` (the FK-join assumption:
  one side has a unique key, so each row on the smaller side matches at most
  one row on the larger side).

- **Tie-breaking**: the symmetric cost formula means many orderings tie.
  We break ties by preferring plans whose **leftmost leaf has smaller
  cardinality** — the classical heuristic of driving the join from the
  smallest relation. This is what makes the 3-table star test produce a
  satellite-first plan (S1 ⋈ C rather than C ⋈ S1).

- **Errors**: returns `Error::InvalidArg` for n=0, n>15, or disconnected
  join graphs.

### Cardinality Estimator (Task 11-2)

- **FK-join heuristic**: since we don't track per-column distinct counts,
  we assume `distinct(R.k) ≈ |R|` and `distinct(S.k) ≈ |S|`, giving
  `|R ⋈ S| = |R|·|S| / max(|R|,|S|) = min(|R|,|S|)`.
- **Selectivity defaults**: `0.1` for equality (1/distinct, assuming ~10
  distinct values), `0.33` for range (Selinger's classical default).
- The `_left_key`/`_right_key`/`_table` parameters are accepted for API
  stability but unused in this simple version — a future column-stats
  extension (deferred to ADR-016) will index them.

### Plan Lowerer (Task 11-3)

- **Cost-aware tier selection**: for each operator, the lowerer enumerates
  the candidate tiers (L3, Ddr5, Cxl), checks the `KernelTable` for a
  genuine kernel at each tier (not a fallback), and picks the cheapest
  tier per the `CostModel`.
- **Genuine-kernel check**: `KernelTable::select` falls back to "any kernel
  for this operator" if no exact match exists. We check the returned
  kernel's own `.tier()` against the requested tier to detect this
  fallback path and skip such tiers.
- **Default L3**: if no tier has a genuine kernel (shouldn't happen for
  any registered operator), falls back to `L3` — matching the original
  `lower_to_kernels` behavior.
- **Same emission order** as `lower_to_kernels` (children before parents).

## Tests Added

26 new lib tests + 1 new doc-test = 27 new tests.

### dpccp.rs (10 tests)
- `dpccp_two_table_join_produces_simple_plan` — 2-table join → Inner node
- `dpccp_single_table_returns_leaf` — single table → Leaf
- `dpccp_three_table_star_joins_satellite_first` — 3-table star, leftmost = satellite
- `dpccp_five_table_chain_returns_valid_plan` — 5-table chain, valid plan
- `dpccp_rejects_more_than_fifteen_relations` — n > 15 → error
- `dpccp_rejects_empty_input` — n = 0 → error
- `dpccp_rejects_disconnected_graph` — disconnected → error
- `join_tree_cost_and_cardinality_accessors` — accessor methods
- `is_connected_handles_undirected_graph` — undirected adjacency
- `dpccp_cost_formula_is_correct` — cost formula verification

### cardinality.rs (9 tests + 1 doc-test)
- `estimate_join_with_known_sizes_returns_min` — FK-join gives min(|R|,|S|)
- `estimate_join_is_symmetric` — R⋈S = S⋈R
- `estimate_join_unknown_table_returns_zero` — unknown → 0
- `estimate_selectivity_equality_returns_default` — equality → 0.1
- `estimate_selectivity_range_returns_033` — range → 0.33
- `estimate_selectivity_unknown_defaults_to_01` — unknown → 0.1
- `add_table_overwrites` — overwrite behavior
- `table_row_count_unknown_returns_zero` — unknown → 0
- `default_estimator_is_empty` — Default impl

### lowerer.rs (7 tests)
- `lowerer_scan_produces_one_invocation` — scan → 1 invocation
- `lowerer_aggregate_produces_two_invocations` — agg-over-scan → 2
- `lowerer_join_produces_three_invocations` — join → 3
- `lowerer_materialize_is_passthrough` — materialize → 0 (passthrough)
- `pick_best_tier_scan_prefers_l3` — scan picks L3
- `pick_best_tier_aggregate_returns_l3` — aggregate picks L3
- `pick_best_tier_hash_probe_returns_l3` — hash probe picks L3
- `cell_count_extracts_correctly` — cell_count helper

## DoD Verification

```
$ cargo fmt
$ cargo clippy -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.45s

$ cargo clippy --all-targets -- -D warnings   # extra: stricter check
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.08s

$ cargo test
test result: ok. 340 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out  (lib)
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out    (integration)
test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out    (doc-tests)
```

- Baseline: 320 tests (313 lib + 7 integration).
- After Wave 11: 348 tests (340 lib + 7 integration + 1 doc-test).
- +28 new tests, all passing.
- `cargo clippy -- -D warnings` passes (the exact command from the task).
- `cargo clippy --all-targets -- -D warnings` also passes (after fixing a
  pre-existing warning in `src/sketch/hll.rs`).
- DPccp produces valid join trees for n ∈ {1, 2, 3, 5}.
- PlanLowerer generates correct kernel invocations (scan→1, agg→2, join→3).

## Required Test Coverage (per task spec)

| # | Required Test | Implemented As |
|---|---------------|----------------|
| 1 | DPccp 3-table star → satellite first | `dpccp_three_table_star_joins_satellite_first` |
| 2 | DPccp 5-table → valid plan | `dpccp_five_table_chain_returns_valid_plan` |
| 3 | DPccp 2-table → simple plan | `dpccp_two_table_join_produces_simple_plan` |
| 4 | DPccp single table → leaf | `dpccp_single_table_returns_leaf` |
| 5 | CardinalityEstimator estimate_join | `estimate_join_with_known_sizes_returns_min` |
| 6 | CardinalityEstimator equality 0.1 | `estimate_selectivity_equality_returns_default` |
| 7 | PlanLowerer scan → 1 invocation | `lowerer_scan_produces_one_invocation` |
| 8 | PlanLowerer aggregate → 2 invocations | `lowerer_aggregate_produces_two_invocations` |

All 8 required tests present and passing.

## Public API Surface

```rust
// planner/dpccp.rs
pub struct JoinRelation { pub name: String, pub cardinality: usize, pub joins_with: Vec<usize> }
pub enum JoinTree { Leaf(JoinRelation), Inner { left: Box<JoinTree>, right: Box<JoinTree>, cost: f64, cardinality: usize } }
impl JoinTree { pub fn cost(&self) -> f64; pub fn cardinality(&self) -> usize; }
pub fn dpccp(relations: &[JoinRelation]) -> Result<JoinTree>;

// planner/cardinality.rs
pub struct CardinalityEstimator { /* table_stats: HashMap<String, usize> */ }
impl CardinalityEstimator {
    pub fn new() -> Self;
    pub fn add_table(&mut self, name: &str, row_count: usize);
    pub fn estimate_join(&self, left: &str, right: &str, _left_key: &str, _right_key: &str) -> usize;
    pub fn estimate_selectivity(&self, _table: &str, predicate_type: &str) -> f64;
    pub fn table_row_count(&self, name: &str) -> usize;
}
impl Default for CardinalityEstimator;

// planner/lowerer.rs
pub struct PlanLowerer { /* cost_model: CostModel, kernel_table: Arc<KernelTable> */ }
impl PlanLowerer {
    pub fn new(cost_model: CostModel, kernel_table: Arc<KernelTable>) -> Self;
    pub fn lower(&self, plan: &LogicalPlan) -> Vec<KernelInvocation>;
}
```

Re-exported from `crate::planner`:
- `CardinalityEstimator`
- `dpccp`, `JoinRelation`, `JoinTree`
- `PlanLowerer`
- (existing) `KingmanPredictor`, `CostModel`, `estimate_cost`

## Notes for Future Waves

- The DPccp implementation is **left-deep only**. A future wave could extend
  it to full bushy DPccp (consider joining two non-singleton subsets), which
  is O(3ⁿ) but produces better plans on cyclic schemas.
- IDP (Iterative Dynamic Programming) for n > 15 is **not yet implemented**.
  ADR-019 specifies IDP with block size k=8 for 16 ≤ n ≤ 40, and greedy
  GOO for n > 40. Currently `dpccp()` returns `Error::InvalidArg` for n > 15.
- The cardinality estimator is **per-table only**. A future `ANALYZE`
  command could populate per-column distinct-value counts and make
  `estimate_join` and `estimate_selectivity` more accurate.
- The plan lowerer always picks L3 in the current cost model (because
  `estimate_compute` only sees `n_cells`, not residency). A residency-aware
  cost model that knows the working-set size vs. L3 capacity would flip
  the choice for large scans.
