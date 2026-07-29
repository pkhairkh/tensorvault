//! Per-tier LRU memory manager (ADR-010).
//!
//! The [`MemoryManager`] tracks which [`Region`](crate::memory::region::Region)s
//! are resident in each [`MemoryTier`] and decides which to evict when a tier
//! is full. The eviction policy is **LRU** (Least Recently Used): on access,
//! a region moves to the front of its tier's LRU list; on insertion into a
//! full tier, the back of the list (the least-recently-used region) is
//! evicted.
//!
//! ## Why LRU
//!
//! LRU is **k-competitive** (Sleator-Tarjan 1985): the total migration cost
//! is at most `k ×` the offline optimal, where `k` is the number of tiers.
//! For our 4-tier hot path (L3 / DDR5 / CXL / NVMe) this gives a formal 4×
//! bound on migration overhead — see ADR-010 for the full analysis.
//!
//! ## Concurrency
//!
//! All methods take `&mut self`. Callers that need to share a manager
//! across threads should wrap it in a `parking_lot::Mutex` (already in
//! `Cargo.toml`) — the manager itself does not perform internal locking,
//! because the LRU operations are short and contention on a single mutex
//! would dominate.

use crate::memory::region::{Region, RegionId};
use crate::memory::tier::MemoryTier;
use crate::Result;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Sentinel capacity value indicating an unlimited tier.
///
/// Used for [`MemoryTier::Nvme`] by default — the NVMe tier can grow without
/// bound because it's backed by persistent storage, not DRAM.
const UNLIMITED_CAPACITY: usize = usize::MAX;

/// A tier-aware memory manager with per-tier LRU eviction (ADR-010).
///
/// The manager is the k-server solver for the tier-migration problem: it
/// decides which regions to evict from a tier when a new region needs to be
/// placed there. The eviction policy is LRU, which is k-competitive (cost
/// ≤ k × offline optimal, where k is the number of tiers).
///
/// # Layout
///
/// - `tier_lru` — one [`VecDeque`] per tier, ordered front = most-recently-
///   used, back = least-recently-used.
/// - `regions` — the canonical `RegionId → Arc<Region>` map. Every ID in
///   any `tier_lru` deque is present here; eviction removes from both.
/// - `tier_capacity` — max regions per tier. Defaults: L3=16, DDR5=256,
///   CXL=1024, NVMe=unlimited. Tiers not present in the map default to
///   unlimited.
pub struct MemoryManager {
    /// Per-tier LRU lists: region IDs ordered by last access
    /// (front = most recent).
    tier_lru: HashMap<MemoryTier, VecDeque<RegionId>>,
    /// All registered regions, keyed by ID.
    regions: HashMap<RegionId, Arc<Region>>,
    /// Capacity per tier (in number of regions). Missing entries default
    /// to [`UNLIMITED_CAPACITY`].
    tier_capacity: HashMap<MemoryTier, usize>,
}

impl MemoryManager {
    /// Create a new manager with the default per-tier capacities.
    ///
    /// | Tier | Default capacity |
    /// |------|-----------------|
    /// | L3 | 16 regions (32 MB) |
    /// | DDR5 | 256 regions (512 MB) |
    /// | CXL | 1024 regions (2 GB) |
    /// | NVMe | unlimited |
    ///
    /// All other tiers default to unlimited.
    #[must_use]
    pub fn new() -> Self {
        let mut caps = HashMap::new();
        caps.insert(MemoryTier::L3, 16);
        caps.insert(MemoryTier::Ddr5, 256);
        caps.insert(MemoryTier::Cxl, 1024);
        caps.insert(MemoryTier::Nvme, UNLIMITED_CAPACITY);
        Self::with_capacity(caps)
    }

    /// Create a new manager with the given per-tier capacities.
    ///
    /// Tiers not present in `capacities` default to unlimited.
    #[must_use]
    pub fn with_capacity(capacities: HashMap<MemoryTier, usize>) -> Self {
        Self { tier_lru: HashMap::new(), regions: HashMap::new(), tier_capacity: capacities }
    }

