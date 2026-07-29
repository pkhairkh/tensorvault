# Wave 6: Morsel-Driven Executor (ADR-018) — Work Record

**Task ID:** wave-6
**Agent:** Z.ai Code (single-agent execution)
**Status:** ✅ Complete
**Date:** 2026-07-30

## Summary

Implemented Wave 6 of the turboGP database engine: the morsel-driven
executor per ADR-018. The wave introduces four new modules under
`src/executor/`:

- `morsel.rs` — the `Morsel` struct (1024-cell batch, ADR-007) and the
  `MORSEL_SIZE` constant.
- `worker.rs` — the `WorkerThread` struct, NUMA-pinned per ADR-008, with a
  `process_morsel` method that runs a kernel on a morsel's data.
- `pipeline.rs` — the `Pipeline` struct (a fixed sequence of `Operator`s
  executed per-morsel) and the `PipelineBreaker` struct (forces
  materialization to DRAM for non-pipelineable operators like hash-join
  build).
- `dispatcher.rs` — the `MorselDispatcher`, which assigns morsels to
  workers in round-robin order.

The existing `Scheduler` (Wave 3, sequential kernel dispatch) and `plan`
modules are untouched. The two execution paths coexist: `Scheduler` is the
v1 sequential path (still used by the integration tests), while the
morsel-driven types are the v2 path that downstream waves will wire into
the query planner.

All DoD gates pass: `cargo fmt --check` (clean), `cargo clippy --all-targets
-- -D warnings` (clean), `cargo test` (180 passed = 153 baseline + 27 new,
debug and release modes both green).

## Files Created / Modified

