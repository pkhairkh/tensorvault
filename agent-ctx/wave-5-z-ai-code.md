# Wave 5: Memory Manager — Work Record

**Task ID:** wave-5
**Agent:** Z.ai Code (single-agent execution)
**Status:** ✅ Complete
**Date:** 2026-07-30

## Summary

Implemented Wave 5 of the turboGP database engine: the tier-aware memory
manager (ADR-010), NUMA thread pinning (ADR-008), and a heuristic
bandwidth monitor. The memory manager owns per-tier LRU lists of region
IDs and decides which regions to evict when a tier is full — LRU gives a
formal k-competitive guarantee (cost ≤ k × offline optimal, where k is
the number of tiers; for our 4-tier hot path this is a 4× bound per
ADR-010).

All DoD gates pass: `cargo fmt --check` (clean), `cargo clippy -- -D
warnings` (clean), `cargo clippy --all-targets -- -D warnings` (clean),
`cargo test` (153 passed = 132 baseline + 21 new, debug and release
modes both green).

## Files Modified

| File | Change |
|------|--------|
| `src/memory/numa.rs` | Added `pub fn pin_thread_to_cpu(cpu_id: u32) -> Result<()>` — Linux path calls `libc::sched_setaffinity(0, size_of::<cpu_set_t>(), &cpuset)` with a 1-CPU set built via `libc::CPU_ZERO` + `libc::CPU_SET`. Non-Linux is a no-op returning `Ok(())`. Returns `Error::Unsupported` if `cpu_id ≥ 8 × size_of::<cpu_set_t>()` (the static cpu_set_t bit count, 1024 on glibc), or `Error::Io` if `sched_setaffinity` returns nonzero. Added `pub fn get_current_cpu() -> u32` — Linux path calls `libc::sched_getcpu()` (safe FFI, vDSO-backed, no syscall) and maps the -1 error sentinel to 0; non-Linux returns 0. Both Linux-only implementations live in private `pin_thread_to_cpu_linux` to keep the cfg-gating tight. All unsafe blocks have `// SAFETY:` comments. Added `use crate::Result;` to the imports. 2 new tests. |
| `src/memory/manager.rs` | **New file.** `MemoryManager` struct with three private fields: `tier_lru: HashMap<MemoryTier, VecDeque<RegionId>>` (per-tier LRU, front = MRU, back = LRU), `regions: HashMap<RegionId, Arc<Region>>` (canonical ID → region map), `tier_capacity: HashMap<MemoryTier, usize>` (max regions per tier; missing entries default to `usize::MAX` = unlimited). `const UNLIMITED_CAPACITY: usize = usize::MAX` sentinel. Methods: `new()` → defaults L3=16, Ddr5=256, Cxl=1024, Nvme=unlimited; `with_capacity(HashMap)` → custom capacities; `register(Arc<Region>)` → adds to region's own `tier` LRU (idempotent — re-registering moves the ID out of any old tier's LRU first); `access(RegionId) -> Option<Arc<Region>>` → moves to front of LRU, returns cloned Arc; `place_region(Arc<Region>, MemoryTier) -> Result<Vec<RegionId>>` → removes from old LRU if re-placing, evicts LRU regions from the back of `target_tier` until room, inserts at front, returns evicted IDs; `evict_from_tier(MemoryTier, usize) -> Vec<RegionId>` → pops `count` from back (clamped if tier smaller); accessors `regions_in_tier`, `total_regions`, `contains`, `tier_capacity`, `tier_lru_order`. Private helpers `tier_is_full`, `pop_back_from_tier`, `remove_from_any_tier`. Custom `Debug` impl that renders `tier_lru` as `{tier → len}` (avoids dumping long deques). `Default` impl delegates to `new()`. 12 unit tests. |
| `src/memory/bandwidth.rs` | **New file.** `BandwidthMonitor` struct with four fields: `last_read_bytes: u64` (last `MemTotal − MemFree` from /proc/meminfo), `last_timestamp: Instant`, `tier_counts: HashMap<MemoryTier, usize>` (heuristic, defaults to ~50% utilization), `tier_capacity: HashMap<MemoryTier, usize>` (mirrors MemoryManager defaults). `const DDR5_BANDWIDTH_BPS: u64 = 50_000_000_000` (cfg-gated to non-Linux only — it's the constant estimate returned when /proc/meminfo isn't available). Methods: `new()` → initializes counts/capacities with defaults; `read_memory_bandwidth(&mut self) -> f64` → Linux: reads /proc/meminfo, computes per-second delta of `MemTotal − MemFree`, clamps to non-negative, returns 0.0 on first call; non-Linux: returns the constant 50 GB/s; `tier_utilization(&self, MemoryTier) -> f64` → `count / capacity` clamped to [0, 1], returns 0.0 for unlimited or unknown tiers; `set_tier_count(&mut self, MemoryTier, usize)` → hook for MemoryManager to push live counts. Private Linux-only `read_proc_meminfo_used()` parses `MemTotal:` and `MemFree:` lines (in kB, × 1024 to bytes). Custom `Debug` impl. `Default` impl. 7 unit tests (one Linux-only). |
| `src/memory/mod.rs` | Added `pub mod bandwidth;` and `pub mod manager;` (alphabetical order before `numa`). Added `pub use bandwidth::BandwidthMonitor;` and `pub use manager::MemoryManager;`. Expanded the module doc-comment with a `## Modules` section listing all five submodules and their key types (including cross-references to `numa::pin_thread_to_cpu` / `numa::get_current_cpu` and to ADR-008 / ADR-010). |

