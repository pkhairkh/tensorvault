# Wave 13A — AGM Bound + Leapfrog Triejoin Kernel

**Agent**: z-ai-code
**Date**: 2026-07-30
**Status**: Complete
**Baseline**: 348 tests (340 lib + 7 integration + 1 doc-test)
**After Wave 13A**: 375 tests (365 lib + 7 integration + 3 doc-tests)

## Tasks Completed

### 13-1: `src/planner/agm.rs` — AGM Fractional Cover

Created the Atserias-Grohe-Marx (AGM) bound module, which computes the
worst-case size of a join result:

```text
|Join(R1,...,Rm)| ≤ ∏ |Ri|^fi
```

where `(f1,...,fm)` is the optimal fractional cover of the query
hypergraph — the solution to the LP:

```text
minimize  Σ fi · log(|Ri|)
subject to  Σ_{i ∋ a} fi ≥ 1   for each attribute a
            fi ≥ 0
```

#### Key design decisions

1. **`JoinHypergraph` struct** — stores per-relation attribute-index lists
   and attribute names. Includes a `from_named` helper that resolves
   string attribute names to indices (sorted, deduped).

2. **`agm_bound` public function** — takes a hypergraph + cardinalities,
   returns the bound as an `f64`. Handles edge cases: empty hypergraph
   → 1.0, zero-cardinality relation → 0.0 (empty input collapses the
   output).

3. **`solve_fractional_cover` internal function** — solves the LP using a
   **log-barrier interior-point method** with **path-following**:

   - Initialize at a strictly interior feasible point (`f_i = 2.0` for
     all `i`, so every coverage constraint is strictly satisfied).
   - For a sequence of decreasing barrier weights `μ ∈ {1.0, 0.3, 0.1,
     0.03, 0.01, 0.003, 0.001}`, run gradient descent on the penalized
     objective:
     `Σ fi · ci - μ · (Σa log(cov_a - 1) + Σi log(fi))`
   - Step size `η = 0.1 · μ` (proportional to μ — large steps for large
     μ to reach the interior fast, small steps for small μ to refine
     near the boundary).
   - 800 iterations per μ level, 7 levels = 5600 total iterations.
   - After convergence, normalize: scale all `fi` so that the minimum
     attribute coverage equals exactly 1 (project to exact feasibility).

4. **Why interior-point, not subgradient?** The initial implementation
   used a projected subgradient method (find the most-violated attribute,
   increase weights on its covering relations). This converged to *a*
   feasible point but not the *optimal* one — on the triangle query
   `R(A,B) ⋈ S(B,C) ⋈ T(A,C)` it gave bound ≈ 2424 instead of the correct
   1000 (the symmetric optimum `f = (0.5, 0.5, 0.5)` requires coordinated
   growth across all three relations). The log-barrier method correctly
   finds the symmetric optimum because the barrier keeps all variables
   strictly positive throughout the optimization, and the path-following
   gradually pushes them toward the boundary in a coordinated way.

#### Test results

| Test | Query | Expected | Got |
|------|-------|----------|-----|
| `agm_bound_two_relations_on_one_attr` | 2 rels on A, \|R\|=100 | 100 | ~100 ✓ |
| `agm_bound_two_relations_unequal_cardinality` | 2 rels on A, \|R1\|=10, \|R2\|=1000 | 10 (LP min: put weight on cheaper rel) | ~10 ✓ |
| `agm_bound_triangle_query` | R(A,B),S(B,C),T(A,C), N=100 | N^1.5 = 1000 | ~1000 ✓ |
| `agm_bound_triangle_query_large_n` | triangle, N=1000 | N^1.5 = 31623 | ~31623 ✓ |
| `agm_bound_path_query` | R(A,B),S(B,C),T(C,D), N=100 | N² = 10000 | ~10000 ✓ |
| `agm_bound_star_query` | R(A,B),S(A,C),T(A,D), N=100 | N³ = 1M | ~1M ✓ |
| `agm_bound_single_relation` | 1 rel on A, \|R\|=42 | 42 | ~42 ✓ |
| `agm_bound_empty_hypergraph` | no relations | 1.0 | 1.0 ✓ |
| `agm_bound_le_product_of_cardinalities` | triangle | ≤ \|R\|³ | ✓ |
| `agm_bound_ge_max_cardinality` | 2 rels on A | ≥ max \|R\| | ✓ |
| `solve_fractional_cover_feasibility` | triangle | every attr cov ≥ 1 | ✓ |
| `from_named_resolves_indices` | helper test | sorted indices | ✓ |