| File | Change |
|------|--------|
| `src/executor/morsel.rs` | **New file.** `pub const MORSEL_SIZE: usize = 1024` (ADR-007 / ADR-018). `pub struct Morsel { data: Vec<u64>, region_id: u64, offset: usize, len: usize, numa_node: Option<u32> }` with `#[derive(Debug, Clone, Default)]`. Methods: `new(region_id, offset, cells: &[u64]) -> Self` (copies up to `MORSEL_SIZE` cells, sets `numa_node = Some(0)` as a heuristic placeholder); `as_slice(&self) -> &[u64]` (returns `&self.data` — the invariant `data.len() == len` is documented); `len(&self) -> usize`; `is_empty(&self) -> bool`. Doc-comment explains why 1024 cells (8 KB, fits in L1, power-of-two, >100 ns dispatch amortization). 6 unit tests. |
| `src/executor/worker.rs` | **New file.** `pub struct WorkerThread { cpu_id: u32, numa_node: u32 }` with `#[derive(Debug, Clone, Copy)]`. Methods: `new(cpu_id, numa_node) -> Self`; `pin(&self) -> Result<()>` (delegates to `crate::memory::numa::pin_thread_to_cpu(self.cpu_id)` — Linux path calls `sched_setaffinity(2)`, non-Linux is a no-op); `process_morsel(&self, morsel: &Morsel, kernel: &dyn Kernel, params: &KernelParams) -> KernelResult` (copies `*params`, overrides `cell_count = morsel.len()`, allocates a 64-byte stack output buffer, calls `unsafe { kernel.execute(...) }` with a `// SAFETY:` comment covering the input pointer validity, output buffer size, and CPU feature flag check). Doc-comment explains why pinning matters (eliminates cross-NUMA migrations, preserves L1/L2 state across morsels) and notes that the worker does NOT spawn an OS thread — that's the dispatcher's job. 5 unit tests. |
| `src/executor/pipeline.rs` | **New file.** Two public types: **`Pipeline`** `{ stages: Vec<Operator>, results: Vec<KernelResult> }` with methods `new(stages) -> Self`, `execute_morsel(&mut self, morsel, &KernelTable, &KernelParams) -> Result<()>` (per stage: look up kernel via `kernel_table.select(op, MemoryTier::L3)`, override `cell_count = morsel.len()`, execute on `morsel.as_slice().as_ptr()`, push result), `results(&self) -> &[KernelResult]`, `stage_count(&self) -> usize`, `reset(&mut self)`. Custom `Debug` impl that shows `stages` + `result_count` (avoids dumping long result vectors). **`PipelineBreaker`** `{ materialized: Vec<u64> }` with `#[derive(Debug, Default)]` and methods `new() -> Self`, `push(&mut self, &[u64])` (extends via `extend_from_slice`), `drain(&mut self) -> Vec<u64>` (uses `mem::take` for zero-copy ownership transfer), `len(&self) -> usize`, `is_empty(&self) -> bool` (added to satisfy clippy's `len_without_is_empty` lint). 8 unit tests. |
| `src/executor/dispatcher.rs` | **New file.** `pub const DEFAULT_CORES_PER_NUMA_NODE: u32 = 8` (heuristic placeholder for worker→NUMA-node mapping). `pub struct MorselDispatcher { workers: Vec<WorkerThread>, next_worker: usize }`. Methods: `new(num_workers: usize) -> Self` (creates workers `0..num_workers`, each pinned to CPU `i` on NUMA node `i / DEFAULT_CORES_PER_NUMA_NODE`); `dispatch(&mut self, morsel: Morsel) -> usize` (round-robin: `next_worker % workers.len()`, advances `next_worker = (next_worker + 1) % workers.len()`, returns chosen index; panics if `workers.is_empty()` with a descriptive message); `worker_count(&self) -> usize`; `worker(&self, idx) -> Option<&WorkerThread>`. Doc-comment explains why round-robin (no contention, cache-friendly stride) and notes the dispatcher does NOT spawn OS threads — it's the assignment layer. 8 unit tests (including a `should_panic` for the 0-workers case). |
| `src/executor/mod.rs` | Added `pub mod dispatcher;`, `pub mod morsel;`, `pub mod pipeline;`, `pub mod worker;` (alphabetical order between existing `plan` and `scheduler`). Added `pub use dispatcher::MorselDispatcher;`, `pub use morsel::{Morsel, MORSEL_SIZE};`, `pub use pipeline::{Pipeline, PipelineBreaker};`, `pub use worker::WorkerThread;`. Expanded the module doc-comment with a `## Wave 6: morsel-driven execution (ADR-018)` section explaining that the two execution paths (sequential `Scheduler` + morsel-driven) coexist and that downstream waves will wire the morsel path into the planner. |

## Design Decisions

### Task 6-1: Morsel struct

**`data: Vec<u64>` owns its cells (not a borrow).** The spec said "Pointer
to the cell data (1024 × 8 bytes = 8 KB)" with type `Vec<u64>`. An
alternative would be a `Box<[u64; 1024]>` (fixed-size, stack-friendly) or a
`&[u64]` borrow (zero-copy). I chose owning `Vec<u64>` because: (1) it
matches the spec's declared type, (2) owning the data lets the morsel
outlive the source region's lock — the dispatcher can hand the morsel to a
worker queue without holding the region's `Mutex<RegionBacking>` across
the dispatch, (3) `Vec` is the natural Rust type for "a buffer of N cells
where N may be < 1024".

**`data.len() == len` invariant.** The struct has both a `data: Vec<u64>`
field and a `len: usize` field, which looks redundant. The `len` field is
preserved because: (1) the spec explicitly lists it as a public field with
the semantics "may be < 1024 for the last morsel", (2) a future version
may pre-allocate a full 1024-cell buffer and use `len` to mark the valid
prefix (avoiding per-morsel allocation), in which case only `as_slice`
would need to change. The current implementation keeps `data.len() == len`
so `as_slice` is just `&self.data`; the doc-comment documents this as an
implementation detail.

**`new` copies up to 1024 cells.** `cells[..cells.len().min(MORSEL_SIZE)]
.to_vec()`. The caller is responsible for slicing the next morsel from
`offset + 1024` — this matches the spec ("creates a morsel from a slice
(copies up to 1024 cells)") and avoids surprising the caller with implicit
chunking.

**`numa_node` heuristic.** `new` sets `numa_node = Some(0)` as a
placeholder. A real implementation would consult `NumaTopology::detect()`
to look up the NUMA node of the source region. Since the dispatcher does
not yet route by NUMA node (round-robin only), the placeholder has no
behavioral impact. The `get_current_cpu()` vDSO call is touched (and its
return value discarded) so the heuristic is at least honest about being
CPU-aware — a future version can replace `Some(0)` with the actual
NUMA-node-of-current-CPU lookup.

**`#[derive(Default)]` instead of a manual `impl Default`.** Clippy's
`derivable_impls` lint flagged the manual `impl Default for Morsel`. All
fields have natural defaults (`Vec::new()`, `0`, `0`, `0`, `None`), so the
derive is equivalent and idiomatic.

### Task 6-2: WorkerThread

**`pin` delegates to `crate::memory::numa::pin_thread_to_cpu`.** This
reuses the Linux `sched_setaffinity(2)` implementation from Wave 5
(`src/memory/numa.rs`). No FFI duplication, no cfg-gating here — the cfg
lives in `numa.rs`. The worker just forwards `self.cpu_id`.

**`process_morsel` takes `&dyn Kernel`, not `&KernelTable`.** The spec
signature is `process_morsel(&self, morsel: &Morsel, kernel: &dyn Kernel,
params: &KernelParams) -> KernelResult`. The caller (typically the
pipeline or a test) is responsible for selecting the kernel from the
table — this keeps the worker focused on "run this kernel on this morsel"
and makes it testable without a full `KernelTable`. The
`WorkerThread::process_morsel_runs_scan_eq` test exercises the path by
selecting the kernel via `KernelTable::select` then passing it to the
worker.

**64-byte stack output buffer.** `let mut output = [0u8; 64]`. The
`KernelResult` struct is 32 bytes (8 `count` + 8 `sum` + 8 `mask` + 8
padding), but the kernel trait's `execute` signature takes `*mut u8` with
no length parameter — the kernel writes `size_of::<KernelResult>()` bytes.
64 bytes gives alignment headroom and matches the pattern in
`Scheduler::execute_invocation` (which also uses `[0u8; 64]`). Stack-
allocated, so no per-morsel heap allocation.

**`// SAFETY:` comment covers the three preconditions.** (1) input pointer
validity (`morsel.as_slice().as_ptr()` is valid for `morsel.len() * 8`
bytes because the Vec owns exactly that many), (2) output buffer size
(64-byte stack array, more than `size_of::<KernelResult>()`), (3) CPU
feature flags (the kernel was selected from the `KernelTable`, which only
registers kernels whose CPU features are present per ADR-003).

**`WorkerThread` is `Copy`.** Added `#[derive(Debug, Clone, Copy)]` —
`WorkerThread` is just two `u32`s, so `Copy` is appropriate. The
`worker_clone_copy_preserves_fields` test verifies this.

### Task 6-3: Pipeline

**`stages: Vec<Operator>`, not `Vec<dyn Kernel>`.** The spec declares the
field as `Vec<Operator>`. This means the pipeline stores the *operator
enum* and looks up the kernel per-stage in `execute_morsel` via the
`KernelTable`. Two benefits: (1) the pipeline is `Clone`-able and
`Debug`-able without trait-object gymnastics, (2) the same pipeline can be
re-used across CPUs with different kernel selections (the kernel table
picks the best kernel for the running CPU at execution time, not at
pipeline-construction time). Trade-off: each `execute_morsel` call does a
hash-map lookup per stage — negligible cost (one `HashMap::get` per
stage per morsel, ~10 ns each).

**Each stage uses the same morsel as input.** The spec says "For now,
each stage uses the same input data (the morsel)". This is the *simple
pipeline* model — sufficient for kernels that produce scalar aggregates
(count, sum), where the caller reduces across morsels. A future version
that supports filter-then-aggregate (where the filter's output mask feeds
the aggregate's input) will need a richer stage representation; the
doc-comment flags this as deferred.

**`MemoryTier::L3` for kernel selection.** The morsel data is L1-resident
(after the first stage touches it), but the kernel table only registers
L3 / Ddr5 / Cxl tiers (no `L1L2` kernels exist — L1 is hardware-managed,
not software-selectable). `L3` is the closest match. The `KernelTable::select`
fallback chain (exact → scalar-for-tier → any-kernel-for-operator) means
this works even if no L3 kernel is registered for an operator — it falls
back to any available kernel.

**`results` layout is `(morsel_idx * stages.len() + stage_idx)`.** Each
`execute_morsel` call appends `stages.len()` results. Callers that want a
per-stage reduction across morsels iterate by stride `stages.len()`. The
`pipeline_accumulates_across_multiple_morsels` test demonstrates this
pattern (sum the `count` field across all results for a 1-stage pipeline).
The alternative — a `Vec<Vec<KernelResult>>` (outer per-morsel, inner
per-stage) — would be more readable but adds an allocation per morsel;
the flat layout is cache-friendlier for the common reduce-across-morsels
pattern.

**Custom `Debug` impl.** `Pipeline` has a `Vec<KernelResult>` field that
can grow long (one entry per stage per morsel). The custom `Debug` shows
`stages` and `result_count` instead of dumping the full result vector —
matches the pattern in `MemoryManager` (Wave 5) and `Region` (Wave 2).

### Task 6-5: PipelineBreaker

**`Vec<u64>` backing, not mmap.** A real implementation would back the
breaker with a NUMA-aware huge-page allocation (ADR-009) to avoid TLB
pressure on the probe side. The `Vec<u64>` is sufficient for v1 and keeps
the API simple — the breaker is a flat 8-byte-cell buffer that grows as
morsels push their cells. The doc-comment flags the huge-page upgrade as
future work.

**`drain` uses `mem::take`.** `std::mem::take(&mut self.materialized)`
returns the owned `Vec<u64>` and replaces `self.materialized` with
`Default::default()` (an empty `Vec`). This is zero-copy: the returned
`Vec` is the same allocation the breaker was holding. The caller now owns
that allocation and can iterate it without copying.

**`is_empty` added for clippy.** Clippy's `len_without_is_empty` lint
fires when a type has a `len` method but no `is_empty`. Adding
`is_empty(&self) -> bool { self.materialized.is_empty() }` satisfies the
lint and is a useful accessor in its own right (the
`pipeline_breaker_push_three_batches_drain_returns_all` test uses it to
verify the breaker is empty after `drain`).

### Task 6-4: MorselDispatcher

**Round-robin, not NUMA-aware (yet).** The spec says "round-robin". A
production dispatcher would route morsels to workers on the same NUMA
node as the data (ADR-008), but that requires the morsel's `numa_node`
field to be populated correctly (currently `Some(0)` placeholder) and the
worker's `numa_node` to come from `NumaTopology::detect()` rather than
the `DEFAULT_CORES_PER_NUMA_NODE` heuristic. The doc-comment flags
NUMA-aware routing as future work; the round-robin policy is the v1
baseline and is sufficient for correctness (just not optimal for
multi-socket systems).

**`dispatch` takes `Morsel` by value.** The signature is `dispatch(&mut
self, morsel: Morsel) -> usize`. Taking ownership models the data-flow
semantics of a real executor: the dispatcher hands the morsel off to the
worker, and the caller no longer holds it. The chosen worker index is
returned so the caller knows where the morsel went (useful for tests that
want to call `worker.process_morsel` directly after dispatch). The
morsel itself is currently dropped at the end of `dispatch` — a real
executor would `send` it to the chosen worker's morsel queue. The
doc-comment is explicit about this: "Does NOT spawn a thread, run the
morsel, or move the morsel into a queue".

**`next_worker` wraps with `% workers.len()`.** Both the read
(`next_worker % n`) and the increment (`(next_worker + 1) % n`) use the
modulo. This keeps `next_worker` bounded, so it never grows unboundedly
across a long run (avoids the theoretical overflow at `usize::MAX`
morsels — ~3.7 × 10^19, but still good hygiene). The
`dispatcher_round_robin_wraps_after_exactly_n_morsels` test verifies the
wrap.

**`dispatch` panics on 0 workers.** `assert!(n > 0, ...)`. The
alternative — returning `Option<usize>` or `Result` — would force every
caller to handle a case that indicates a programming error (constructing
a dispatcher with 0 workers is meaningless). The panic message is
descriptive (`"MorselDispatcher::dispatch on a dispatcher with 0
workers"`) so a misbehaving caller sees the cause immediately. The
`dispatcher_zero_workers_panics_on_dispatch` test uses `#[should_panic]`
to verify.

**`DEFAULT_CORES_PER_NUMA_NODE = 8` heuristic.** Modern x86 server CPUs
have 8–16 cores per NUMA node (Intel Sapphire Rapids: 8 cores/tile;
AMD Zen 4 Genoa: 12 cores/CCD; AMD Zen 5 Turin: 16 cores/CCD). 8 is a
reasonable default that maps worker indices 0..7 → NUMA 0, 8..15 → NUMA 1,
etc. The `dispatcher_assigns_numa_node_heuristic` test verifies this
mapping. A real implementation would call `NumaTopology::detect()` and
build a CPU → NUMA-node lookup table; the constant is a placeholder
flagged in the doc-comment.

### Task 6-6: Module registration

**Alphabetical ordering.** `dispatcher` < `morsel` < `plan` < `pipeline`
< `scheduler` < `worker`. The `pub use` statements are also alphabetical
by re-exported name. This matches the convention in `src/memory/mod.rs`
(Wave 5) and `src/lib.rs`.

**Expanded module doc-comment.** Added a `## Wave 6: morsel-driven
execution (ADR-018)` section explaining that the two execution paths
(sequential `Scheduler` + morsel-driven) coexist, and that downstream
waves will wire the morsel path into the query planner. This makes the
module self-documenting for callers reading `cargo doc`.

### Task 6-6: Tests

27 new unit tests across the four new modules.

#### `src/executor/morsel.rs` (6 tests)

1. `morsel_from_large_slice_is_capped_at_1024` — 2000 input cells →
   `len == 1024`, `data.len() == 1024`, `as_slice() == &cells[..1024]`.
   Verifies the off-by-one boundary (`as_slice()[1023] == 1023`).
2. `morsel_from_short_slice_is_tail_morsel` — 500 input cells →
   `len == 500`, `len < MORSEL_SIZE`, `as_slice() == &cells[..]`. Verifies
   the tail-morsel case.
3. `morsel_empty_slice_is_empty` — empty input → `len == 0`,
   `is_empty() == true`.
4. `morsel_size_is_exactly_1024_cells` — `MORSEL_SIZE == 1024` and
   `MORSEL_SIZE * 8 == 8 * 1024` (8 KB). Direct DoD verification.
5. `morsel_default_is_empty` — `Morsel::default()` is empty with
   `region_id == 0`.
6. `morsel_clone_preserves_data` — `Morsel::clone()` preserves `data`,
   `region_id`, `offset`, `len`.

#### `src/executor/worker.rs` (5 tests)

1. `worker_pin_does_not_crash` — `WorkerThread::new(0, 0).pin()` accepts
   either `Ok` or `Err` (DoD: "pin doesn't crash"; restricted containers
   may return `EPERM`).
2. `worker_process_morsel_runs_scan_eq` — 10 cells with 4 sevens →
   `ScanEqU64` returns `count == 4`.
3. `worker_process_morsel_runs_sum_f64` — 5 f64 values `[1, 2, 3, 4, 5]`
   → `AggregateSumF64` returns `sum == 15.0`, `count == 5`.
4. `worker_default_is_cpu_zero_numa_zero` — `WorkerThread::default()` has
   `cpu_id == 0`, `numa_node == 0`.
5. `worker_clone_copy_preserves_fields` — `Copy` semantics: `let w2 = w1`
   preserves both fields.

#### `src/executor/pipeline.rs` (8 tests)

1. `pipeline_2_stage_produces_count_and_sum` — the **core DoD test**.
   2-stage pipeline `[ScanEqU64, AggregateSumF64]` on a morsel of
   `bits(1.0, 2.0, 1.0, 3.0, 1.0, 4.0)` with `target_u64 = bits(1.0)`.
   Stage 0 (ScanEq) → `count == 3`. Stage 1 (AggregateSum) →
   `sum == 12.0`, `count == 6`. Verifies both stages ran on the same
   morsel and produced correct independent results.
2. `pipeline_reset_clears_results` — after `reset()`, `results` is empty;
   re-running on a different morsel produces 1 result (pipeline structure
   intact).
3. `pipeline_accumulates_across_multiple_morsels` — 2 morsels, 1 stage
   (ScanEq) → 2 results. Caller reduces: `total = sum of counts == 4`.
   Demonstrates the flat-result-layout reduce pattern.
4. `pipeline_empty_stages_is_noop` — `Pipeline::new(vec![])` →
   `execute_morsel` is a no-op, `results` stays empty, `stage_count == 0`.
5. `pipeline_breaker_push_three_batches_drain_returns_all` — the **core
   DoD test** for PipelineBreaker. Push `[1,2,3]`, `[4,5]`, `[6,7,8,9]` →
   `len == 9`; `drain()` returns `[1,2,3,4,5,6,7,8,9]`; breaker is empty
   after drain.
6. `pipeline_breaker_drain_on_empty_returns_empty_vec` — `drain()` on a
   fresh breaker returns `vec![]` (no panic).
7. `pipeline_breaker_reusable_after_drain` — push, drain, push again,
   drain again → second drain returns only the second push (breaker is
   reusable).
8. `pipeline_debug_format_works` — `format!("{pipeline:?}")` contains
   `"Pipeline"`, `"stages"`, `"result_count"`.

#### `src/executor/dispatcher.rs` (8 tests)

1. `dispatcher_round_robin_three_workers_five_morsels` — the **core DoD
   test**. 3 workers, 5 morsels → assignments `[0, 1, 2, 0, 1]`.
2. `dispatcher_single_worker_assigns_all_to_zero` — 1 worker, 5 morsels
   → all assigned to worker 0.
3. `dispatcher_worker_count_matches_construction` — `new(N).worker_count()
   == N` for N ∈ {1, 4, 16}.
4. `dispatcher_assigns_numa_node_heuristic` — 16 workers → workers 0..7
   on NUMA 0, workers 8..15 on NUMA 1 (per `DEFAULT_CORES_PER_NUMA_NODE
   = 8`).
5. `dispatcher_worker_indexing_returns_none_out_of_range` —
   `worker(3)` on a 3-worker dispatcher returns `None`.
6. `dispatcher_default_has_one_worker` —
   `MorselDispatcher::default().worker_count() == 1`.
7. `dispatcher_zero_workers_panics_on_dispatch` —
   `#[should_panic]` test for the 0-workers case.
8. `dispatcher_round_robin_wraps_after_exactly_n_morsels` — after N
   morsels (N = worker count), the (N+1)th morsel goes to worker 0
   (verifies the wrap-around).

## Test Results

```
cargo fmt --check:                              clean (exit 0)
cargo clippy --all-targets -- -D warnings:      clean (exit 0)
cargo test (debug):
  lib unit tests:     173 passed  (was 146, +27 new)
  integration tests:    7 passed  (unchanged)
  total:              180 passed  (was 153, +27 new)
cargo test --release:
  lib unit tests:     173 passed
  integration tests:    7 passed
  total:              180 passed
```

## DoD Verification

- [x] `cargo test` passes (153 existing + 27 new = 180 total)
- [x] `cargo clippy -- -D warnings` passes (also `--all-targets`)
- [x] `cargo fmt --check` passes
- [x] Multi-stage pipeline (scan → aggregate) produces correct results —
      validated by `pipeline_2_stage_produces_count_and_sum`:
      ScanEq → count=3, AggregateSum → sum=12.0, count=6
- [x] Morsel size is exactly 1024 cells — `MORSEL_SIZE == 1024`, verified
      by `morsel_size_is_exactly_1024_cells` and
      `morsel_from_large_slice_is_capped_at_1024`
- [x] `Morsel` struct has `data`, `region_id`, `offset`, `len`,
      `numa_node` fields per spec
- [x] `Morsel::new` copies up to 1024 cells from a slice
- [x] `Morsel::as_slice`, `len`, `is_empty` implemented per spec
- [x] `WorkerThread` struct has `cpu_id`, `numa_node` fields per spec
- [x] `WorkerThread::new`, `pin`, `process_morsel` implemented per spec
- [x] `WorkerThread::pin` calls
      `crate::memory::numa::pin_thread_to_cpu(self.cpu_id)`
- [x] `Pipeline` struct has `stages: Vec<Operator>`, `results:
      Vec<KernelResult>` per spec
- [x] `Pipeline::new`, `execute_morsel`, `results`, `reset` implemented
      per spec
- [x] `PipelineBreaker` struct has `materialized: Vec<u64>` per spec
- [x] `PipelineBreaker::new`, `push`, `drain`, `len` implemented per spec
- [x] `MorselDispatcher` struct has `workers: Vec<WorkerThread>`,
      `next_worker: usize` per spec
- [x] `MorselDispatcher::new`, `dispatch`, `worker_count` implemented
      per spec
- [x] Round-robin dispatch validated by
      `dispatcher_round_robin_three_workers_five_morsels`
- [x] All unsafe blocks have `// SAFETY:` comments (3 sites:
      `WorkerThread::process_morsel`, `Pipeline::execute_morsel`)
- [x] New modules registered in `src/executor/mod.rs`
- [x] `pub use` re-exports added for all new public types
      (`Morsel`, `MORSEL_SIZE`, `WorkerThread`, `Pipeline`,
      `PipelineBreaker`, `MorselDispatcher`)

## Notes for Downstream Waves

- **The morsel path is not wired into the query planner.** The existing
  `Scheduler::execute_plan` (Wave 3) still uses the sequential
  kernel-per-region model. A future wave should add a
  `Scheduler::execute_plan_morsel_driven` (or a parallel `MorselExecutor`
  struct) that: (1) splits each scan's region into 1024-cell morsels,
  (2) constructs a `Pipeline` from the plan's operators, (3) uses
  `MorselDispatcher` to assign morsels to `WorkerThread`s, (4) reduces
  the per-morsel `KernelResult`s into a final `PlanResult`. The
  infrastructure is all here; the wiring is the missing piece.

- **No OS threads are spawned.** Both `WorkerThread` and
  `MorselDispatcher` are bookkeeping types — they don't `std::thread::spawn`.
  A real executor needs a thread pool (one thread per worker, each
  calling `worker.pin()` inside its spawn closure, then looping on a
  morsel queue calling `worker.process_morsel` or
  `pipeline.execute_morsel`). The current API returns the chosen worker
  index from `dispatch` so a benchmark or test can drive the workers
  synchronously. Recommended next step: a `MorselExecutor` struct that
  owns a `crossbeam` or `flume` mpsc channel per worker, spawns the
  threads, and `send`s morsels to the chosen worker.

- **`Morsel::numa_node` is a placeholder.** `new` sets `numa_node =
  Some(0)` regardless of the source region's actual NUMA node. A real
  implementation should look up the region's `numa_node` field (set by
  `MemoryManager` when the region is placed) and copy it into the morsel.
  The dispatcher's NUMA-aware routing (currently round-robin only) would
  then consult `morsel.numa_node` to pick a worker on the same node.

- **`Pipeline` is a "simple pipeline" — each stage runs on the same
  morsel.** This is sufficient for kernels that produce scalar aggregates
  (count, sum). A future version that supports filter-then-aggregate
  (where the filter's output mask feeds the aggregate's input) needs a
  richer stage representation — likely `Vec<PipelineStage>` where
  `PipelineStage` is an enum `{ PassThrough(Operator), Filter(Operator,
  Predicate), Aggregate(Operator) }` and `execute_morsel` threads the
  morsel through each stage's output. The current `Vec<Operator>` field
  is preserved per spec; the upgrade is additive.

- **`PipelineBreaker` is `Vec<u64>`-backed, not mmap-backed.** A real
  implementation should back the breaker with a NUMA-aware huge-page
  allocation (ADR-009) to avoid TLB pressure on the probe side. The
  `RegionBacking` type from `src/memory/region.rs` already implements
  the mmap + `MAP_HUGETLB` path — a future wave could re-use it for the
  breaker's backing storage.

- **`DEFAULT_CORES_PER_NUMA_NODE = 8` is a heuristic.** A real
  implementation should call `NumaTopology::detect()` (Wave 5) and build
  a CPU → NUMA-node lookup table at dispatcher construction time. The
  current constant is a placeholder flagged in the doc-comment.

- **`Pipeline::execute_morsel` selects `MemoryTier::L3` for kernel
  lookup.** This is correct for the current kernel table (which only
  registers L3/Ddr5/Cxl tiers — L1 is hardware-managed). If a future
  wave adds L1L2-specific kernels (e.g. a hand-tuned AVX-512 kernel
  optimized for L1-resident data), the pipeline should select for
  `MemoryTier::L1L2` instead. The `KernelTable::select` fallback chain
  means the current code works either way, but the explicit `L3` is a
  lie about the data's actual tier.

- **The 64-byte output buffer in `WorkerThread::process_morsel` and
  `Pipeline::execute_morsel` is larger than `size_of::<KernelResult>()`
  (32 bytes).** This matches the pattern in `Scheduler::execute_invocation`
  and gives alignment headroom. If a future wave adds a kernel that
  writes more than 64 bytes to the output buffer (e.g. a hash-join probe
  that returns a list of matching row indices), the buffer size must
  increase. The `Kernel` trait's `execute` signature takes `*mut u8`
  with no length parameter — there is no runtime check that the kernel
  stays within the buffer. A future wave should either (1) add a
  `output_len: usize` parameter to `execute` and panic on overflow, or
  (2) switch to a typed `&mut KernelResult` output (which would require
  boxing for variable-size results).
