//! A 4 KB page — the fundamental I/O unit.
//!
//! A page is 4096 bytes = 64 cache lines = 512 u64 cells. The first 64 bytes
//! (1 cache line) is the header; the remaining 4032 bytes hold 504 cells.
//!
//! The page size is chosen because:
//! - 4 KB matches the OS page size and x86 TLB granularity
//! - 4 KB = 64×64-byte cache lines
//! - Scanning a 4 KB page with `VPCMPEQQ` takes ~64 cycles, fitting in L1

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3;

/// Page size: 4096 bytes.
pub const PAGE_SIZE: usize = 4096;

/// Header size: 64 bytes (1 cache line).
pub const HEADER_SIZE: usize = 64;

/// Number of u64 cells per page: (4096 - 64) / 8 = 504.
pub const PAGE_CELLS: usize = (PAGE_SIZE - HEADER_SIZE) / 8;

/// Page header — 64 bytes, exactly one cache line.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable, Serialize, Deserialize)]
pub struct PageHeader {
    /// Page type tag (which kernel operates on this page).
    pub page_type: u64,
    /// Tier hint (which memory tier this page prefers).
    pub tier_hint: u64,
    /// Homogeneity mask (which cell tags are present).
    pub homogeneity: u64,
    /// Number of valid cells in this page.
    pub row_count: u64,
    /// xxh3 checksum of the cell data.
    pub checksum: u64,
    /// Predecessor page ID (for LSM chains).
    pub predecessor: u64,
    /// Successor page ID.
    pub successor: u64,
    /// Reserved for future use.
    pub reserved: u64,
}

impl PageHeader {
    /// Size of the header in bytes.
    pub const SIZE: usize = std::mem::size_of::<Self>();

    /// Compute the checksum of the cell data.
    pub fn compute_checksum(cells: &[u8]) -> u64 {
        xxh3::xxh3_64(cells)
    }
}

impl Default for PageHeader {
    fn default() -> Self {
        Self {
            page_type: 0,
            tier_hint: 0,
            homogeneity: 0,
            row_count: 0,
            checksum: 0,
            predecessor: 0,
            successor: 0,
            reserved: 0,
        }
    }
}

/// A 4 KB page.
#[repr(C, align(64))]
pub struct Page {
    /// The header (64 bytes).
    pub header: PageHeader,
    /// The cell data (4032 bytes = 504 u64 cells).
    pub cells: [u8; PAGE_SIZE - HEADER_SIZE],
}

impl Page {
    /// Allocate a new zeroed page.
    pub fn new() -> Self {
        Self {
            header: PageHeader::default(),
            cells: [0u8; PAGE_SIZE - HEADER_SIZE],
        }
    }

    /// Get a cell as a u64.
    pub fn get_cell(&self, index: usize) -> u64 {
        assert!(index < PAGE_CELLS, "cell index {} out of range", index);
        let offset = index * 8;
        u64::from_le_bytes(
            self.cells[offset..offset + 8]
                .try_into()
                .unwrap(),
        )
    }

    /// Set a cell as a u64.
    pub fn set_cell(&mut self, index: usize, value: u64) {
        assert!(index < PAGE_CELLS, "cell index {} out of range", index);
        let offset = index * 8;
        self.cells[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// Get the cells as a slice of u64s.
    pub fn as_u64_slice(&self) -> &[u64] {
        let ptr = self.cells.as_ptr() as *const u64;
        unsafe { std::slice::from_raw_parts(ptr, PAGE_CELLS) }
    }

    /// Get the cells as a mutable slice of u64s.
    pub fn as_u64_slice_mut(&mut self) -> &mut [u64] {
        let ptr = self.cells.as_mut_ptr() as *mut u64;
        unsafe { std::slice::from_raw_parts_mut(ptr, PAGE_CELLS) }
    }

    /// Verify the page's checksum.
    pub fn verify_checksum(&self) -> bool {
        let computed = PageHeader::compute_checksum(&self.cells);
        computed == self.header.checksum
    }

    /// Recompute and store the checksum.
    pub fn update_checksum(&mut self) {
        self.header.checksum = PageHeader::compute_checksum(&self.cells);
    }

    /// Write the page to a byte slice (for serialization).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(PAGE_SIZE);
        out.extend_from_slice(bytemuck::bytes_of(&self.header));
        out.extend_from_slice(&self.cells);
        out
    }

    /// Read a page from a byte slice.
    pub fn from_bytes(bytes: &[u8]) -> crate::Result<Self> {
        if bytes.len() < PAGE_SIZE {
            return Err(crate::Error::Corruption(format!(
                "page too small: {} bytes",
                bytes.len()
            )));
        }
        let header: PageHeader = *bytemuck::from_bytes(&bytes[..HEADER_SIZE]);
        let mut cells = [0u8; PAGE_SIZE - HEADER_SIZE];
        cells.copy_from_slice(&bytes[HEADER_SIZE..PAGE_SIZE]);
        Ok(Self { header, cells })
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_is_4kb() {
        assert_eq!(PAGE_SIZE, 4096);
    }

    #[test]
    fn page_header_is_64_bytes() {
        assert_eq!(HEADER_SIZE, 64);
        assert_eq!(PageHeader::SIZE, 64);
    }

    #[test]
    fn page_cells_is_504() {
        assert_eq!(PAGE_CELLS, 504);
        assert_eq!((PAGE_SIZE - HEADER_SIZE) / 8, 504);
    }

    #[test]
    fn page_get_set_cell() {
        let mut p = Page::new();
        p.set_cell(0, 42);
        p.set_cell(1, 0xDEADBEEF);
        p.set_cell(503, u64::MAX);
        assert_eq!(p.get_cell(0), 42);
        assert_eq!(p.get_cell(1), 0xDEADBEEF);
        assert_eq!(p.get_cell(503), u64::MAX);
    }

    #[test]
    fn page_checksum_roundtrip() {
        let mut p = Page::new();
        p.set_cell(0, 42);
        p.set_cell(1, 99);
        p.update_checksum();
        assert!(p.verify_checksum());
        // Tamper with a cell.
        p.set_cell(0, 100);
        assert!(!p.verify_checksum());
    }

    #[test]
    fn page_to_from_bytes_roundtrip() {
        let mut p = Page::new();
        p.set_cell(0, 42);
        p.set_cell(100, 12345);
        p.header.row_count = 101;
        p.update_checksum();
        let bytes = p.to_bytes();
        let p2 = Page::from_bytes(&bytes).unwrap();
        assert_eq!(p2.get_cell(0), 42);
        assert_eq!(p2.get_cell(100), 12345);
        assert_eq!(p2.header.row_count, 101);
        assert!(p2.verify_checksum());
    }

    #[test]
    fn page_as_u64_slice() {
        let mut p = Page::new();
        p.set_cell(0, 10);
        p.set_cell(1, 20);
        p.set_cell(2, 30);
        let slice = p.as_u64_slice();
        assert_eq!(slice.len(), PAGE_CELLS);
        assert_eq!(slice[0], 10);
        assert_eq!(slice[1], 20);
        assert_eq!(slice[2], 30);
    }
}
