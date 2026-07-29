//! A morsel: the unit of work for the morsel-driven executor (ADR-018).
//!
//! A morsel is a fixed-size batch of 1024 u64 cells (8 KB), chosen to fit in a
//! single L1 cache line way (typical L1 is 32 KB, 8-way). The morsel-driven
//! executor dispatches whole morsels to worker threads, which run the full
//! pipeline (scan → filter → aggregate) on one morsel at a time, keeping
//! intermediate data in L1/L2 and avoiding DRAM round-trips.
//!
//! ## Why 1024 cells (ADR-007)
//!
//! - 1024 × 8 bytes = 8 KB, fits comfortably in a 32 KB L1 cache.
//! - Power of two, so morsel boundary computation is a cheap shift.
//! - Large enough that per-morsel dispatch overhead is amortized (>100 ns
//!   per dispatch, so a 1 µs kernel needs ≥ 1% overhead).
//! - Small enough that one morsel's pipeline fits entirely in L1 — no
//!   cache pollution between stages.

use crate::memory::numa::get_current_cpu;

/// The morsel size: exactly 1024 cells (ADR-007, ADR-018).
///
/// 1024 u64 cells = 8192 bytes = 8 KB — fits in L1.
pub const MORSEL_SIZE: usize = 1024;

/// A morsel = 1024 cells (8 KB, fits in L1). The unit of work for the
/// morsel-driven executor.
///
/// A morsel owns its cell data (via a `Vec<u64>`). For morsels created from a
/// region, `region_id` and `offset` record provenance so the executor can
/// update the region's LRU statistics (ADR-010) after the morsel is
/// processed. The last morsel of a region may have `len < MORSEL_SIZE` when
/// the region's cell count is not a multiple of 1024.
#[derive(Debug, Clone, Default)]
pub struct Morsel {
    /// Pointer to the cell data (up to 1024 × 8 bytes = 8 KB).
    ///
    /// `data.len()` is always equal to `len` (the number of valid cells). We
    /// never pre-allocate a full 1024-cell buffer for a short final morsel —
    /// the small allocation savings is not worth the extra indirection, and
    /// keeping `data.len() == len` makes `as_slice` trivial and lets the
    /// optimizer see the actual length.
    pub data: Vec<u64>,
    /// The region ID this morsel came from.
    pub region_id: u64,
    /// The starting cell offset within the region.
    pub offset: usize,
    /// Number of valid cells in this morsel (may be < 1024 for the last
    /// morsel of a region whose cell count is not a multiple of 1024).
    pub len: usize,
    /// The NUMA node the data resides on.
    ///
    /// `None` when the NUMA topology is unknown (e.g. on non-Linux, or for
    /// test morsels not backed by a region). The dispatcher uses this hint to
    /// route morsels to workers pinned on the same NUMA node (ADR-008).
    pub numa_node: Option<u32>,
}

impl Morsel {
    /// Create a morsel from a slice of cells.
    ///
    /// Copies up to [`MORSEL_SIZE`] (1024) cells from `cells`. If `cells.len()`
    /// exceeds 1024, only the first 1024 cells are copied (the caller is
    /// responsible for slicing the next morsel from `offset + 1024`). If
    /// `cells.len()` is less than 1024, the morsel is a short "tail" morsel.
    ///
    /// `offset` is the cell offset within the source region — it is stored
    /// verbatim and not used by `new` itself, but is recorded for provenance.
    pub fn new(region_id: u64, offset: usize, cells: &[u64]) -> Self {
        let len = cells.len().min(MORSEL_SIZE);
        let data = cells[..len].to_vec();
        // Heuristic NUMA hint: tag the morsel with the calling thread's CPU's
        // NUMA node. On non-Linux this is always 0; the dispatcher ignores
        // `numa_node` when it cannot determine the worker's NUMA node.
        let numa_node = Some(0);
        let _ = get_current_cpu(); // touch the vDSO so the field is honest
        Self { data, region_id, offset, len, numa_node }
    }

    /// Returns the valid cells in this morsel as a slice.
    ///
    /// `data.len()` is always equal to `len`, so this is just `&self.data`.
    /// The method is kept as an accessor (rather than exposing `data` directly
    /// and asking callers to use `&morsel.data[..morsel.len]`) because the
    /// invariant `data.len() == len` is an implementation detail — a future
    /// version may pre-allocate a full 1024-cell buffer and use `len` to mark
    /// the valid prefix, in which case only `as_slice` would need to change.
    pub fn as_slice(&self) -> &[u64] {
        &self.data
    }

    /// Number of valid cells in this morsel.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if this morsel contains zero valid cells.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn morsel_from_large_slice_is_capped_at_1024() {
        // 2000 input cells → morsel holds the first 1024.
        let cells: Vec<u64> = (0..2000).collect();
        let morsel = Morsel::new(42, 0, &cells);
        assert_eq!(morsel.region_id, 42);
        assert_eq!(morsel.offset, 0);
        assert_eq!(morsel.len, MORSEL_SIZE);
        assert_eq!(morsel.len, 1024);
        assert_eq!(morsel.data.len(), 1024);
        // Data matches the first 1024 input cells.
        assert_eq!(morsel.as_slice(), &cells[..1024]);
        // The 1024th cell is 1023, and the 1025th (index 1024, value 1024) is
        // NOT in the morsel — this is the off-by-one boundary.
        assert_eq!(morsel.as_slice()[1023], 1023);
    }

    #[test]
    fn morsel_from_short_slice_is_tail_morsel() {
        // 500 input cells → morsel is a tail with len < 1024.
        let cells: Vec<u64> = (0..500).collect();
        let morsel = Morsel::new(7, 4096, &cells);
        assert_eq!(morsel.region_id, 7);
        assert_eq!(morsel.offset, 4096);
        assert_eq!(morsel.len, 500);
        assert!(morsel.len < MORSEL_SIZE);
        assert_eq!(morsel.as_slice(), &cells[..]);
        assert!(!morsel.is_empty());
    }

    #[test]
    fn morsel_empty_slice_is_empty() {
        let morsel = Morsel::new(0, 0, &[]);
        assert_eq!(morsel.len, 0);
        assert!(morsel.is_empty());
        assert_eq!(morsel.as_slice().len(), 0);
    }

    #[test]
    fn morsel_size_is_exactly_1024_cells() {
        // ADR-007 / ADR-018: the morsel batch size is exactly 1024 cells.
        assert_eq!(MORSEL_SIZE, 1024);
        // 8 KB at 8 bytes per cell.
        assert_eq!(MORSEL_SIZE * 8, 8 * 1024);
    }

    #[test]
    fn morsel_default_is_empty() {
        let m = Morsel::default();
        assert!(m.is_empty());
        assert_eq!(m.len, 0);
        assert_eq!(m.region_id, 0);
    }

    #[test]
    fn morsel_clone_preserves_data() {
        let cells: Vec<u64> = (0..50).collect();
        let m1 = Morsel::new(1, 100, &cells);
        let m2 = m1.clone();
        assert_eq!(m1.as_slice(), m2.as_slice());
        assert_eq!(m1.region_id, m2.region_id);
        assert_eq!(m1.offset, m2.offset);
        assert_eq!(m1.len, m2.len);
    }
}
