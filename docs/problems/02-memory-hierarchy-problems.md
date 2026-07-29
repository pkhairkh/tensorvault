# Memory Hierarchy Problems

> Problems related to the tier-aware memory manager: placement, migration,
> NUMA topology, CXL discovery, and the cost model that drives placement
> decisions.
>
> **Research source**: `docs/cpu_energy_kb.md` §2 (memory hierarchy),
> §4 (interconnects), §5 (NUMA topology); `docs/instruction_first_architecture.md`.

---

## P-02-01: Tier detection and discovery 🟢

**Layer**: Memory hierarchy
**Status**: 🟢 solved (NUMA topology detection exists in `src/memory/numa.rs`)
**Math**: none
**Effort**: —
**Impact**: high

### Problem (solved)

The engine must detect the available memory tiers at startup: L3 (always
present), DDR5 (always present on servers), HBM (Xeon Max, MI300A), CXL
(Type-3 devices), NVMe (always present).

### Resolution

`NumaTopology::detect()` reads `/sys/devices/system/node/` on Linux and
classifies each NUMA node by tier (DDR5 if it has CPUs, CXL if memory-only).
CXL availability is checked via `/sys/bus/cxl`.

### Open follow-up

- HBM detection on Xeon Max is not implemented.
- Apple Silicon UMA has no NUMA; the topology returns a single node.

---

## P-02-02: Region placement policy (initial) 🟡

**Layer**: Memory hierarchy
**Status**: 🟡 partial (regions can be allocated in any tier, but no automatic policy)
**Math**: IV (optimization — knapsack, LP)
**Effort**: M
**Impact**: high

### Problem

When a new region is created, which tier should it go in? The current API
requires the caller to specify the tier explicitly. We need an automatic
policy that considers:

- Expected access frequency (from the schema or query history)
- Working set fit (does this region fit in L3? DDR5? CXL?)
- Latency sensitivity (is this region on the critical path?)
- Energy budget (is moving this region cheaper than the cumulative access cost?)

### Open questions

- Should the policy be a knapsack (maximize value given capacity) or an LP
  (minimize cost given demand)?
- How do we predict access frequency for a new region?

### Success criteria

- A `PlacementPolicy` trait with implementations: `HotFirst` (L3 → DDR5 →
  CXL → NVMe), `CapacityFirst` (fill DDR5 before CXL), `EnergyOptimal`
  (LP-based).
- The memory manager picks the policy based on workload type.

---

## P-02-03: Region migration mechanics 🟡

