//! A 2 MB region — the unit of migration between tiers.

use crate::memory::tier::MemoryTier;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Unique identifier for a region.
pub type RegionId = u64;

/// Statistics about a region's access pattern, used by the placement policy.
#[derive(Debug, Default)]
pub struct RegionStats {
    /// Total reads since creation.
    pub reads: AtomicU64,
    /// Total writes since creation.
    pub writes: AtomicU64,
    /// Last access timestamp (nanos since UNIX epoch).
    pub last_access_ns: AtomicU64,
}

impl RegionStats {
    /// Record a read.
    pub fn record_read(&self) {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.last_access_ns.store(now_ns(), Ordering::Relaxed);
    }

    /// Record a write.
    pub fn record_write(&self) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.last_access_ns.store(now_ns(), Ordering::Relaxed);
    }

    /// Total accesses (reads + writes).
    pub fn total_accesses(&self) -> u64 {
        self.reads.load(Ordering::Relaxed) + self.writes.load(Ordering::Relaxed)
    }
}

impl Clone for RegionStats {
    fn clone(&self) -> Self {
        Self {
            reads: AtomicU64::new(self.reads.load(Ordering::Relaxed)),
            writes: AtomicU64::new(self.writes.load(Ordering::Relaxed)),
            last_access_ns: AtomicU64::new(self.last_access_ns.load(Ordering::Relaxed)),
        }
    }
}

/// Region size: 2 MB (matches huge page granularity).
pub const REGION_SIZE: usize = 2 * 1024 * 1024;

/// A 2 MB region of memory, placed in a specific tier.
///
/// The region is the unit of migration. The memory manager moves whole
/// regions between tiers based on access statistics.
pub struct Region {
    /// Unique ID.
    pub id: RegionId,
    /// The raw bytes (2 MB).
    pub data: Arc<Mutex<Vec<u8>>>,
    /// Current tier.
    pub tier: MemoryTier,
    /// NUMA node (for DDR5/HBM tiers).
    pub numa_node: Option<u32>,
    /// Access statistics.
    pub stats: RegionStats,
    /// Logical column this region belongs to.
    pub column_id: Option<u64>,
    /// Row range (start, end).
    pub row_range: Option<(u64, u64)>,
}

impl Region {
    /// Allocate a new region in the given tier.
    pub fn allocate(id: RegionId, tier: MemoryTier) -> Self {
        Self {
            id,
            data: Arc::new(Mutex::new(vec![0u8; REGION_SIZE])),
            tier,
            numa_node: None,
            stats: RegionStats::default(),
            column_id: None,
            row_range: None,
        }
    }

    /// Allocate a region and fill it with the given bytes.
    pub fn from_bytes(id: RegionId, tier: MemoryTier, bytes: &[u8]) -> Self {
        let mut data = vec![0u8; REGION_SIZE];
        let copy_len = bytes.len().min(REGION_SIZE);
        data[..copy_len].copy_from_slice(&bytes[..copy_len]);
        Self {
            id,
            data: Arc::new(Mutex::new(data)),
            tier,
            numa_node: None,
            stats: RegionStats::default(),
            column_id: None,
            row_range: None,
        }
    }

    /// Read a slice of the region.
    pub fn read(&self, offset: usize, len: usize) -> Vec<u8> {
        self.stats.record_read();
        let data = self.data.lock();
        let end = (offset + len).min(data.len());
        data[offset..end].to_vec()
    }

    /// Write a slice to the region.
    pub fn write(&self, offset: usize, src: &[u8]) {
        self.stats.record_write();
        let mut data = self.data.lock();
        let end = (offset + src.len()).min(data.len());
        data[offset..end].copy_from_slice(&src[..end - offset]);
    }

    /// Read the region as u64 cells.
    pub fn as_u64_cells(&self) -> Vec<u64> {
        self.stats.record_read();
        let data = self.data.lock();
        data.chunks_exact(8)
            .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
            .collect()
    }

    /// Number of u64 cells in this region.
    pub fn cell_count(&self) -> usize {
        REGION_SIZE / 8
    }

    /// Size in bytes.
    pub fn size(&self) -> usize {
        REGION_SIZE
    }

    /// Migrate to a new tier (returns a new Region; caller handles the actual
    /// memory movement).
    pub fn migrate_to(&self, new_tier: MemoryTier) -> Self {
        let data = self.data.lock().clone();
        Self {
            id: self.id,
            data: Arc::new(Mutex::new(data)),
            tier: new_tier,
            numa_node: self.numa_node,
            stats: self.stats.clone(),
            column_id: self.column_id,
            row_range: self.row_range,
        }
    }
}

impl std::fmt::Debug for Region {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Region")
            .field("id", &self.id)
            .field("tier", &self.tier)
            .field("numa_node", &self.numa_node)
            .field("reads", &self.stats.reads.load(Ordering::Relaxed))
            .field("writes", &self.stats.writes.load(Ordering::Relaxed))
            .field("column_id", &self.column_id)
            .finish()
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_allocate_is_zeroed() {
        let r = Region::allocate(0, MemoryTier::Ddr5);
        let data = r.data.lock();
        assert_eq!(data.len(), REGION_SIZE);
        assert!(data.iter().all(|&b| b == 0));
    }

    #[test]
    fn region_write_and_read() {
        let r = Region::allocate(0, MemoryTier::Ddr5);
        r.write(100, &[1, 2, 3, 4]);
        let read = r.read(100, 4);
        assert_eq!(read, vec![1, 2, 3, 4]);
    }

    #[test]
    fn region_stats_track_accesses() {
        let r = Region::allocate(0, MemoryTier::Ddr5);
        assert_eq!(r.stats.total_accesses(), 0);
        r.read(0, 8);
        r.read(0, 8);
        r.write(0, &[1]);
        assert_eq!(r.stats.total_accesses(), 3);
        assert_eq!(r.stats.reads.load(Ordering::Relaxed), 2);
        assert_eq!(r.stats.writes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn region_as_u64_cells() {
        let r = Region::allocate(0, MemoryTier::Ddr5);
        r.write(0, &1u64.to_le_bytes());
        r.write(8, &2u64.to_le_bytes());
        let cells = r.as_u64_cells();
        assert_eq!(cells[0], 1);
        assert_eq!(cells[1], 2);
    }

    #[test]
    fn region_migrate_preserves_data() {
        let r = Region::allocate(0, MemoryTier::Ddr5);
        r.write(0, &[42; 16]);
        let r2 = r.migrate_to(MemoryTier::Cxl);
        let read = r2.read(0, 16);
        assert_eq!(read, vec![42; 16]);
        assert_eq!(r2.tier, MemoryTier::Cxl);
    }

    #[test]
    fn region_cell_count() {
        let r = Region::allocate(0, MemoryTier::Ddr5);
        assert_eq!(r.cell_count(), REGION_SIZE / 8);
        assert_eq!(r.cell_count(), 262_144); // 2 MB / 8 bytes
    }
}