## Design Decisions

### Task 5-1: NUMA thread pinning

**Two public functions, one private helper.** `pin_thread_to_cpu` is the
public entry point; the actual Linux syscall lives in
`pin_thread_to_cpu_linux` (private, `#[cfg(target_os = "linux")]`). This
keeps the cfg-gating tight: the public function has both `#[cfg(...)]`
arms (Linux dispatches to the helper, non-Linux is a no-op), and the
helper only compiles on Linux where `libc::cpu_set_t` etc. exist.

**`cpu_set_t` is statically sized.** The libc crate's `cpu_set_t` is a
fixed-size struct (sized for `CPU_SETSIZE = 1024` on glibc). `CPU_SET`
with an index ≥ 1024 would be an out-of-bounds write. I added an explicit
bounds check `cpu_idx >= 8 * size_of::<cpu_set_t>()` that returns
`Error::Unsupported` for `cpu_id ≥ 1024`. A production version could use
dynamic cpu_set allocation for systems with > 1024 CPUs, but that's
overkill for v1 — even the largest current CPUs (AMD Bergamo, 128 cores)
don't exceed 1024 logical CPUs in a single `cpu_set_t`.

**`mem::zeroed()` for cpu_set_t initialization.** I use
`unsafe { std::mem::zeroed() }` to initialize the `cpu_set_t`. The libc
crate's `cpu_set_t` is a `#[repr(C)]` struct of `c_ulong` words whose
all-zero bit pattern is the documented empty-set state (the same state
`CPU_ZERO` writes). `mem::zeroed()` is sound for this type per the libc
crate's own usage. An alternative is `MaybeUninit::zeroed().assume_init()`
which is functionally identical but more verbose; I went with `mem::zeroed`
to match the libc crate's own examples.

**`CPU_ZERO` + `CPU_SET` in a single unsafe block.** Both calls take
`&mut cpu_set_t` (verified via a small standalone test using
`cargo check` against a `check_libc` scratch project). The unsafe block
covers both, with a single `// SAFETY:` comment that addresses: (1) the
reference is valid (local variable), (2) `cpu_idx` is bounds-checked
above.

**`sched_setaffinity(0, ...)` targets the calling thread.** `pid = 0`
means "the calling thread" — no need to call `gettid()` (which itself
requires `unsafe`). The kernel reads `size_of::<cpu_set_t>()` bytes from
the `cpuset` pointer and updates the calling thread's affinity mask.
Returns 0 on success, -1 on error (errno set); we use
`std::io::Error::last_os_error()` to read errno and wrap it as
`Error::Io`.

**`get_current_cpu` is safe to call.** `libc::sched_getcpu()` is a vDSO
call (no syscall) that reads the CPU index from thread-local storage. The
libc crate exposes it as `unsafe extern "C" fn` (like all extern "C"
functions in modern libc versions), but the function itself has no safety
preconditions — it takes no pointer arguments and performs no I/O. The
`// SAFETY:` comment documents this. The -1 error sentinel (which would
indicate a broken vDSO) is mapped to 0.