**Layer**: Memory hierarchy
**Status**: 🟡 partial (`Region::migrate_to()` exists, but it's a full copy)
**Math**: none
**Effort**: M
**Impact**: high

### Problem

The current `migrate_to()` does a full `memcpy` of the 2 MB region. This is
correct but slow (~20 µs for 2 MB at 100 GB/s DRAM bandwidth). For
frequently-migrated regions, this dominates.

Real migration should use:
- `migrate_pages(2)` syscall on Linux for NUMA migration (page-level, no copy)
- `mbind(2)` for policy-based placement
- `move_pages(2)` for explicit migration with latency measurement

### Open questions

- Does `migrate_pages` work for CXL NUMA nodes?
- Should we use `userfaultfd` for lazy migration?

### Success criteria

- `Region::migrate_to()` uses `migrate_pages` on Linux.
- Migration latency < 1 ms for a 2 MB region.
- A benchmark that migrates a region 1000 times and measures overhead.

---

## P-02-04: Tier-aware region migration policy with competitive ratio 🔴

**Layer**: Memory hierarchy
**Status**: 🔴 open
**Math**: IV (online algorithms — LRU is k-competitive; Work Function Algorithm is (2k-1)-competitive)
**Effort**: L (3–6 months)
**Impact**: critical

### Problem

This is the **must-solve** problem for the memory manager. Given a stream
of region accesses, which regions should be in which tier?

The online paging problem: we have k tiers, each with finite capacity. An
access to a region in tier T costs `latency(T)`. A migration costs
`migration_cost(region, from_tier, to_tier)`. We want to minimize total cost.

**Theoretical bounds** (from `docs/research/optimization_theory_db.md` §12):
- LRU is k-competitive (Sleator-Tarjan 1985)
- The Work Function Algorithm is (2k-1)-competitive (Koutsoupias-Papadimitriou 1995)
- No deterministic online algorithm can beat k-competitive

### Open questions

- Is LRU good enough, or do we need WFA?
- How do we handle the multi-tier case (k > 2)? Most theory is for 2-tier.
- How does CXL's variable latency (modeled by Kingman — see P-05-03) change
  the competitive analysis?

### Success criteria

- A `MigrationPolicy` that is provably k-competitive.
- A benchmark showing the policy achieves ≤ 2× the offline optimal cost on
  realistic access traces.

---

## P-02-05: NUMA-aware thread pinning 🟡

**Layer**: Memory hierarchy
**Status**: 🟡 partial (NUMA topology is detected, but no thread pinning)
**Math**: none
**Effort**: M
**Impact**: high

### Problem

Cross-socket memory access is 1.5–2× local latency and 2–4× energy (see
`cpu_energy_kb.md` §5.6). The executor's worker threads must be pinned to
the same NUMA node as the data they're scanning.

### Open questions

- Should we pin one worker thread per physical core (SMT off) or per logical
  core (SMT on)?
- How do we handle cross-NUMA joins (build side on NUMA 0, probe side on
  NUMA 1)?

### Success criteria

- The executor pins worker threads to NUMA nodes via `pthread_setaffinity_np`.
- A benchmark showing cross-NUMA access is < 5% of total (vs ~50% without
  pinning).

---

## P-02-06: CXL latency variability modeling 🔴

**Layer**: Memory hierarchy
**Status**: 🔴 open
**Math**: III (probability — Kingman's formula, queueing theory)
**Effort**: L
**Impact**: critical

### Problem

CXL.mem latency is not a fixed number — it's a distribution:
- Best case: ~140 ns
- Typical: ~250 ns
- Contended: ~350–520 ns

The memory manager must model this distribution, not just the mean, to make
good placement decisions. Kingman's formula (see `docs/research/probability_sketching_for_db.md`
§11) predicts the mean waiting time from utilization and variability:

$$
W \approx \frac{\rho}{1-\rho} \cdot \frac{c_a^2 + c_s^2}{2} \cdot \mu^{-1}
$$

### Open questions

- Is the CXL latency distribution lognormal? Weibull? (Empirical measurement
  needed.)
- How do we instrument the CXL link to collect arrival/service time
  statistics?
- Should the planner use p50, p99, or p99.9 for cost estimation?

### Success criteria

- A `TierLatencyStats` struct that tracks arrival rate, service rate, and
  variances per tier.
- A `KingmanPredictor` that predicts p99 latency within 20% of measured.
- The planner uses predicted p99 (not mean) for tier placement.

---

## P-02-07: HBM tier support (Xeon Max, MI300A) 🔴

**Layer**: Memory hierarchy
**Status**: 🔴 open
**Math**: none
**Effort**: M
**Impact**: medium

### Problem

HBM (High Bandwidth Memory) on Xeon Max (64 GB HBM2E, ~1.6 TB/s) and MI300A
(128 GB HBM3, 5.3 TB/s) is a distinct tier from DDR5. It has:
- Higher bandwidth (5–10× DDR5)
- Lower latency (~100–150 ns vs ~90 ns for DDR5)
- Smaller capacity

The memory manager should treat HBM as a distinct tier and place
scan-heavy working sets there.

### Open questions

- How do we detect HBM on Xeon Max? (NUMA node with specific attributes?)
- Should HBM be managed by the OS (as a NUMA node) or by the engine directly?

### Success criteria

- `MemoryTier::Hbm` is detected and populated on Xeon Max.
- A benchmark showing scan throughput on HBM is 5× DDR5.

---

## P-02-08: CXL memory pooling (multi-host) 🔴

**Layer**: Memory hierarchy
**Status**: 🔴 open
**Math**: V (category theory — sheaves for distributed consistency)
**Effort**: XL (6+ months)
**Impact**: high

### Problem

CXL 3.0 enables memory pooling: multiple hosts share a CXL-attached memory
device via a switch. This is the path to rack-scale memory disaggregation
(see `docs/cpu_energy_kb.md` §2.5).

The engine must:
1. Discover pooled CXL memory devices
2. Negotiate allocation with other hosts (or a central manager)
3. Handle the case where a pooled region is reclaimed by another host

### Open questions

- Should we use the Linux CXL subsystem (`/sys/bus/cxl/devices/`) or a
  custom manager?
- How do we handle fault tolerance if a host holding a pooled region crashes?

### Success criteria

- The memory manager can allocate regions from a CXL pool.
- A multi-host test where host A writes to a pooled region and host B reads it.

---

## P-02-09: Memory bandwidth monitoring 🔴

**Layer**: Memory hierarchy
**Status**: 🔴 open
**Math**: none
**Effort**: M
**Impact**: medium

### Problem

The memory manager needs to know the current bandwidth utilization of each
tier to make good placement decisions. If DDR5 is saturated, new regions
should go to CXL.

On Linux, this is available via:
- `perf stat -e dram_read_cycles,dram_write_cycles` (Intel)
- `pcm-memory` (Intel Performance Counter Monitor)
- AMD's `amd_pmu` for Zen

### Open questions

- Can we read bandwidth counters without root?
- How frequently should we sample? (Too frequent → overhead; too sparse →
  stale data.)

### Success criteria

- A `BandwidthMonitor` that samples each tier's utilization every 100 ms.
- The placement policy reads the monitor before placing a new region.

---

## P-02-10: Large page (2 MB / 1 GB) management 🟡

**Layer**: Memory hierarchy
**Status**: 🟡 partial (regions are 2 MB, but we don't explicitly request huge pages)
**Math**: none
**Effort**: S
**Impact**: medium

### Problem

TLB misses are expensive (~7–30 cycles for a 2-level page walk; see
`cpu_energy_kb.md` §4.2). Using 2 MB huge pages instead of 4 KB pages
reduces TLB pressure by 512×.

The region size is 2 MB (matching huge page granularity), but we don't
explicitly request huge pages via `mmap(MAP_HUGETLB)` or `madvise(MADV_HUGEPAGE)`.

### Open questions

- Should we use transparent huge pages (THP) or explicit huge pages?
- How do we handle fragmentation? (Huge pages need contiguous physical memory.)

### Success criteria

- All region allocations use huge pages.
- A benchmark showing TLB miss rate drops by > 100×.

---

## P-02-11: Memory-tier-aware allocator 🔴

**Layer**: Memory hierarchy
**Status**: 🔴 open
**Math**: none
**Effort**: L
**Impact**: high

### Problem

The current `Region::allocate()` uses `Vec::with_capacity()` which goes
through the global allocator (jemalloc/malloc). This allocator doesn't know
about tiers — it can't allocate from a specific NUMA node or CXL device.

We need a tier-aware allocator that:
1. Allocates from the specified NUMA node (`numa_alloc_onnode`)
2. Uses huge pages
3. Supports CXL devices (via `/dev/daxN.M` or `ndctl`)

### Open questions

- Should we use `mmap` directly with `MAP_HUGETLB | MAP_POPULATE`?
- How do we integrate with Rust's `GlobalAlloc` trait?

### Success criteria

- A `TierAllocator` struct that allocates from a specific tier.
- `Region::allocate(id, tier)` uses the tier allocator.

---

## P-02-12: Cold-start memory warmup 🔴

**Layer**: Memory hierarchy
**Status**: 🔴 open
**Math**: none
**Effort**: M
**Impact**: medium

### Problem

On cold start, the buffer pool is empty — all data is on NVMe. The first
queries are slow because every page must be fetched from NVMe (~20 µs per
page).

A warmup strategy could:
1. Prefetch the working set (identified by query history) into DDR5/CXL
   before the first query arrives.
2. Use `madvise(MADV_WILLNEED)` to trigger async readahead.
3. Pin hot indexes in L3 via `mlock`.

### Open questions

- How do we identify the "working set" without query history?
- Should warmup be explicit (a `WARMUP` SQL command) or implicit?

### Success criteria

- A `warmup(table)` function that prefetches a table's regions into DDR5.
- Cold-start query latency drops by > 5× after warmup.

---

## Summary

| # | Problem | Status | Effort | Impact |
|---|---------|--------|--------|--------|
| 01 | Tier detection and discovery | 🟢 | — | high |
| 02 | Region placement policy (initial) | 🟡 | M | high |
| 03 | Region migration mechanics | 🟡 | M | high |
| 04 | Tier-aware migration with competitive ratio | 🔴 | L | critical |
| 05 | NUMA-aware thread pinning | 🟡 | M | high |
| 06 | CXL latency variability modeling | 🔴 | L | critical |
| 07 | HBM tier support (Xeon Max, MI300A) | 🔴 | M | medium |
| 08 | CXL memory pooling (multi-host) | 🔴 | XL | high |
| 09 | Memory bandwidth monitoring | 🔴 | M | medium |
| 10 | Large page (2 MB / 1 GB) management | 🟡 | S | medium |
| 11 | Memory-tier-aware allocator | 🔴 | L | high |
| 12 | Cold-start memory warmup | 🔴 | M | medium |
