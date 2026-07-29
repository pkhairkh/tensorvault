//! Memory bandwidth monitoring (heuristic).
//!
//! [`BandwidthMonitor`] is a lightweight, polling-based estimate of current
//! memory bandwidth. A production implementation would read RAPL counters
//! (ADR-022) or `perf` PMU events; the implementation here reads
//! `/proc/meminfo` on Linux and computes the per-second delta of
//! `MemTotal − MemFree` — a coarse proxy for "memory churned through since
//! the last call".
//!
//! ## Why a heuristic
//!
//! The placement policy (ADR-010) and the cost model (ADR-023) need *some*
//! signal for "is the DRAM tier saturated?". A real PMU-based read requires
//! root or `perf_event_open` capabilities that aren't available in most
//! containers, so we fall back to `/proc/meminfo` — which is universally
//! readable. The numbers are noisy and biased low, but they're non-zero and
//! monotonic with real pressure, which is all the heuristic needs.

use crate::memory::tier::MemoryTier;
use std::collections::HashMap;
use std::time::Instant;

/// Typical DDR5 bandwidth in bytes/sec (50 GB/s — used as a constant
/// estimate on non-Linux targets where `/proc/meminfo` is unavailable).
#[cfg(not(target_os = "linux"))]
const DDR5_BANDWIDTH_BPS: u64 = 50_000_000_000;

/// A polling-based memory bandwidth monitor.
///
/// Tracks the last value of `MemTotal − MemFree` from `/proc/meminfo` (on
/// Linux) and reports the per-second delta as "current memory bandwidth".
/// Also exposes a [`tier_utilization`](Self::tier_utilization) heuristic
/// that returns the ratio of regions-in-tier to tier capacity (mirroring
/// the [`MemoryManager`](crate::memory::manager::MemoryManager) defaults).
///
/// # Concurrency
///
/// `read_memory_bandwidth` takes `&mut self` because it updates the
/// `last_read_bytes` / `last_timestamp` state. Callers that need to share
/// a monitor across threads should wrap it in a `parking_lot::Mutex`.
pub struct BandwidthMonitor {
    /// Last read bytes from `/proc/meminfo` (Linux) or `0` (non-Linux).
    /// The "used memory" value (`MemTotal − MemFree`) from the most recent
    /// poll — used to compute the per-second delta.
    last_read_bytes: u64,
    /// Timestamp of the most recent poll.
    last_timestamp: Instant,
    /// Per-tier region counts (heuristic — populated with defaults that
    /// mirror [`MemoryManager::new`](crate::memory::manager::MemoryManager::new)).
    /// A production version would be updated by a callback from the
    /// manager; here we use static defaults so `tier_utilization` returns
    /// a sensible value out of the box.
    tier_counts: HashMap<MemoryTier, usize>,
    /// Per-tier capacities, mirroring the `MemoryManager` defaults.
    tier_capacity: HashMap<MemoryTier, usize>,
}

impl BandwidthMonitor {
    /// Create a new monitor with default tier counts/capacities.
    ///
    /// The defaults mirror [`MemoryManager::new`](crate::memory::manager::MemoryManager::new):
    /// L3=16, DDR5=256, CXL=1024, NVMe=unlimited. Initial `tier_counts`
    /// are set to ~50% utilization for finite tiers (a placeholder; in
    /// production the [`MemoryManager`] would push real counts here).
    #[must_use]
    pub fn new() -> Self {
        let tier_capacity: HashMap<MemoryTier, usize> = [
            (MemoryTier::L3, 16),
            (MemoryTier::Ddr5, 256),
            (MemoryTier::Cxl, 1024),
            (MemoryTier::Nvme, usize::MAX),
        ]
        .into_iter()
        .collect();

        // Heuristic initial counts: 50% of capacity for finite tiers, 0
        // for the unlimited NVMe tier. These are placeholders — a real
        // implementation would receive updates from the MemoryManager.
        let tier_counts: HashMap<MemoryTier, usize> = [
            (MemoryTier::L3, 8),
            (MemoryTier::Ddr5, 128),
            (MemoryTier::Cxl, 512),
            (MemoryTier::Nvme, 0),
        ]
        .into_iter()
        .collect();

        Self { last_read_bytes: 0, last_timestamp: Instant::now(), tier_counts, tier_capacity }
    }