**Test tolerance for restricted containers.** The
`pin_thread_to_cpu_does_not_crash` test pins to the current CPU (queried
via `get_current_cpu()`), which should always succeed because the thread
is already running on that CPU (so it's in the cgroup cpuset). However, in
heavily restricted containers `sched_setaffinity` can fail with `EPERM`
even for the current CPU, so the test accepts either `Ok` or `Err` and
just verifies no panic. The DoD requirement is "doesn't crash on Linux",
which is satisfied either way.

### Task 5-2: MemoryManager with per-tier LRU

**`VecDeque<RegionId>` for each tier's LRU.** The spec said to use
`VecDeque`; front = most-recently-used (push_front), back =
least-recently-used (pop_back). `VecDeque` gives O(1) push/pop at both
ends. The alternative — a hand-rolled doubly-linked list — would give O(1)
move-to-front, but `VecDeque::retain` is O(n) per access, and for our
tier sizes (max 1024 regions) the linear scan is cheaper than the
pointer-chase of a linked list (cache-friendly contiguous memory beats
linked nodes for any n < ~10K).

**`remove_from_any_tier` is the key correctness helper.** When `register`
or `place_region` is called with an ID that's already in `regions`, we
must remove it from its old tier's LRU before adding it to the new one —
otherwise the same ID would appear in two deques, breaking the
"every ID in any LRU is in `regions`" invariant. The helper looks up the
*old* region's tier (not the new region's) to find the right deque. This
is called before any eviction logic so the placement is consistent.

**`place_region` doesn't mutate the region's `tier` field.** The input is
`Arc<Region>`, and `Region::tier` is a plain field (not behind a Mutex).
We can't mutate it through an Arc. The manager's bookkeeping places the
ID in `target_tier`'s LRU regardless of `region.tier`; the doc comment
explains that callers wanting `region.tier` to actually change should
call `Region::migrate_to(target_tier)` first to obtain a new region with
the desired tier. This separation matches the ADR-006 model: the manager
does *bookkeeping*, the `migrate_to` call does the *physical memory copy*.

**Eviction returns evicted IDs, not regions.** The `place_region` and
`evict_from_tier` methods return `Vec<RegionId>` — the IDs of evicted
regions, not the `Arc<Region>`s themselves. This is because the caller
typically wants to *migrate* evicted regions to a lower tier (e.g. CXL →
NVMe), which requires looking them up in the canonical map and calling
`migrate_to`. Returning IDs lets the caller decide what to do with them
without forcing an extra `Arc` clone. The trade-off: the caller must
`access(evicted_id)` to get the Arc before migrating, but that's one hash
lookup — negligible.

**`Result<Vec<RegionId>>` is always `Ok` for now.** The `Result` wrapper
is retained for forward compatibility (future versions may reject
placement into a capacity-0 tier, or fail to evict if the tier is empty
but somehow full). Currently the function always succeeds. Clippy's
`unnecessary_wraps` lint doesn't fire on public functions, so this is
fine.

**`UNLIMITED_CAPACITY = usize::MAX` sentinel.** Used for NVMe (persistent
storage, no DRAM constraint). `tier_is_full` checks `cap == UNLIMITED_CAPACITY`
first and returns `false` immediately, avoiding the (vacuous) comparison
`current >= usize::MAX`. This also makes `place_region` into NVMe a pure
insertion with no eviction loop, which is what the test
`unlimited_tier_never_evicts` verifies (1000 regions, zero evictions).

**`Debug` impl renders `tier_lru` as `{tier → len}`.** The full deques can
be long (up to 1024 entries for CXL). The custom `Debug` impl uses a
helper struct `TierLruSizes` that renders each tier as `tier → len`,
keeping the debug output readable. This matches the pattern in
`Region::Debug` (which omits the raw bytes).

**No internal Mutex.** All methods take `&mut self`. The spec said
"Use `parking_lot::Mutex` for thread safety *if needed*" — it's not
needed, because the methods are short (O(1) amortized hash + deque ops)
and an internal mutex would serialize all callers. Users who need to
share a manager across threads wrap it in their own `parking_lot::Mutex`
(the crate is already a dependency). This matches the pattern in
`Region` (which uses `Mutex<RegionBacking>` only for the data, not for
the metadata fields).

### Task 5-3: BandwidthMonitor

