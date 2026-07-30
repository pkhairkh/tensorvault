# Wave 15 — MCTS Plan Search + Graph Pruning

**Agent**: z-ai-code
**Date**: 2026-07-31
**Status**: Complete
**Baseline**: 443 tests (428 lib + 7 integration + 8 doc-tests) — Wave 14
**After Wave 15**: 476 tests (460 lib + 7 integration + 9 doc-tests; 1 ignored)

## Tasks Completed

### 15-1: `src/planner/mcts.rs` — MCTS Join Ordering

Implemented training-free Monte Carlo Tree Search for join ordering, used
when `n > 15` relations (the DPccp limit). The search follows the standard
four-phase MCTS loop:

1. **Selection** — descend from the root using UCT.
2. **Expansion** — add one untried child (a relation adjacent to the current
   covered set).
3. **Simulation** — randomly complete the join order to a leaf, computing
   the total cost (using the same FK-join cost model as DPccp).
4. **Backpropagation** — update `visits` and `total_cost` along the path.

After `max_iterations`, the best complete plan found is returned as a
`JoinTree` (drop-in compatible with DPccp's output).

#### Public API

```rust
pub struct MctsJoinOrderer {
    exploration: f64,        // default √2
    max_iterations: usize,   // default 10_000
    seed: u64,               // default 0xC0FFEE — deterministic
}

impl MctsJoinOrderer {
    pub fn new() -> Self;
    pub fn with_exploration(self, f64) -> Self;
    pub fn with_iterations(self, usize) -> Self;
    pub fn with_seed(self, u64) -> Self;
    pub fn order(&self, relations: &[JoinRelation]) -> Result<JoinTree>;
}
```

#### UCT for cost minimization

The task description specifies `uct = avg_cost + exploration · sqrt(ln(N) / n)`
with "lower is better". The literal formula is ambiguous (it would, as
written, pick high-cost children). The conventional interpretation for
**minimization** uses `reward = -cost`:

```
uct = -avg_cost + exploration · sqrt(ln(parent_visits) / child_visits)
```

We pick the **maximum** UCT. This is documented in the module docstring.

Unvisited children always get `+∞` (picked first). Children with poisoned
(infinite) `avg_cost` (disconnected graph rollouts) get `-∞` (skipped).

#### Cost model

Reuses the DPccp cost model verbatim:
- Leaves cost 0.
- `cost(S ⋈ j) = cost(S) + cost(j) + |S| · |j|`.
- `|S ⋈ j| = max(|S|, |j|)` (FK assumption).

The `MctsNode.total_cost` field tracks the **full plan cost from the root to
the leaf** (not just the cost from this node), so `total_cost / visits` is
directly comparable across nodes at different depths.

#### Disconnected-graph handling

If `valid_children(covered)` returns an empty set before `covered == full_mask`,
the simulation returns `f64::INFINITY` for the remaining cost. The main loop
discards incomplete plans (only plans with `path.len() == n` are accepted).
After all iterations, if no valid complete plan was found, `order` returns
`Error::InvalidArg("... join graph may be disconnected ...")`.

#### Determinism

The PRNG is seeded with a fixed constant (`0xC0FFEE`) by default, so the same
input always produces the same plan. This is important for plan-cache
stability and for reproducible tests. The `with_seed` builder allows callers
to override.

### 15-2: `src/planner/graph_prune.rs` — Graph-Based Pruning

Implements two pruning rules based on the join graph:

#### Connectivity pruning (`valid_children`)

A valid join plan never produces a cross product: every added relation must
share a join predicate with at least one relation already in the plan. The
pruner enforces this by returning only relations adjacent to the current
covered set (or all relations if the set is empty — the first pick is free).

This is the same constraint enforced by DPccp's "connected complement pairs"
and by Selinger's original connectivity pruning, but exposed as an O(n)
method for use inside the MCTS loop.

#### Admissible lower bound (`lower_bound`)

```
lb(covered) = max_card(covered) · Σ_{r ∉ covered} card(r)
```

This is **admissible** (never overestimates the true optimal cost) under the
FK-join cost model:

- `|S|` (the cardinality of the current partial result) only grows as we add
  relations, so each future join costs at least `max_card(covered) · card(r)`.
- Each remaining relation `r` must be joined exactly once, so we sum over all
  remaining relations.

The bound is **tight** for star queries (where every satellite joins at cost
`center_card · sat_card`, and the cardinality never grows because
`center_card > sat_card`). It is loose for chains of large relations.

#### Public API

```rust
pub struct GraphPruner { adjacency: Vec<Vec<usize>> }

impl GraphPruner {
    pub fn new(relations: &[JoinRelation]) -> Self;
    pub fn valid_children(&self, covered: u64) -> Vec<usize>;
    pub fn lower_bound(&self, covered: u64, relations: &[JoinRelation]) -> f64;
}
```

The adjacency list is built once at construction (O(|E|)) and reused across
all MCTS iterations, so `valid_children` is O(n) per call.

### 15-3: Integration with the existing planner

In `src/planner/mod.rs`:

- Added `pub mod mcts;` and `pub mod graph_prune;` module declarations.
- Added `pub use graph_prune::GraphPruner;` and `pub use mcts::MctsJoinOrderer;`
  re-exports.
- Added `use crate::error::Result;` import (needed for `order_joins`).
- Added the dispatcher function:

```rust
pub fn order_joins(relations: &[JoinRelation]) -> Result<JoinTree> {
    if relations.len() <= 15 {
        dpccp(relations)
    } else {
        MctsJoinOrderer::new().order(relations)
    }
}
```

This is the single entry point for join ordering in turboGP — callers do not
need to know which algorithm is appropriate for their query size. The
returned `JoinTree` is drop-in compatible with both planners.

Updated the module-level docstring to mention the new submodules.

### 15-4: `benches/bench_planner.rs` — DPccp vs MCTS Benchmark

Created a criterion benchmark with four workload groups:

1. **`planner/n5_star`** — 5-relation star (center C(1000) + 4 satellites
   S1..S4(10 each)). Both DPccp and MCTS handle this. The benchmark asserts
   at setup time that MCTS finds a plan within 2× of the DPccp optimum (it
   usually finds the optimum exactly). Measures:
   - `dpccp` — optimal DPccp plan.
   - `mcts_2000` — MCTS with 2000 iterations.
   - `order_joins_dispatch` — the `order_joins` dispatcher (delegates to
     DPccp for n ≤ 15).

2. **`planner/n10_chain`** — 10-relation chain with varying cardinalities
   (drawn from a deterministic splitmix64 PRNG). Both planners handle this.
   Same three benchmark variants.

3. **`planner/n20_chain`** — 20-relation chain. MCTS only (DPccp rejects).
   The benchmark asserts at setup time that DPccp rejects n=20. Varies the
   MCTS iteration budget (500, 2000, 5000) to show the cost-vs-quality
   trade-off, plus the `order_joins_dispatch` variant.

4. **`planner/n30_chain`** — 30-relation chain. MCTS only. Same structure
   as n=20.

Throughput is reported in `Elements/sec` (joins/sec), where "elements" is
the relation count `n`.

#### Measured performance (Zen 5 dev box, --quick mode)

| Workload              | DPccp      | MCTS (2000 iters) | MCTS (5000 iters) |
|-----------------------|------------|-------------------|-------------------|
| n=5 star              | 16.7 µs    | 665 µs            | —                 |
| n=10 chain            | (similar)  | —                 | —                 |
| n=20 chain            | (rejects)  | ~3.9 ms           | ~9.7 ms           |
| n=30 chain            | (rejects)  | ~6.7 ms           | ~17.0 ms          |

MCTS scales roughly linearly with `iterations × n`, as expected (each
iteration does O(n) work: O(n) for `valid_children` plus O(n) for the
simulation rollout).

### 15-5: Tests

Added 33 new tests (32 unit + 1 doc-test), bringing the total from 443 to
476:

#### `src/planner/graph_prune.rs` (12 tests)

- `graph_pruner_builds_symmetric_adjacency_for_chain`
- `graph_pruner_normalizes_asymmetric_joins_with`
- `valid_children_for_empty_set_returns_all_relations` (DoD #5)
- `valid_children_after_adding_relation_zero_returns_only_neighbors` (DoD #6)
- `valid_children_for_chain_middle_after_covering_ends`
- `valid_children_for_full_set_is_empty`
- `valid_children_excludes_disconnected_relations`
- `lower_bound_is_non_negative` (DoD #7)
- `lower_bound_matches_formula_for_partial_set`
- `lower_bound_for_empty_set_is_zero`
- `lower_bound_for_full_set_is_zero`
- `lower_bound_is_tight_for_star_query_from_center`

#### `src/planner/mcts.rs` (15 tests)

- `mcts_three_relation_chain_finds_valid_plan` (DoD #1)
- `mcts_five_relation_star_finds_valid_plan` (DoD #2)
- `mcts_ten_relation_chain_finds_valid_plan_within_1000_iterations` (DoD #3)
- `mcts_twenty_relation_chain_finds_valid_plan` (DoD #4)
- `mcts_thirty_relation_chain_finds_valid_plan`
- `mcts_plan_cost_within_2x_of_dpccp_optimal_for_n5` (DoD #9)
- `mcts_rejects_empty_input`
- `mcts_single_relation_returns_leaf`
- `mcts_rejects_disconnected_graph`
- `mcts_rejects_more_than_64_relations`
- `step_join_cost_is_zero_for_first_relation`
- `step_join_cost_uses_max_covered_cardinality`
- `max_covered_card_handles_empty_and_nonempty`
- `build_left_deep_tree_computes_correct_cost`
- `mcts_is_deterministic_with_fixed_seed`

#### `src/planner/mod.rs` (5 tests + 1 doc-test)

- `order_joins_uses_dpccp_for_small_n` (DoD #8)
- `order_joins_uses_mcts_for_large_n` (DoD #8)
- `order_joins_at_dpccp_boundary_uses_dpccp`
- `order_joins_just_past_dpccp_boundary_uses_mcts`
- `order_joins_rejects_empty_input`
- Doc-test for `order_joins` (the example in the rustdoc)

## Cargo.toml changes

- Added `rand = "0.9"` to `[dependencies]` (used for MCTS simulation). The
  task description said `rand` was already in Cargo.toml, but it was not —
  the project's existing benchmarks use a hand-rolled `splitmix64` PRNG. For
  the MCTS module (which is in the main library, not just benchmarks), I
  added `rand` as a proper dependency so it gets tracked in `Cargo.lock`.
- Added a new `[[bench]]` entry for `bench_planner`:
  ```toml
  [[bench]]
  name = "bench_planner"
  harness = false
  path = "benches/bench_planner.rs"
  ```

## DoD Verification

| Criterion                                                        | Status |
|------------------------------------------------------------------|--------|
| `cargo test` passes (443 existing + new tests)                   | ✅ 476 tests pass |
| `cargo clippy -- -D warnings` passes                             | ✅ clean |
| `cargo build --benches` compiles                                 | ✅ compiles in `bench` profile |
| MCTS finds valid plans for n=20 (where DPccp can't)              | ✅ `mcts_twenty_relation_chain_finds_valid_plan` |
| MCTS plan cost within 2× of DPccp optimal for n=5                | ✅ `mcts_plan_cost_within_2x_of_dpccp_optimal_for_n5` |

## Files Created / Modified

### Created

- `src/planner/graph_prune.rs` (384 lines, 12 tests)
- `src/planner/mcts.rs` (744 lines, 15 tests)
- `benches/bench_planner.rs` (264 lines, 4 benchmark groups)

### Modified

- `Cargo.toml` — added `rand = "0.9"` dep + `[[bench]]` entry for `bench_planner`.
- `src/planner/mod.rs` — added module declarations, re-exports, `use crate::error::Result;`, the `order_joins` dispatcher function (with rustdoc example), 5 new tests, and updated the module-level docstring.

## Design Notes

### Why MCTS over IDP?

The task description mentioned IDP (Iterative Dynamic Programming) as the
fallback for `n > 15`. IDP runs DPccp on a subset of relations, picks the
best partial plan, then iteratively expands the subset. It is a good choice
but has two downsides vs MCTS:

1. **No anytime behavior**: IDP needs to complete each DPccp iteration
   before it can return a plan. MCTS can stop at any iteration and return
   the best plan so far.
2. **No principled exploration**: IDP's subset choice is heuristic. MCTS's
   UCT provides a principled exploration-exploitation trade-off.

MCTS is also a better fit for future extensions (e.g., learning a policy
network to guide rollouts — the standard AlphaZero-style architecture).

### Why track full-plan cost (not remaining cost) in `MctsNode.total_cost`?

Two options:

- **Option A**: `total_cost` = full plan cost from root to leaf through this
  node. `avg_cost = total_cost / visits` is directly comparable across
  nodes at different depths.
- **Option B**: `total_cost` = remaining cost from this node to leaf.
  Requires tracking the cost-so-far separately during backprop.

Option A is simpler (one value, no separate cost-so-far tracking) and
makes UCT comparisons straightforward. The trade-off is that the same
rollout cost is added to every node on the path (not node-specific), which
is fine because UCT compares children of the same parent.

### Why a fixed seed?

Determinism is important for:
- **Plan-cache stability**: the same query should produce the same plan,
  so the plan cache hits.
- **Reproducible tests**: the "MCTS within 2× of DPccp" test must reliably
  find a good plan.
- **Debugging**: a user reporting a bad plan can share the exact input and
  we can reproduce the search tree.

The `with_seed` builder allows callers to override (e.g., for fuzz testing).

### Why does `MctsNode` have a `last_relation` field if it's redundant with
### the `(usize, MctsNode)` tuple in `children`?

The task description specifies the field. It's used in `select_expand_simulate`
to read the relation index from the child node (rather than from the
parent's `children` tuple), which makes the field meaningfully used and
silences the dead-code warning. The field is also useful for future
extensions (e.g., debugging tools that print the path to a node).

## Future Work

- **Bushy plans**: the current MCTS produces left-deep trees only (matching
  DPccp). Bushy plans (joining two non-singleton subtrees) can be
  significantly better for some queries, but require a different MCTS state
  representation (a forest of partial trees, not a single sequence).
- **Learned rollouts**: replace the uniform-random simulation with a
  lightweight policy (e.g., pick the smallest adjacent relation first).
  This is the standard "MCTS with heuristic rollouts" pattern and typically
  improves plan quality by 2-5×.
- **Lower-bound pruning in selection**: currently `GraphPruner::lower_bound`
  is implemented but not used in the MCTS loop (it's there for future
  alpha-beta-style pruning). Wiring it in would require tracking
  `cost_so_far + lower_bound(covered)` and pruning rollouts whose
  lower-bound already exceeds the incumbent.
- **Parallel MCTS**: the search is currently single-threaded. Tree
  parallelization (running multiple MCTS iterations concurrently on
  different parts of the tree) would give a near-linear speedup on
  multi-core machines.
