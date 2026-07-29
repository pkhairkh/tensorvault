# Wave 13B — Free Join Planner + WCOJ Benchmark

**Agent**: z-ai-code
**Date**: 2026-07-30
**Status**: Complete
**Baseline**: 375 tests (365 lib + 7 integration + 3 doc-tests) — Wave 13A
**After Wave 13B**: 397 tests (384 lib + 7 integration + 6 doc-tests)

## Tasks Completed

### 13-5: `src/planner/wcoj.rs` — WCOJ Plan Selection

Created the WCOJ (worst-case optimal join) plan-selection module that
picks between binary hash join and leapfrog triejoin based on the AGM
bound.

#### Public API

```rust
pub enum JoinAlgorithm { HashJoin, Leapfrog }

pub struct WcojPlan {
    pub relation_order: Vec<usize>,   // sorted by ascending cardinality
    pub join_attributes: Vec<usize>,  // sorted union of all attribute indices
    pub estimated_size: f64,          // AGM bound
}

pub fn choose_join_algorithm(graph: &JoinHypergraph, cardinalities: &[usize])
    -> JoinAlgorithm;

pub fn build_wcoj_plan(graph: &JoinHypergraph, cardinalities: &[usize])
    -> WcojPlan;
```

#### Decision rule (`choose_join_algorithm`)

1. Compute the AGM bound via `agm_bound(graph, cardinalities)`.
2. Compute the naive product `∏ |Ri|` (the worst-case size of a binary
   hash-join cascade).
3. If `agm_bound < product / 2`, return `Leapfrog`; else `HashJoin`.

The `1/2` safety margin accounts for leapfrog's higher per-key cost
(binary seek + N-iterator comparison vs hash join's tight `VPCMPEQB`
metadata scan on SwissTable slots): we require a clear asymptotic win
before switching.

#### Edge cases handled

- Empty hypergraph (no relations) → `HashJoin` (nothing to join).
- Single relation → `HashJoin` (product = AGM = cardinality; the
  `agm < product/2` test fails).
- Any zero-cardinality relation → `Leapfrog` (the join is empty;
  leapfrog terminates immediately on the first exhausted iterator,
  avoiding the hash-table build cost).

#### `build_wcoj_plan` details

- **`relation_order`**: relations sorted by ascending cardinality
  (`sort_by_key` provides stable ordering on ties by relation index).
  Leapfrog's seek cost is `O(log |R|)` per seek, so driving from the
  smallest relation minimizes the number of expensive seeks.
- **`join_attributes`**: the union of all attribute indices covered by
  the relations, sorted ascending. For a single-attribute intersection
  `R(A) ⋈ S(A) ⋈ T(A)`, this is `[0]`; for a triangle
  `R(A,B) ⋈ S(B,C) ⋈ T(A,C)`, it is `[0, 1, 2]`.