### 13-2: `src/kernel/leapfrog.rs` — Leapfrog Triejoin Kernel

Created the Leapfrog triejoin kernel (Veldhuizen 2014), a worst-case
optimal join algorithm that runs in `O(IN + OUT + AGM)` time.

#### Key design decisions

1. **`SortedIterator` trait** — abstracts over sorted key iterators with
   `current_key()`, `seek(key)`, `next()`. The `seek` method uses binary
   search (`partition_point`) for `O(log n)` seek.

2. **`SliceSortedIterator`** — a concrete implementation over `&[u64]`.
   Uses `partition_point` for fast seeking. Assumes the slice is
   pre-sorted (no runtime validation — leapfrog silently produces wrong
   results on unsorted input, same as a merge join).

3. **`LeapfrogJoin` struct** — the standalone join runner. Takes
   `Vec<Box<dyn SortedIterator>>` and produces the intersection (keys
   present in ALL iterators). The algorithm:
   - Initialize all iterators (seek to first key).
   - Find `max_key` across all current keys.
   - Seek every iterator to `≥ max_key`.
   - If all land on `max_key` → emit, advance one iterator.
   - If any lands past `max_key` → new max is larger, loop.
   - Stop on any exhausted iterator.

4. **`LeapfrogScalar` kernel** — a `Kernel` trait impl that wraps a
   **2-way** leapfrog intersection. Encodes two slices as a single
   contiguous buffer (since the `Kernel` trait takes one `input`
   pointer): first slice length in `params.cell_count`, second slice
   length in `params.target_u64`. Returns the count of matching keys
   in `KernelResult::count`. Multi-way joins use `LeapfrogJoin` directly
   (the kernel wrapper exists only for kernel-table introspection).

5. **Edge cases handled**:
   - Empty iterator list → `vec![]`
   - Any single empty iterator → `vec![]`
   - Single iterator → returns all keys
   - All disjoint → `vec![]`
   - Null pointer in the scalar kernel with `total == 0` → early return
     (avoids tripping `slice::from_raw_parts`'s debug precondition
     requiring a non-null pointer).

#### Test results

| Test | Input | Expected | Got |
|------|-------|----------|-----|
| `leapfrog_two_iterators_intersection` | [1,2,3,4,5] & [2,4,6] | [2,4] | [2,4] ✓ |
| `leapfrog_three_disjoint_iterators_empty` | [1,2,3],[10,20,30],[100,200,300] | [] | [] ✓ |
| `leapfrog_single_iterator_returns_all` | [5,10,15,20,25] | all | all ✓ |
| `leapfrog_empty_iterator_empty_result` | [1,2,3],[],[2,3,4] | [] | [] ✓ |
| `leapfrog_no_iterators_empty_result` | (none) | [] | [] ✓ |
| `leapfrog_three_iterators_partial_overlap` | [1,2,3,4,5],[2,3,4,5,6],[3,4,5,6,7] | [3,4,5] | [3,4,5] ✓ |
| `leapfrog_identical_iterators` | [1,5,10,15,20,100] ×3 | same | same ✓ |
| `leapfrog_matches_brute_force_random` | 20 random trials, 3 iters each | brute-force intersection | matches ✓ |
| `slice_iterator_seek` | [1,5,10,15,20,25] | seek(12)→15, seek(100)→None | ✓ |
| `slice_iterator_next` | [1,5,10] | 1→5→10→None | ✓ |
| `slice_iterator_empty` | [] | None throughout | ✓ |
| `leapfrog_scalar_kernel_counts_matches` | [1,2,3,4,5] & [2,4,6] | count=2 | 2 ✓ |
| `leapfrog_scalar_kernel_empty` | both empty | count=0 | 0 ✓ |

### 13-3: LeapfrogJoin in the kernel table