    /// Register a region with the manager.
    ///
    /// The region is added to its own `tier`'s LRU list (the region's
    /// `tier` field determines placement). If a region with the same ID is
    /// already registered, it is replaced: the old entry is removed from
    /// its (possibly different) tier's LRU, and the new region takes its
    /// place at the front of the new tier's LRU.
    ///
    /// This does **not** trigger eviction — registering a region in a full
    /// tier will leave the tier over capacity. Use [`place_region`](Self::place_region)
    /// if eviction is desired.
    pub fn register(&mut self, region: Arc<Region>) {
        let region_id = region.id;
        let tier = region.tier;

        // If re-registering an existing ID, remove it from its old tier's
        // LRU first so the same ID doesn't appear in two deques.
        self.remove_from_any_tier(region_id);

        self.regions.insert(region_id, region);
        self.tier_lru.entry(tier).or_default().push_front(region_id);
    }

    /// Access a region by ID, moving it to the front of its tier's LRU.
    ///
    /// Returns a cloned [`Arc<Region>`] if the region is registered, or
    /// `None` if no region with that ID is known to the manager.
    ///
    /// The LRU move-to-front is what makes the manager k-competitive
    /// (Sleator-Tarjan): recently-accessed regions survive eviction.
    pub fn access(&mut self, region_id: RegionId) -> Option<Arc<Region>> {
        let region = self.regions.get(&region_id).cloned()?;
        let tier = region.tier;

        // Move to front: remove from current position, then push_front.
        if let Some(lru) = self.tier_lru.get_mut(&tier) {
            lru.retain(|&id| id != region_id);
            lru.push_front(region_id);
        }

        Some(region)
    }

    /// Place a region into a specific tier, evicting LRU regions if full.
    ///
    /// This is the primary placement API. The region is inserted at the
    /// front of `target_tier`'s LRU. If `target_tier` is at capacity, the
    /// least-recently-used regions are evicted from the back (in LRU order)
    /// until there is room. The IDs of evicted regions are returned so the
    /// caller can migrate them to a lower tier (e.g. CXL → NVMe).
    ///
    /// # Region tier field
    ///
    /// The region's own `tier` field is **not** mutated — `Arc<Region>`
    /// is shared and the field is immutable through an `Arc`. The
    /// manager's bookkeeping places the ID in `target_tier`'s LRU regardless
    /// of `region.tier`. Callers that need the region's `tier` field to
    /// match should call [`Region::migrate_to`] first to obtain a new
    /// region with the desired tier.
    ///
    /// # Errors
    ///
    /// Currently always returns `Ok`. The `Result` wrapper is retained for
    /// forward compatibility (future versions may reject placement into a
    /// tier with capacity 0, or fail to evict if the tier is empty but
    /// somehow full).
    pub fn place_region(
        &mut self,
        region: Arc<Region>,
        target_tier: MemoryTier,
    ) -> Result<Vec<RegionId>> {
        let region_id = region.id;

        // If the region is already registered (possibly in a different
        // tier), remove it from its old LRU first.
        self.remove_from_any_tier(region_id);

        // Evict from the back of target_tier until there's room.
        let mut evicted = Vec::new();
        while self.tier_is_full(target_tier) {
            match self.pop_back_from_tier(target_tier) {
                Some(evicted_id) => {
                    self.regions.remove(&evicted_id);
                    evicted.push(evicted_id);
                }
                None => break,
            }
        }

        // Insert the region into the canonical map and the target tier's
        // LRU (at the front, since it's the most-recently-placed).
        self.regions.insert(region_id, region);
        self.tier_lru.entry(target_tier).or_default().push_front(region_id);

        Ok(evicted)
    }

    /// Evict `count` LRU regions from the back of `tier`'s LRU list.
    ///
    /// Returns the IDs of the evicted regions, in eviction order (oldest
    /// first). If the tier has fewer than `count` regions, all of them are
    /// evicted and the returned vector is shorter than `count`.
    ///
    /// Evicted regions are removed from both the tier's LRU and the
    /// canonical `regions` map, so subsequent [`access`](Self::access)
    /// calls for those IDs return `None`.
    pub fn evict_from_tier(&mut self, tier: MemoryTier, count: usize) -> Vec<RegionId> {
        let mut evicted = Vec::with_capacity(count);
        for _ in 0..count {
            match self.pop_back_from_tier(tier) {
                Some(id) => {
                    self.regions.remove(&id);
                    evicted.push(id);
                }
                None => break,
            }
        }
        evicted
    }