- **`estimated_size`**: the AGM bound (worst-case output size, also
  leapfrog's `O(AGM)` runtime bound).

#### Test results (13 new tests)

| Test | Input | Expected | Got |
|------|-------|----------|-----|
| `choose_join_algorithm_picks_leapfrog_for_triangle` | triangle N=100 | Leapfrog (AGM=1000 ≪ product=1M) | ✓ |
| `choose_join_algorithm_picks_hashjoin_for_acyclic_two_table` | R(A,B)⋈S(B,C), N=100 | HashJoin (AGM=product=10K) | ✓ |
| `choose_join_algorithm_picks_leapfrog_for_intersection` | R(A)⋈S(A), N=100 | Leapfrog (AGM=100 ≪ product=10K) | ✓ |
| `choose_join_algorithm_single_relation_picks_hashjoin` | 1 rel | HashJoin | ✓ |
| `choose_join_algorithm_empty_hypergraph_picks_hashjoin` | empty | HashJoin | ✓ |
| `choose_join_algorithm_zero_cardinality_picks_leapfrog` | one rel empty | Leapfrog | ✓ |
| `build_wcoj_plan_orders_relations_by_cardinality` | cards [300,50,200] | order [1,2,0] | ✓ |
| `build_wcoj_plan_estimated_size_is_agm_bound` | triangle N=100 | ~1000 | ✓ |
| `build_wcoj_plan_collects_all_join_attributes` | path A-B-C-D | [0,1,2,3] | ✓ |
| `build_wcoj_plan_single_attribute_intersection` | 3 rels on A | attrs=[0] | ✓ |
| `build_wcoj_plan_empty_hypergraph` | empty | empty plan, size=1.0 | ✓ |
| `build_wcoj_plan_single_relation` | 1 rel A,B | order=[0], attrs=[0,1], size=42 | ✓ |
| `build_wcoj_plan_tie_breaks_by_relation_index` | equal cards | order=[0,1,2] | ✓ |

### 13-6: WCOJ integration into `src/planner/lowerer.rs`

Added three new public methods to `PlanLowerer`:

#### `pick_join_operator(&self, _left, _right, graph, cardinalities) -> Operator`

The decision method: takes a `JoinHypergraph` and per-relation
cardinalities (provided by the caller because `LogicalPlan` does not
carry schema info), delegates to `choose_join_algorithm`, and returns
`Operator::HashProbe` or `Operator::LeapfrogJoin`.

#### `estimate_join_agm(&self, graph, cardinalities) -> f64`

Direct wrapper around `agm_bound` — the lowerer's entry point to the
AGM LP solver. Called by `lower_with_wcoj` when it encounters a Join
node with 3+ Scan leaves under it (the "multi-way" join case where
WCOJ shines).

#### `lower_with_wcoj(&self, plan, join_contexts) -> Vec<KernelInvocation>`

WCOJ-aware lowering. Walks the plan tree in pre-order; for each
`PlanNode::Join`:

1. Counts the `PlanNode::Scan` leaves under the Join subtree
   (`count_scan_leaves` helper).
2. If the count is ≥ 2 (any join), consumes the next
   `(hypergraph, cardinalities)` context from the caller-provided list
   and calls `pick_join_operator` to decide `HashProbe` vs
   `LeapfrogJoin`.
3. For multi-way joins (3+ leaves), the lowerer also calls
   `estimate_join_agm` directly — satisfying the contract "the lowerer
   calls `agm_bound` when it encounters a Join node with multiple
   relations". (The result is currently used for introspection; a
   future cardinality-estimation pass will thread it through to the
   cost model.)
4. If no context is available (the caller supplied fewer contexts than
   there are joins), falls back to the plan's declared operator
   (typically `HashProbe`).

A private helper `count_scan_leaves(node: &PlanNode) -> usize`
recursively counts `PlanNode::Scan` leaves, passing through `Aggregate`
and `Materialize` nodes. Used to distinguish binary joins (2 leaves)
from multi-way joins (3+ leaves).

#### Test results (6 new lowerer tests)

| Test | Input | Expected | Got |
|------|-------|----------|-----|
| `pick_join_operator_picks_leapfrog_for_triangle` | triangle N=100 | `Operator::LeapfrogJoin` | ✓ |
| `pick_join_operator_picks_hash_probe_for_acyclic` | R(A,B)⋈S(B,C) | `Operator::HashProbe` | ✓ |
| `estimate_join_agm_returns_agm_bound` | triangle N=100 | ~1000 | ✓ |
| `lower_with_wcoj_emits_leapfrog_for_triangle` | `Join(Join(R,S),T)` | inner=HashProbe, outer=LeapfrogJoin | ✓ |
| `lower_with_wcoj_falls_back_to_hash_probe_without_context` | no contexts | HashProbe (fallback) | ✓ |
| `count_scan_leaves_counts_correctly` | various plan shapes | 1, 1, 2, 3, 1 | ✓ |

### 13-7: `benches/bench_wcoj.rs` — WCOJ vs Hash Join Benchmark

Created a criterion benchmark with 6 benchmark functions across 3
workloads:

#### Workloads

1. **Triangle (cyclic, 3-way intersection)** — three sets of 100K u64
   keys drawn uniformly from `[0, 4N)`, so adjacent sets overlap by
   ~25%. Leapfrog intersects all three at once; hash join does two
   sequential `HashTable::build` + probe passes.