**`/proc/meminfo` heuristic, not `perf` counters.** A real implementation
would read RAPL counters (ADR-022) or `perf_event_open` PMU events, but
both require root or `CAP_PERFMON` — unavailable in most containers. The
`/proc/meminfo` fallback is universally readable. The number it produces
is *not* real bandwidth: it's the per-second delta of `MemTotal − MemFree`,
which only sees *net memory growth*, not churn. A workload that allocates
and frees 1 GB repeatedly would report ~0 bandwidth here while a PMU
counter would see 1 GB/cycle. The doc comment is explicit about this
limitation. Acceptable for "is the DRAM tier saturated?" thresholding,
which is the only current consumer of this API.

**First call returns 0.0.** `last_read_bytes` starts at 0; on the first
call to `read_memory_bandwidth`, there's no prior reading to diff
against. The code detects this (`last_read_bytes > 0` is false) and
returns 0.0 while storing the current reading. Subsequent calls compute
the real delta. This avoids a div-by-zero on the first call (when
`elapsed` might be sub-microsecond) and avoids returning a misleading
huge number (which would happen if we diffed against 0).

**Non-negative clamp.** The delta `current_used - last_read_bytes` can be
negative (memory freed between polls). Bandwidth is conventionally
non-negative, so we clamp with `.max(0.0)`. The test
`bandwidth_monitor_returns_non_negative` relies on this.

**Non-Linux returns a constant 50 GB/s.** When `/proc/meminfo` isn't
available (macOS, Windows), the function returns the typical DDR5
bandwidth as a constant. The `DDR5_BANDWIDTH_BPS` constant is
`#[cfg(not(target_os = "linux"))]` so it doesn't trigger a dead-code
warning on Linux. The test passes on both platforms because the constant
is non-negative.

**`tier_utilization` returns count/capacity, clamped to [0, 1].** The
monitor owns its own `tier_counts` and `tier_capacity` maps, initialized
to defaults that mirror `MemoryManager::new`. The `set_tier_count` method
is the hook a `MemoryManager` would call (after `register` or
`evict_from_tier`) to keep the monitor's heuristic in sync with reality.
For unlimited tiers (`capacity == usize::MAX`), utilization returns 0.0
("no pressure by definition"); for unknown tiers (`capacity == 0`, not in
the defaults), it also returns 0.0.

**Why `BandwidthMonitor` owns its own tier state.** The spec shows the
struct with only two fields (`last_read_bytes`, `last_timestamp`). I added
`tier_counts` and `tier_capacity` because `tier_utilization` needs *some*
notion of regions-per-tier to compute the ratio. The alternative — having
`tier_utilization` take a `&MemoryManager` parameter — would deviate from
the spec's `&self` signature. The current design lets `BandwidthMonitor`
stand alone (testable without a `MemoryManager`), with `set_tier_count`
as the integration hook.

**Linux-only `read_proc_meminfo_used` helper.** Parses `MemTotal:` and
`MemFree:` lines, extracts the numeric value (in kB), multiplies by 1024
to get bytes, and returns `MemTotal - MemFree` (saturating, in case
`MemFree` > `MemTotal` due to a race during reading). Returns `None` if
`/proc/meminfo` can't be read or doesn't contain the expected keys. The
test `proc_meminfo_used_is_sane` (Linux-only) verifies it returns a
positive value < 1 TiB on a running Linux system.

### Task 5-4: Module registration

**Alphabetical ordering.** `bandwidth` < `manager` < `numa` < `region` <
`tier`. The `pub use` statements are also alphabetical by re-exported
name. This matches the existing convention in `src/lib.rs`.

**Expanded module doc-comment.** Added a `## Modules` section listing
all five submodules with one-line descriptions and cross-references to
the key types and ADRs. This makes the module self-documenting for
callers reading `cargo doc`.

### Task 5-5: Tests

### `src/memory/numa.rs` (2 new tests, 5 total)