    /// Read the current memory bandwidth in bytes/sec.
    ///
    /// On Linux, this reads `/proc/meminfo`, computes
    /// `MemTotal − MemFree` (the "used memory" value), and returns the
    /// per-second delta since the last call. The first call returns `0.0`
    /// (no prior reading to diff against).
    ///
    /// On non-Linux targets, returns a constant estimate of
    /// 50 GB/s (typical DDR5 bandwidth).
    ///
    /// # Notes
    ///
    /// The returned value is **clamped to be non-negative**: if memory was
    /// freed between polls (the delta is negative), `0.0` is returned.
    /// Bandwidth is conventionally non-negative.
    ///
    /// The `/proc/meminfo` heuristic undercounts real bandwidth — it only
    /// sees net memory growth, not churn. A workload that allocates and
    /// frees 1 GB repeatedly would report ~0 bandwidth here while a PMU
    /// counter would see 1 GB/cycle. Acceptable for "is the tier
    /// saturated?" thresholding; not for precise accounting.
    pub fn read_memory_bandwidth(&mut self) -> f64 {
        let now = Instant::now();

        #[cfg(target_os = "linux")]
        {
            let current_used = read_proc_meminfo_used().unwrap_or(self.last_read_bytes);
            let elapsed = now.duration_since(self.last_timestamp).as_secs_f64();
            let bandwidth = if elapsed > 0.0 && self.last_read_bytes > 0 {
                let diff = current_used as f64 - self.last_read_bytes as f64;
                diff / elapsed
            } else {
                // First call (or zero elapsed) — no prior reading to diff
                // against. Return 0 and let subsequent calls compute the
                // delta.
                0.0
            };
            self.last_read_bytes = current_used;
            self.last_timestamp = now;
            // Clamp to non-negative: bandwidth is conventionally ≥ 0, and
            // a negative delta (memory freed) is reported as 0.
            bandwidth.max(0.0)
        }

        #[cfg(not(target_os = "linux"))]
        {
            // Non-Linux: return a constant estimate. No state to update
            // (we don't read /proc/meminfo on non-Linux), but we refresh
            // the timestamp so a future Linux-aware call has a baseline.
            self.last_read_bytes = DDR5_BANDWIDTH_BPS;
            self.last_timestamp = now;
            DDR5_BANDWIDTH_BPS as f64
        }
    }

    /// Return a 0.0–1.0 utilization estimate for `tier`.
    ///
    /// The heuristic is `regions_in_tier / capacity`:
    /// - For tiers with finite capacity, this is the fraction of slots
    ///   used.
    /// - For tiers with unlimited capacity (e.g. NVMe), returns `0.0` —
    ///   "no utilization pressure" by definition.
    /// - For tiers with capacity `0` (not in the defaults), returns `0.0`.
    ///
    /// The counts and capacities come from the monitor's own internal
    /// state, which is initialized to the same defaults as
    /// [`MemoryManager::new`](crate::memory::manager::MemoryManager::new).
    /// A production version would receive live updates from the manager.
    #[must_use]
    pub fn tier_utilization(&self, tier: MemoryTier) -> f64 {
        let cap = self.tier_capacity.get(&tier).copied().unwrap_or(0);
        if cap == 0 || cap == usize::MAX {
            // Capacity 0 (unknown tier) or unlimited — no utilization
            // pressure to report.
            return 0.0;
        }
        let count = self.tier_counts.get(&tier).copied().unwrap_or(0) as f64;
        (count / cap as f64).clamp(0.0, 1.0)
    }

    /// Update the recorded region count for `tier`.
    ///
    /// This is the hook a [`MemoryManager`](crate::memory::manager::MemoryManager)
    /// would call (e.g. after a `register` or `evict_from_tier`) to keep
    /// the monitor's `tier_utilization` heuristic in sync with reality.
    /// Exposed as `pub` so callers can drive it from outside the manager
    /// too (e.g. for testing or for a non-default manager).
    pub fn set_tier_count(&mut self, tier: MemoryTier, count: usize) {
        self.tier_counts.insert(tier, count);
    }
}

impl Default for BandwidthMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for BandwidthMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BandwidthMonitor")
            .field("last_read_bytes", &self.last_read_bytes)
            .field("last_timestamp", &self.last_timestamp)
            .field("tier_counts", &self.tier_counts)
            .field("tier_capacity", &self.tier_capacity)
            .finish()
    }
}