- Added `Operator::LeapfrogJoin` variant to the `Operator` enum in
  `src/kernel/mod.rs`, with documentation explaining it is a worst-case
  optimal multiway intersection.
- Registered a `LeapfrogScalar` kernel in a new `register_join_kernels`
  function, called from `KernelTable::new()` alongside the existing
  `register_scan_kernels`, `register_hash_kernels`,
  `register_aggregate_kernels`, `register_similarity_kernels`.
- Registered at `(Operator::LeapfrogJoin, CpuTarget::Scalar,
  MemoryTier::L3)`.
- The kernel table now has 9 operators (was 8).

### 13-4: Module registration

- `src/planner/mod.rs`: added `pub mod agm;` and re-exports
  `pub use agm::{agm_bound, JoinHypergraph};`.
- `src/kernel/mod.rs`: added `pub mod leapfrog;`.

## DoD Verification

```bash
$ cargo fmt                                                # clean
$ cargo clippy -- -D warnings                              # Finished, no warnings
$ cargo clippy --all-targets -- -D warnings                # Finished, no warnings
$ cargo test                                               # 365 lib + 7 integration + 3 doc-tests = 375 pass
```

Test count breakdown:
- Baseline: 348 tests
- New AGM tests: 12
- New Leapfrog tests: 13
- New doc-tests: 2 (JoinHypergraph example, agm_bound example)
- Total new: 27
- Grand total: 375 tests, all passing.

## Files Created / Modified

| File | Status | Lines |
|------|--------|-------|
| `src/planner/agm.rs` | created | 520 |
| `src/kernel/leapfrog.rs` | created | 562 |
| `src/planner/mod.rs` | modified | +3 (pub mod agm + re-exports) |
| `src/kernel/mod.rs` | modified | +14 (LeapfrogJoin enum variant + register_join_kernels) |

## Notes for Future Waves

### On the AGM solver

The interior-point barrier method was chosen after a simpler projected
subgradient method failed to converge to the symmetric optimum on the
triangle query. The subgradient method finds *a* feasible point (all
coverage constraints satisfied) but not the *minimum-objective* feasible
point, because it greedily increases weights on the most-violated
attribute's relations without coordinating across attributes.

The barrier method avoids this by keeping all variables strictly
positive throughout the optimization, so the gradient of the linear
cost term is always balanced against the gradient of the barrier terms.
The path-following schedule (μ from 1.0 down to 0.001) ensures the
solution tracks the central path of the LP polytope, converging to the
optimal vertex as μ → 0.

A future optimization could replace the gradient descent with Newton's
method (which converges in O(log(1/ε)) iterations instead of O(1/ε²)),
but for the small LPs the planner deals with (m, n ≤ 15), the current
5600-iteration gradient descent runs in <1 ms and is fast enough.

### On the Leapfrog kernel

The current `LeapfrogScalar` kernel is a 2-way intersection wrapper.
Real multiway joins (3+ relations) use `LeapfrogJoin` directly, which
takes `Vec<Box<dyn SortedIterator>>`. This is because the `Kernel`
trait takes a single `input: *const u8` pointer, and encoding N
variable-length slices into one buffer is awkward.

A future extension could:
1. Add a `LeapfrogMultiKernel` that reads a header describing N slices
   (offsets + lengths) from the input buffer, then runs the N-way
   leapfrog.
2. Integrate the planner to emit Leapfrog join nodes when the AGM bound
   is much smaller than the binary-join estimate (the planner currently
   only emits hash joins).
3. Add a sorted-run builder kernel (external sort) so unsorted inputs
   can be sorted in-place before the leapfrog join.

### On the leapfrog → AGM connection

The leapfrog triejoin is worst-case optimal: it runs in
`O(IN + OUT + AGM)` time. This means the AGM bound computed by
`solve_fractional_cover` is not just a theoretical curiosity — it is
the actual runtime bound of the leapfrog kernel. The planner can use
the AGM bound to decide between a hash join (binary, runs in
`O(|R| · |S|)`) and a leapfrog join (multiway, runs in `O(AGM)`) for
cyclic queries where the AGM bound is much smaller than any binary
decomposition.