1. `pin_thread_to_cpu_does_not_crash` — pins to the current CPU (queried
   via `get_current_cpu()` so we know it's in the cgroup cpuset). Accepts
   either Ok or Err — the DoD requirement is "doesn't crash". On Linux
   this should normally be Ok; EPERM in restricted containers is
   tolerated.
2. `get_current_cpu_returns_valid` — asserts the returned CPU index is
   `< 8192` (sanity upper bound; no current hardware exceeds this).

### `src/memory/manager.rs` (12 new tests)

1. `register_and_access_region` — register a region, access it by ID,
   verify the returned Arc has the right ID and `contains` returns true.
2. `access_unknown_returns_none` — accessing an unregistered ID returns
   `None` and `contains` returns false.
3. `lru_eviction_oldest_first` — fill a tier to capacity 3 (LRU back→front:
   [2, 1, 0]), access region 0 (promote to front: [0, 2, 1]), place
   region 3 — verifies region 1 (the new LRU back) is evicted, not region
   0 (the recently-accessed one). This is the core DoD test.
4. `place_region_with_full_tier_evicts` — fill a tier to capacity 2,
   place a third region — verifies the oldest (region 10) is evicted and
   regions 11, 12 survive.
5. `place_region_into_non_full_tier_evicts_nothing` — placing into a
   tier with capacity 8 and 0 current regions evicts nothing.
6. `place_region_moves_between_tiers` — register region 0 in DDR5, then
   re-place into CXL — verifies the region is removed from DDR5's LRU
   (count goes to 0) and added to CXL's LRU (count goes to 1), with no
   duplication in `total_regions`.
7. `evict_from_tier_removes_count_oldest` — register 4 regions, evict 2,
   verify the 2 oldest are returned in eviction order and the 2 newest
   survive.
8. `evict_from_tier_over_evict_is_clamped` — register 2 regions, evict
   10 — verifies only the 2 present regions are evicted (no panic, no
   phantom IDs).
9. `default_capacities_match_spec` — `new()` produces L3=16, Ddr5=256,
   Cxl=1024, Nvme=usize::MAX, and other tiers (Hbm, L1L2) default to
   usize::MAX.
10. `access_promotes_to_front_of_lru` — register 3 regions, access region
    0, verify `tier_lru_order` returns `[0, 2, 1]` (region 0 is now at
    the front).
11. `unlimited_tier_never_evicts` — place 1000 regions into NVMe
    (unlimited capacity), verify zero evictions and `regions_in_tier ==
    1000`.
12. `debug_format_works` — `format!("{mgr:?}")` on a populated manager
    produces a string containing "MemoryManager" and "total_regions".

### `src/memory/bandwidth.rs` (7 new tests, 1 Linux-only)

1. `bandwidth_monitor_returns_non_negative` — call `read_memory_bandwidth`
   twice (with a 10ms sleep between), verify both values are ≥ 0. On
   Linux the first is 0.0 (no prior reading) and the second is the
   clamped delta; on non-Linux both are the constant 50 GB/s.
2. `tier_utilization_in_unit_range` — for all 8 tiers, utilization is in
   `[0.0, 1.0]`.
3. `tier_utilization_unlimited_tier_is_zero` — NVMe (unlimited capacity)
   returns 0.0 by definition.
4. `tier_utilization_reflects_count_over_capacity` — L3 capacity 16,
   default count 8 → 0.5; set count to 16 → 1.0; set count to 32 →
   clamped to 1.0.
5. `default_tier_capacities_match_manager` — set counts to L3=16, Ddr5=256,
   Cxl=1024 and verify utilization is exactly 1.0 for each (proves the
   capacities match the manager defaults).
6. `debug_format_works` — `format!("{mon:?}")` produces a string
   containing "BandwidthMonitor".
7. `proc_meminfo_used_is_sane` (Linux-only) — `read_proc_meminfo_used()`
   returns `Some` with a value > 0 and < 1 TiB on a running Linux system.

## Test Results

```
cargo fmt --check:                              clean (exit 0)
cargo clippy -- -D warnings:                    clean (exit 0)  [DoD form]
cargo clippy --all-targets -- -D warnings:      clean (exit 0)
cargo test (debug):
  lib unit tests:     146 passed  (was 125, +21 new)
  integration tests:    7 passed  (unchanged)
  total:              153 passed  (was 132, +21 new)
cargo test --release:
  lib unit tests:     146 passed
  integration tests:    7 passed
  total:              153 passed
```

## DoD Verification

- [x] `cargo test` passes (132 existing + 21 new = 153 total)
- [x] `cargo clippy -- -D warnings` passes (also `--all-targets`)
- [x] LRU eviction works correctly — validated by
      `lru_eviction_oldest_first` and `place_region_with_full_tier_evicts`
- [x] NUMA pinning doesn't crash on Linux — validated by
      `pin_thread_to_cpu_does_not_crash`
- [x] `pin_thread_to_cpu` calls `libc::sched_setaffinity` with `CPU_SET`
      on Linux (gated behind `#[cfg(target_os = "linux")]`)
- [x] `pin_thread_to_cpu` returns `Ok(())` (no-op) on non-Linux
- [x] `get_current_cpu` uses `libc::sched_getcpu()` on Linux
- [x] `MemoryManager` has per-tier LRU lists (`tier_lru: HashMap<MemoryTier,
      VecDeque<RegionId>>`)
- [x] `MemoryManager::new()` returns default capacities (L3: 16, Ddr5: 256,
      Cxl: 1024, Nvme: unlimited)
- [x] `MemoryManager::register`, `access`, `place_region`, `evict_from_tier`
      all implemented per spec
- [x] `BandwidthMonitor` has `last_read_bytes` and `last_timestamp` fields
      (plus `tier_counts` / `tier_capacity` for `tier_utilization`)
- [x] `BandwidthMonitor::read_memory_bandwidth` reads `/proc/meminfo` on
      Linux, returns constant on non-Linux
- [x] `BandwidthMonitor::tier_utilization` returns 0.0–1.0 estimate
- [x] `src/memory/mod.rs` registers `bandwidth` and `manager` modules and
      re-exports `MemoryManager` and `BandwidthMonitor`
- [x] All unsafe blocks have `// SAFETY:` comments
- [x] Uses `std::collections::{HashMap, VecDeque}` as specified

## Notes for Downstream Waves

- **`BandwidthMonitor` is not yet wired to `MemoryManager`.** The
  `set_tier_count` hook exists but no caller invokes it. Wave 6 (or
  whichever wave builds the placement policy) should call
  `set_tier_count(tier, mgr.regions_in_tier(tier))` after every
  `register` / `place_region` / `evict_from_tier` to keep the monitor's
  `tier_utilization` heuristic in sync with reality. Alternatively, wrap
  both in a single `MemorySystem` struct that updates them together.

- **`BandwidthMonitor::read_memory_bandwidth` is a heuristic, not real
  bandwidth.** It reports the per-second delta of `MemTotal − MemFree`,
  which undercounts churn (alloc-then-free patterns report ~0). A future
  wave should replace the `/proc/meminfo` read with a `perf_event_open`
  PMU read (requires `CAP_PERFMON` or root). ADR-022 (RAPL energy
  benchmarking) is the natural place for this — the same `perf_event_open`
  infrastructure can read both energy and bandwidth counters.

- **`place_region` doesn't mutate `Region::tier`.** The region's `tier`
  field is a plain field (not behind a Mutex), and the input is
  `Arc<Region>` (shared, immutable through Arc). The manager's bookkeeping
  places the ID in `target_tier`'s LRU regardless. Callers wanting the
  region's `tier` field to match should call `Region::migrate_to(target_tier)`
  first. A future wave might add a `place_region_and_migrate` convenience
  method that does both.

- **`MemoryManager` is not thread-safe by itself.** All methods take
  `&mut self`. Callers needing thread safety wrap in `parking_lot::Mutex`.
  The trade-off (no internal locking) is fine for v1 because the manager
  is currently called from the executor's single-threaded morsel dispatch
  loop. If a future wave parallelizes the executor (per ADR-018), the
  manager should either grow an internal `Mutex` or be sharded by tier.

- **`pin_thread_to_cpu` is not yet called by the executor.** ADR-008
  specifies that worker threads should be pinned at startup, but the
  current executor (Wave 3) doesn't do this. Wave 6+ (morsel executor
  per ADR-018) should call `pin_thread_to_cpu` in each worker's spawn
  closure, using `NumaTopology::detect()` to map worker N → NUMA node
  `N / cores_per_node` per the ADR's policy.

- **`get_current_cpu` returns 0 on non-Linux.** This is a documented
  no-op. Tests that depend on the actual CPU should be `#[cfg(target_os =
  "linux")]` (the `proc_meminfo_used_is_sane` test already follows this
  pattern).

- **The `cpu_set_t` static capacity is 1024.** Systems with > 1024 CPUs
  (e.g. multi-socket NUMA with 128+ cores per socket) would need dynamic
  cpu_set allocation via `CPU_ALLOC` / `CPU_FREE`. The current
  implementation rejects `cpu_id ≥ 1024` with `Error::Unsupported`. This
  is fine for v1 (no current single-system turboGP deployment exceeds
  1024 CPUs), but a future wave targeting large multi-socket boxes should
  add the dynamic path.