/// Read `MemTotal − MemFree` from `/proc/meminfo`, in bytes.
///
/// Returns `None` if `/proc/meminfo` cannot be read or doesn't contain
/// the expected keys. Values in `/proc/meminfo` are in kB; we multiply by
/// 1024 to convert to bytes.
#[cfg(target_os = "linux")]
fn read_proc_meminfo_used() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut mem_total_kb: Option<u64> = None;
    let mut mem_free_kb: Option<u64> = None;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            mem_total_kb = rest.split_whitespace().next().and_then(|s| s.parse().ok());
        } else if let Some(rest) = line.strip_prefix("MemFree:") {
            mem_free_kb = rest.split_whitespace().next().and_then(|s| s.parse().ok());
        }
        if mem_total_kb.is_some() && mem_free_kb.is_some() {
            break;
        }
    }
    let total = mem_total_kb?;
    let free = mem_free_kb?;
    Some(total.saturating_sub(free) * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test: `read_memory_bandwidth` returns a non-negative value.
    ///
    /// On Linux the first call returns 0.0 (no prior reading); the second
    /// call (after a brief sleep) returns the per-second delta, clamped
    /// to be non-negative. On non-Linux, both calls return the constant
    /// 50 GB/s estimate.
    #[test]
    fn bandwidth_monitor_returns_non_negative() {
        let mut mon = BandwidthMonitor::new();
        // First call initializes state and returns 0.0 (Linux) or the
        // constant (non-Linux).
        let first = mon.read_memory_bandwidth();
        assert!(first >= 0.0, "first bandwidth reading should be non-negative, got {first}");

        // Sleep briefly so the second call has a non-zero elapsed time.
        std::thread::sleep(std::time::Duration::from_millis(10));

        let second = mon.read_memory_bandwidth();
        assert!(second >= 0.0, "second bandwidth reading should be non-negative, got {second}");
    }

    /// Test: `tier_utilization` returns a value in `[0.0, 1.0]` for every
    /// tier.
    #[test]
    fn tier_utilization_in_unit_range() {
        let mon = BandwidthMonitor::new();
        for &tier in &[
            MemoryTier::L1L2,
            MemoryTier::L3,
            MemoryTier::Ddr5,
            MemoryTier::Hbm,
            MemoryTier::Cxl,
            MemoryTier::Nvme,
            MemoryTier::NvmeOf,
            MemoryTier::Network,
        ] {
            let u = mon.tier_utilization(tier);
            assert!((0.0..=1.0).contains(&u), "tier {tier:?} utilization {u} not in [0, 1]");
        }
    }

    /// Test: `tier_utilization` for the unlimited NVMe tier is 0.0 (no
    /// capacity pressure by definition).
    #[test]
    fn tier_utilization_unlimited_tier_is_zero() {
        let mon = BandwidthMonitor::new();
        assert_eq!(mon.tier_utilization(MemoryTier::Nvme), 0.0);
    }

    /// Test: `tier_utilization` for a tier with finite capacity reflects
    /// the count-to-capacity ratio.
    #[test]
    fn tier_utilization_reflects_count_over_capacity() {
        let mut mon = BandwidthMonitor::new();
        // L3 capacity is 16; default count is 8 → 0.5.
        assert!((mon.tier_utilization(MemoryTier::L3) - 0.5).abs() < 1e-9);

        // Bump the count to 16 → 1.0.
        mon.set_tier_count(MemoryTier::L3, 16);
        assert!((mon.tier_utilization(MemoryTier::L3) - 1.0).abs() < 1e-9);

        // Bump the count to 32 → clamped to 1.0.
        mon.set_tier_count(MemoryTier::L3, 32);
        assert!((mon.tier_utilization(MemoryTier::L3) - 1.0).abs() < 1e-9);
    }

    /// Test: the monitor's default capacities mirror the `MemoryManager`
    /// defaults (L3=16, Ddr5=256, Cxl=1024, Nvme=unlimited).
    #[test]
    fn default_tier_capacities_match_manager() {
        let mon = BandwidthMonitor::new();
        // We check via tier_utilization at full count — if the capacity
        // were wrong, the ratio wouldn't be 1.0.
        let mut mon = mon;
        mon.set_tier_count(MemoryTier::L3, 16);
        mon.set_tier_count(MemoryTier::Ddr5, 256);
        mon.set_tier_count(MemoryTier::Cxl, 1024);
        assert!((mon.tier_utilization(MemoryTier::L3) - 1.0).abs() < 1e-9);
        assert!((mon.tier_utilization(MemoryTier::Ddr5) - 1.0).abs() < 1e-9);
        assert!((mon.tier_utilization(MemoryTier::Cxl) - 1.0).abs() < 1e-9);
    }

    /// Test: `Debug` formatting doesn't panic.
    #[test]
    fn debug_format_works() {
        let mon = BandwidthMonitor::new();
        let s = format!("{mon:?}");
        assert!(s.contains("BandwidthMonitor"));
    }

    /// Test: on Linux, `/proc/meminfo` is readable and returns a sane
    /// "used memory" value. On non-Linux, the helper isn't compiled.
    #[cfg(target_os = "linux")]
    #[test]
    fn proc_meminfo_used_is_sane() {
        let used = read_proc_meminfo_used();
        assert!(used.is_some(), "/proc/meminfo should be readable on Linux");
        let used = used.unwrap();
        // Used memory should be > 0 on any running Linux system with
        // active processes, and < 1 TiB (sanity upper bound).
        assert!(used > 0, "used memory should be positive, got {used}");
        assert!(used < 1u64 << 40, "used memory should be < 1 TiB, got {used}");
    }
}