    /// Number of regions currently resident in `tier`.
    #[must_use]
    pub fn regions_in_tier(&self, tier: MemoryTier) -> usize {
        self.tier_lru.get(&tier).map_or(0, VecDeque::len)
    }

    /// Total number of regions known to the manager (across all tiers).
    #[must_use]
    pub fn total_regions(&self) -> usize {
        self.regions.len()
    }

    /// Is a region with the given ID currently registered?
    #[must_use]
    pub fn contains(&self, region_id: RegionId) -> bool {
        self.regions.contains_key(&region_id)
    }

    /// Capacity of a tier (number of regions). Returns [`UNLIMITED_CAPACITY`]
    /// for tiers with no explicit capacity.
    #[must_use]
    pub fn tier_capacity(&self, tier: MemoryTier) -> usize {
        self.tier_capacity.get(&tier).copied().unwrap_or(UNLIMITED_CAPACITY)
    }

    /// Return the LRU order of a tier's region IDs (front = MRU, back = LRU).
    ///
    /// Useful for diagnostics and tests — the returned slice is a snapshot;
    /// mutating the manager invalidates it.
    #[must_use]
    pub fn tier_lru_order(&self, tier: MemoryTier) -> Vec<RegionId> {
        self.tier_lru.get(&tier).map_or(Vec::new(), |d| d.iter().copied().collect())
    }

    /// Is `tier` at or above its configured capacity?
    fn tier_is_full(&self, tier: MemoryTier) -> bool {
        let cap = self.tier_capacity(tier);
        if cap == UNLIMITED_CAPACITY {
            return false;
        }
        self.regions_in_tier(tier) >= cap
    }

    /// Pop the least-recently-used region ID from `tier`'s LRU.
    ///
    /// Does **not** remove the region from the canonical `regions` map —
    /// the caller is responsible for that.
    fn pop_back_from_tier(&mut self, tier: MemoryTier) -> Option<RegionId> {
        self.tier_lru.get_mut(&tier).and_then(VecDeque::pop_back)
    }

    /// Remove `region_id` from whichever tier's LRU it's currently in (if
    /// any). Used by `register` and `place_region` to avoid double-counting
    /// an ID across two deques when re-placing an existing region.
    fn remove_from_any_tier(&mut self, region_id: RegionId) {
        if let Some(old_region) = self.regions.get(&region_id) {
            let old_tier = old_region.tier;
            if let Some(old_lru) = self.tier_lru.get_mut(&old_tier) {
                old_lru.retain(|&id| id != region_id);
            }
        }
    }
}

impl Default for MemoryManager {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MemoryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryManager")
            .field("total_regions", &self.regions.len())
            .field("tier_lru_sizes", &TierLruSizes(self))
            .field("tier_capacity", &self.tier_capacity)
            .finish()
    }
}

/// Helper for `Debug` — renders `tier_lru` as `{tier → len}` instead of
/// dumping the full deques (which can be long).
struct TierLruSizes<'a>(&'a MemoryManager);

