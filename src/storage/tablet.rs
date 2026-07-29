//! A 2 GB tablet — the NUMA placement unit.
//!
//! A tablet holds 1024 regions (each 2 MB). It's the smallest structure that
//! can be pinned to a specific NUMA node or CXL device.

use crate::memory::region::Region;
use crate::memory::tier::MemoryTier;
use parking_lot::RwLock;
use std::sync::Arc;

/// Tablet ID.
pub type TabletId = u64;

/// Region size: 2 MB (matches huge page).
pub const REGION_SIZE: usize = 2 * 1024 * 1024;

/// Number of regions per tablet.
pub const TABLET_REGIONS: usize = 1024;

/// Tablet size: 2 GB.
pub const TABLET_SIZE: usize = REGION_SIZE * TABLET_REGIONS;

/// A tablet: a collection of regions, all placed in the same NUMA node / tier.
pub struct Tablet {
    /// Unique ID.
    pub id: TabletId,
    /// The regions in this tablet (may be sparse — not all slots filled).
    pub regions: RwLock<Vec<Option<Arc<Region>>>>,
    /// The tier this tablet is pinned to.
    pub tier: MemoryTier,
    /// The NUMA node this tablet is pinned to.
    pub numa_node: Option<u32>,
}

impl Tablet {
    /// Create a new empty tablet.
    pub fn new(id: TabletId, tier: MemoryTier) -> Self {
        Self { id, regions: RwLock::new(vec![None; TABLET_REGIONS]), tier, numa_node: None }
    }

    /// Place a region at a specific slot.
    pub fn put_region(&self, slot: usize, region: Arc<Region>) {
        let mut regions = self.regions.write();
        if slot < TABLET_REGIONS {
            regions[slot] = Some(region);
        }
    }

    /// Get a region by slot.
    pub fn get_region(&self, slot: usize) -> Option<Arc<Region>> {
        self.regions.read().get(slot).cloned().flatten()
    }

    /// Number of filled regions.
    pub fn filled_count(&self) -> usize {
        self.regions.read().iter().filter(|r| r.is_some()).count()
    }

    /// Total bytes used.
    pub fn bytes_used(&self) -> usize {
        self.filled_count() * REGION_SIZE
    }
}

impl std::fmt::Debug for Tablet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tablet")
            .field("id", &self.id)
            .field("tier", &self.tier)
            .field("numa_node", &self.numa_node)
            .field("filled_regions", &self.filled_count())
            .field("total_regions", &TABLET_REGIONS)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tablet_size_is_2gb() {
        assert_eq!(TABLET_SIZE, 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn tablet_region_count() {
        assert_eq!(TABLET_REGIONS, 1024);
    }

    #[test]
    fn tablet_new_is_empty() {
        let t = Tablet::new(0, MemoryTier::Ddr5);
        assert_eq!(t.filled_count(), 0);
        assert_eq!(t.bytes_used(), 0);
    }

    #[test]
    fn tablet_put_and_get_region() {
        let t = Tablet::new(0, MemoryTier::Ddr5);
        let r = Arc::new(Region::allocate(0, MemoryTier::Ddr5));
        t.put_region(5, r);
        assert_eq!(t.filled_count(), 1);
        assert!(t.get_region(5).is_some());
        assert!(t.get_region(0).is_none());
    }

    #[test]
    fn tablet_bytes_used() {
        let t = Tablet::new(0, MemoryTier::Ddr5);
        t.put_region(0, Arc::new(Region::allocate(0, MemoryTier::Ddr5)));
        t.put_region(1, Arc::new(Region::allocate(1, MemoryTier::Ddr5)));
        assert_eq!(t.bytes_used(), 2 * REGION_SIZE);
    }
}