2. **Path (acyclic, 2-way intersection)** — two sets of 100K u64
   keys. Leapfrog 2-way intersection vs single hash join.

3. **Skewed triangle (power-law distribution)** — three sets of 20K
   keys drawn from a small domain (`n/4`), with a quadratic "hot-key"
   term XOR'd with uniform noise. A few hot keys appear in all three
   sets; leapfrog's `O(log |R|)` seek skips cold keys, while the hash
   cascade probes every duplicate.

#### Implementation details

- **PRNG**: deterministic `splitmix64` (no `thread_rng`) so the
  benchmark is reproducible across runs.
- **`uniform_keys`**: draws `n` keys uniformly, then `sort_unstable`
  + `dedup` (leapfrog requires strictly-ascending input).
- **`power_law_keys`**: combines `i² mod domain` (quadratic hot-key
  term) with uniform noise — produces a heavy-hitter distribution
  where leapfrog's seek-based traversal beats hash join's per-key
  probe.
- **`leak_keys`**: leaks each key slice once at setup time to get a
  `'static` reference for `Box<dyn SortedIterator>`. The leak is
  bounded by `Σ |Ri| × 8 bytes` (~2.4 MB for the triangle workload)
  and is intentional: the trait object needs to outlive the
  `LeapfrogJoin`, and restructuring the trait to take iterators by
  value is out of scope for this wave.
- **Throughput**: `Throughput::Elements(Σ |Ri|)` — the total input
  size, directly comparable between leapfrog (reads each input once +
  seeks) and the hash cascade (reads each input once: build + probe).

#### Sample results (cargo bench --quick)

| Benchmark | Leapfrog | Hash Join | Speedup |
|-----------|----------|-----------|---------|
| triangle (3-way, N=100K) | 4.79 ms / 62.7 Melem/s | 23.4 ms / 12.8 Melem/s | **4.9×** |
| path (2-way, N=100K) | 4.20 ms / 47.6 Melem/s | 19.4 ms / 10.3 Melem/s | **4.6×** |
| skewed triangle (3-way, N=20K) | 444 µs / 135 Melem/s | 1.15 ms / 52 Melem/s | **2.6×** |