impl<'a> std::fmt::Debug for TierLruSizes<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.0.tier_lru.iter().map(|(t, d)| (*t, d.len()))).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::region::Region;

    /// Build a manager with a single tier at the given capacity.
    fn mgr_with(tier: MemoryTier, cap: usize) -> MemoryManager {
        let mut caps = HashMap::new();
        caps.insert(tier, cap);
        MemoryManager::with_capacity(caps)
    }

    /// Build an `Arc<Region>` in the given tier with the given ID.
    fn region(id: u64, tier: MemoryTier) -> Arc<Region> {
        Arc::new(Region::allocate(id, tier))
    }

    /// Test: registering a region and then accessing it returns the region.
    #[test]
    fn register_and_access_region() {
        let mut mgr = MemoryManager::new();
        let r = region(0, MemoryTier::Ddr5);
        mgr.register(Arc::clone(&r));

        let accessed = mgr.access(0);
        assert!(accessed.is_some(), "registered region should be accessible");
        assert_eq!(accessed.unwrap().id, 0);
        assert!(mgr.contains(0));
    }

    /// Test: accessing an unknown region returns `None`.
    #[test]
    fn access_unknown_returns_none() {
        let mut mgr = MemoryManager::new();
        assert!(mgr.access(999).is_none());
        assert!(!mgr.contains(999));
    }

    /// Test: LRU eviction works — fill a tier to capacity, insert one more
    /// via `place_region`, and verify the oldest region is evicted.
    #[test]
    fn lru_eviction_oldest_first() {
        let mut mgr = mgr_with(MemoryTier::Ddr5, 3);

        // Fill to capacity (3 regions). After this, LRU order (front→back)
        // is [2, 1, 0] — region 0 is the least-recently-used.
        for i in 0..3u64 {
            mgr.register(region(i, MemoryTier::Ddr5));
        }
        assert_eq!(mgr.regions_in_tier(MemoryTier::Ddr5), 3);

        // Access region 0 — promote it to the front. LRU is now [0, 2, 1],
        // so region 1 is the new LRU candidate.
        mgr.access(0);

        // Place region 3 — tier is full, so the back (region 1) is evicted.
        let evicted = mgr
            .place_region(region(3, MemoryTier::Ddr5), MemoryTier::Ddr5)
            .expect("place_region should succeed");
        assert_eq!(evicted, vec![1], "LRU eviction should target region 1");

        // Region 0 (just accessed) survives; region 1 (evicted) is gone.
        assert!(mgr.access(0).is_some(), "recently-accessed region 0 should survive");
        assert!(mgr.access(1).is_none(), "evicted region 1 should be gone");
        assert!(mgr.access(3).is_some(), "newly-placed region 3 should be present");
    }

    /// Test: `place_region` into a full tier evicts exactly enough regions
    /// to make room (one eviction for one insertion).
    #[test]
    fn place_region_with_full_tier_evicts() {
        let mut mgr = mgr_with(MemoryTier::L3, 2);

        // Fill to capacity.
        mgr.register(region(10, MemoryTier::L3));
        mgr.register(region(11, MemoryTier::L3));
        assert_eq!(mgr.regions_in_tier(MemoryTier::L3), 2);

        // Place region 12 — should evict region 10 (the LRU/back).
        let evicted = mgr
            .place_region(region(12, MemoryTier::L3), MemoryTier::L3)
            .expect("place_region should succeed");
        assert_eq!(evicted, vec![10]);

        // Region 11 and 12 survive; region 10 is gone.
        assert!(mgr.access(11).is_some());
        assert!(mgr.access(12).is_some());
        assert!(mgr.access(10).is_none());
        assert_eq!(mgr.regions_in_tier(MemoryTier::L3), 2);
    }

    /// Test: `place_region` into a non-full tier evicts nothing.
    #[test]
    fn place_region_into_non_full_tier_evicts_nothing() {
        let mut mgr = mgr_with(MemoryTier::Cxl, 8);
        let evicted = mgr
            .place_region(region(0, MemoryTier::Cxl), MemoryTier::Cxl)
            .expect("place_region should succeed");
        assert!(evicted.is_empty(), "no eviction expected for a non-full tier");
        assert_eq!(mgr.regions_in_tier(MemoryTier::Cxl), 1);
    }

    /// Test: re-placing an already-registered region moves it to the new
    /// tier's LRU and removes it from the old tier's LRU.
    #[test]
    fn place_region_moves_between_tiers() {
        let mut mgr = MemoryManager::new();

        // Register region 0 in DDR5.
        mgr.register(region(0, MemoryTier::Ddr5));
        assert_eq!(mgr.regions_in_tier(MemoryTier::Ddr5), 1);
        assert_eq!(mgr.regions_in_tier(MemoryTier::Cxl), 0);

        // Re-place region 0 into CXL — should move, not duplicate.
        let evicted = mgr
            .place_region(region(0, MemoryTier::Ddr5), MemoryTier::Cxl)
            .expect("place_region should succeed");
        assert!(evicted.is_empty());
        assert_eq!(mgr.regions_in_tier(MemoryTier::Ddr5), 0, "old tier should be empty");
        assert_eq!(mgr.regions_in_tier(MemoryTier::Cxl), 1, "new tier should have the region");
        assert_eq!(mgr.total_regions(), 1, "region should not be duplicated");
    }

    /// Test: `evict_from_tier` removes the requested count from the back
    /// of the LRU.
    #[test]
    fn evict_from_tier_removes_count_oldest() {
        let mut mgr = mgr_with(MemoryTier::Ddr5, 8);

        // Register 4 regions. LRU front→back: [3, 2, 1, 0].
        for i in 0..4u64 {
            mgr.register(region(i, MemoryTier::Ddr5));
        }

        // Evict 2 — should remove 0 and 1 (the two oldest).
        let evicted = mgr.evict_from_tier(MemoryTier::Ddr5, 2);
        assert_eq!(evicted, vec![0, 1]);
        assert!(mgr.access(0).is_none());
        assert!(mgr.access(1).is_none());
        assert!(mgr.access(2).is_some());
        assert!(mgr.access(3).is_some());
        assert_eq!(mgr.regions_in_tier(MemoryTier::Ddr5), 2);
    }

    /// Test: `evict_from_tier` with `count` larger than the tier size
    /// evicts everything and returns a shorter vec.
    #[test]
    fn evict_from_tier_over_evict_is_clamped() {
        let mut mgr = mgr_with(MemoryTier::Ddr5, 8);
        mgr.register(region(0, MemoryTier::Ddr5));
        mgr.register(region(1, MemoryTier::Ddr5));

        let evicted = mgr.evict_from_tier(MemoryTier::Ddr5, 10);
        assert_eq!(evicted.len(), 2, "should evict only the 2 present regions");
        assert_eq!(mgr.regions_in_tier(MemoryTier::Ddr5), 0);
        assert_eq!(mgr.total_regions(), 0);
    }

    /// Test: the default capacities match the spec (L3=16, Ddr5=256,
    /// Cxl=1024, Nvme=unlimited).
    #[test]
    fn default_capacities_match_spec() {
        let mgr = MemoryManager::new();
        assert_eq!(mgr.tier_capacity(MemoryTier::L3), 16);
        assert_eq!(mgr.tier_capacity(MemoryTier::Ddr5), 256);
        assert_eq!(mgr.tier_capacity(MemoryTier::Cxl), 1024);
        assert_eq!(mgr.tier_capacity(MemoryTier::Nvme), usize::MAX);

        // Tiers not in the defaults are unlimited.
        assert_eq!(mgr.tier_capacity(MemoryTier::Hbm), usize::MAX);
        assert_eq!(mgr.tier_capacity(MemoryTier::L1L2), usize::MAX);
    }

    /// Test: `access` promotes the region to the front of its tier's LRU,
    /// so a subsequent placement evicts a different (older) region.
    #[test]
    fn access_promotes_to_front_of_lru() {
        let mut mgr = mgr_with(MemoryTier::Ddr5, 3);
        mgr.register(region(0, MemoryTier::Ddr5));
        mgr.register(region(1, MemoryTier::Ddr5));
        mgr.register(region(2, MemoryTier::Ddr5));
        // LRU front→back: [2, 1, 0]

        // Access region 0 — promote to front. LRU: [0, 2, 1]
        mgr.access(0);

        // The LRU order reported by the manager should reflect the move.
        let order = mgr.tier_lru_order(MemoryTier::Ddr5);
        assert_eq!(order, vec![0, 2, 1], "region 0 should be at the front after access");
    }

    /// Test: placing into an unlimited tier (NVMe) never evicts, even with
    /// many regions.
    #[test]
    fn unlimited_tier_never_evicts() {
        let mut mgr = MemoryManager::new();
        for i in 0..1000u64 {
            let evicted = mgr
                .place_region(region(i, MemoryTier::Nvme), MemoryTier::Nvme)
                .expect("place_region should succeed");
            assert!(evicted.is_empty(), "NVMe should be unlimited — no evictions");
        }
        assert_eq!(mgr.regions_in_tier(MemoryTier::Nvme), 1000);
    }

    /// Test: `Debug` formatting doesn't panic on a populated manager.
    #[test]
    fn debug_format_works() {
        let mut mgr = mgr_with(MemoryTier::Ddr5, 2);
        mgr.register(region(0, MemoryTier::Ddr5));
        mgr.register(region(1, MemoryTier::Ddr5));
        let s = format!("{mgr:?}");
        assert!(s.contains("MemoryManager"));
        assert!(s.contains("total_regions"));
    }
}