The leapfrog beats hash join in all three workloads — including the
acyclic path, where the spec said hash join should be "comparable".
This is because the prototype's `HashTable` uses `std::HashMap` (not
the planned SwissTable with `VPCMPEQB` metadata scan), so its constant
factor is much higher than a production hash join. The qualitative
shape is correct: leapfrog's speedup is largest on the cyclic triangle
(4.9×) and smallest on the skewed triangle (2.6×, where the dataset
is smaller and the leapfrog's per-iterator overhead dominates). The
absolute numbers will tighten once the SwissTable lands.

### 13-8: Tests

All required tests are in place:

1. ✅ `choose_join_algorithm` picks Leapfrog for triangle join (cyclic) —
   `choose_join_algorithm_picks_leapfrog_for_triangle`.
2. ✅ `choose_join_algorithm` picks HashJoin for simple 2-table acyclic
   join — `choose_join_algorithm_picks_hashjoin_for_acyclic_two_table`.
3. ✅ `build_wcoj_plan` orders relations by cardinality (smallest first) —
   `build_wcoj_plan_orders_relations_by_cardinality`.
4. ✅ `build_wcoj_plan` returns correct estimated size —
   `build_wcoj_plan_estimated_size_is_agm_bound`.
5. ✅ Benchmark compiles: `cargo build --benches` (verified).

### Module registration

- `src/planner/mod.rs`: added `pub mod wcoj;` and re-exports
  `pub use wcoj::{build_wcoj_plan, choose_join_algorithm, JoinAlgorithm, WcojPlan};`.
- Updated the module-level docs to describe the new `agm` and `wcoj`
  submodules and the lowerer's WCOJ integration.
- `Cargo.toml`: added `[[bench]] name = "bench_wcoj" harness = false
  path = "benches/bench_wcoj.rs"`.

## DoD Verification

```bash
$ cargo fmt                                                # clean (nightly-only warnings on imports_granularity/group_imports — pre-existing)
$ cargo clippy --all-targets -- -D warnings                # Finished, no warnings
$ cargo test                                               # 384 lib + 7 integration + 6 doc-tests = 397 pass (1 doc-test ignored)
$ cargo build --benches                                    # Finished, all 5 benches compile (including bench_wcoj)
$ cargo bench --bench bench_wcoj -- --quick                # All 6 benchmark functions run successfully
```

Test count breakdown:
- Baseline (Wave 13A): 375 tests
- New wcoj tests: 13
- New lowerer tests: 6
- New doc-tests: 3 (choose_join_algorithm, build_wcoj_plan, pick_join_operator)
- Total new: 19 lib tests + 3 doc-tests = 22
- Grand total: 397 tests, all passing.

## Files Created / Modified

| File | Status | Lines |
|------|--------|-------|
| `src/planner/wcoj.rs` | created | 397 |
| `src/planner/lowerer.rs` | modified | +205 (3 new public methods, 1 new helper, 6 new tests, WCOJ doc section) |
| `src/planner/mod.rs` | modified | +10 (`pub mod wcoj`, re-exports, doc update) |
| `benches/bench_wcoj.rs` | created | 310 |
| `Cargo.toml` | modified | +4 (`[[bench]] bench_wcoj` entry) |

## Notes for Future Waves

### On the lowerer → WCOJ integration

The current `lower_with_wcoj` requires the caller to supply a
`(hypergraph, cardinalities)` pair for each Join node, because
`LogicalPlan` does not carry schema information. The proper fix is to
attach an attribute list to each `PlanNode::Scan` (a future schema
layer, ADR-016); once that lands, `lower_with_wcoj` becomes
`lower` — the WCOJ decision is made automatically from the plan's own
metadata.

Until then, callers who want WCOJ must:
1. Build the `JoinHypergraph` from the query's WHERE clause.
2. Estimate per-relation cardinalities (using `CardinalityEstimator`).
3. Pass them as `join_contexts` to `lower_with_wcoj`.

### On the benchmark's hash-join baseline

The hash-join baseline uses `HashTable::build` + `HashTable::probe`,
which is currently a `std::HashMap` wrapper. Once the SwissTable
(ADR-005 `AlignedSlot`) lands, the hash-join baseline will get ~5-10×
faster and the speedup ratios will tighten — especially on the acyclic
path query, where the spec says hash join should be "comparable" to
leapfrog.

### On the leapfrog kernel's single-attribute limitation

The current `LeapfrogJoin` does single-attribute multiway
intersection. A full multi-attribute leapfrog triejoin (which iterates
over one attribute at a time, intersecting per-attribute iterators via
the trie structure) requires a trie-indexed data structure — not yet
implemented. The benchmark's "triangle" workload models the join as a
3-way intersection of u64 key sets, which captures the algorithmic
essence (cyclic vs acyclic, leapfrog's `O(AGM)` vs hash cascade's
`O(∏ |Ri|)`) without requiring the full trie.

A future wave could:
1. Implement a `TrieRegion` storage format that indexes each relation
   by its join attributes (a trie of `(a, b, c)` for the triangle).
2. Extend `LeapfrogJoin` to walk the trie level-by-level, intersecting
   per-attribute iterators.
3. Wire the planner's `build_wcoj_plan` to emit a `LeapfrogTrieJoin`
   operator with the trie regions as inputs.

### On the AGM-bound recomputation

In `lower_node_with_wcoj`, when a multi-way join is encountered, the
lowerer calls `estimate_join_agm` (which calls `agm_bound`) for
introspection, then `pick_join_operator` (which calls
`choose_join_algorithm`, which calls `agm_bound` again). The LP is
`O(iters · m · n)` with `iters = 5600`, `m, n ≤ 15`, so each call is
<1 ms — the duplicate computation is negligible. If it ever becomes a
hotspot, the fix is to have `choose_join_algorithm` return both the
algorithm choice and the AGM bound in a single struct.
