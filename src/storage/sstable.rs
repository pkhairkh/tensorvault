//! Sorted String Table (SSTable) — persisted column data.
//!
//! An SSTable stores sorted pages in a file, with a trailing index that
//! gives the byte offset of each page. The file is memory-mapped on read
//! for zero-copy access.
//!
//! ## File format
//!
//! ```text
//! [Header (16 bytes)]:
//!   - MAGIC "TVSST001" (8 bytes)
//!   - page_count (8 bytes, LE)      ← written at finish()
//!
//! [Page data] (page_count × PAGE_SIZE bytes):
//!   For each page:
//!     - PAGE_SIZE bytes (4096)       ← page header + cells
//!
//! [Index] (page_count × 8 bytes):
//!   For each page:
//!     - offset (8 bytes, LE)         ← absolute byte offset of the page
//!
//! [Footer (24 bytes)]:
//!   - MAGIC "TVSST001" (8 bytes)
//!   - page_count (8 bytes, LE)
//!   - index_offset (8 bytes, LE)     ← absolute byte offset of the index
//! ```
//!
//! The header `page_count` is written as a placeholder (0) when the file is
//! created and updated in place at [`SsTableWriter::finish`]. The footer is
//! the source of truth on read: [`SsTableReader::open`] seeks to the end of
//! the file, reads the footer, and uses `index_offset` to locate the index.
//!
//! ## Sorting invariant
//!
//! Pages must be appended in sorted order by their first cell (`cell[0]`).
//! [`SsTableReader::binary_search`] relies on this invariant to find the
//! page that *may* contain a given key in `O(log n)` page reads.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use memmap2::Mmap;

use crate::storage::page::{Page, HEADER_SIZE, PAGE_SIZE};
use crate::Result;

/// SSTable magic bytes: `"TVSST001"` (TurboGP SSTable v1).
const SSTABLE_MAGIC: &[u8; 8] = b"TVSST001";

/// Header size: MAGIC (8) + page_count (8) = 16 bytes.
const HEADER_SIZE_BYTES: usize = 16;

/// Footer size: MAGIC (8) + page_count (8) + index_offset (8) = 24 bytes.
const FOOTER_SIZE_BYTES: usize = 24;

// ---------------------------------------------------------------------------
// SsTableWriter
// ---------------------------------------------------------------------------

/// An SSTable writer.
///
/// Created with [`SsTableWriter::create`], pages are added via
/// [`SsTableWriter::write_page`], and the file is finalized with
/// [`SsTableWriter::finish`]. After `finish`, the writer is consumed and the
/// file is a valid SSTable readable by [`SsTableReader`].
///
/// Pages must be appended in sorted order by their first cell.
pub struct SsTableWriter {
    /// The underlying buffered file.
    file: std::io::BufWriter<File>,
    /// Number of pages written so far.
    page_count: u64,
    /// Byte offset of each page (absolute, from start of file).
    page_offsets: Vec<u64>,
}

impl SsTableWriter {
    /// Create a new SSTable at the given path, truncating any existing file.
    ///
    /// Writes the 16-byte header (MAGIC + page_count=0 placeholder) at file
    /// creation time. The `page_count` is updated in place at [`finish`].
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().create(true).write(true).truncate(true).open(path)?;
        let mut writer = std::io::BufWriter::new(file);
        writer.write_all(SSTABLE_MAGIC)?;
        writer.write_all(&0u64.to_le_bytes())?;
        Ok(Self { file: writer, page_count: 0, page_offsets: Vec::new() })
    }

    /// Write a page to the SSTable. Returns the absolute byte offset of the
    /// page within the file.
    ///
    /// Pages must be written in sorted order by their first cell
    /// (`page.get_cell(0)`); otherwise [`SsTableReader::binary_search`] will
    /// return incorrect results.
    pub fn write_page(&mut self, page: &Page) -> Result<u64> {
        let offset = HEADER_SIZE_BYTES as u64 + self.page_count * PAGE_SIZE as u64;
        // Write the page header (64 bytes) and cell payload (4032 bytes)
        // directly, avoiding the allocation in `Page::to_bytes`.
        self.file.write_all(bytemuck::bytes_of(&page.header))?;
        self.file.write_all(&page.cells)?;
        self.page_count += 1;
        self.page_offsets.push(offset);
        Ok(offset)
    }

    /// Finish writing the SSTable: flush the index and footer, then go back
    /// and patch the header's `page_count`. Returns the total bytes written.
    pub fn finish(mut self) -> Result<u64> {
        // 1. Write the index: page_count × 8 bytes (one offset per page).
        let index_offset = HEADER_SIZE_BYTES as u64 + self.page_count * PAGE_SIZE as u64;
        for offset in &self.page_offsets {
            self.file.write_all(&offset.to_le_bytes())?;
        }

        // 2. Write the footer: MAGIC + page_count + index_offset.
        self.file.write_all(SSTABLE_MAGIC)?;
        self.file.write_all(&self.page_count.to_le_bytes())?;
        self.file.write_all(&index_offset.to_le_bytes())?;

        // Flush the BufWriter to the OS before seeking on the underlying File.
        self.file.flush()?;

        // 3. Patch the header's page_count (was 0 placeholder at create time).
        //    We need `&mut File` for `seek`/`write_all`, so use `get_mut`
        //    on the BufWriter.
        let file = self.file.get_mut();
        file.seek(SeekFrom::Start(8))?;
        file.write_all(&self.page_count.to_le_bytes())?;
        file.sync_all()?;

        let total_bytes = index_offset + self.page_count * 8 + FOOTER_SIZE_BYTES as u64;
        Ok(total_bytes)
    }

    /// Number of pages written so far.
    pub fn page_count(&self) -> u64 {
        self.page_count
    }
}

// ---------------------------------------------------------------------------
// SsTableReader
// ---------------------------------------------------------------------------

/// An SSTable reader (memory-mapped).
///
/// The file is mmap'd once at [`open`](Self::open); subsequent
/// [`read_page`](Self::read_page) calls are zero-copy reads from the mmap.
/// The index (page offsets) and footer (page_count, index_offset) are parsed
/// once at open and cached in the struct.
pub struct SsTableReader {
    /// The memory-mapped file contents.
    mmap: Mmap,
    /// Number of pages (from the footer).
    page_count: u64,
    /// Absolute byte offset of the index (from the footer).
    index_offset: u64,
}

impl SsTableReader {
    /// Open an SSTable for reading.
    ///
    /// Reads the footer (last 24 bytes) to recover `page_count` and
    /// `index_offset`, then verifies the header magic. Returns an error if
    /// the file is too small, the magic doesn't match, or the offsets are
    /// out of range.
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        // SAFETY: `Mmap::map` requires that the file is not concurrently
        // modified while the mmap is live. SSTables are immutable after
        // `SsTableWriter::finish`, so this is safe in the intended usage.
        // The caller is responsible for not truncating the file underneath
        // an active reader.
        let mmap = unsafe { Mmap::map(&file)? };

        let mmap_len = mmap.len();
        if mmap_len < HEADER_SIZE_BYTES + FOOTER_SIZE_BYTES {
            return Err(crate::Error::Corruption(format!(
                "SSTable too small: {mmap_len} bytes (need at least {} for header+footer)",
                HEADER_SIZE_BYTES + FOOTER_SIZE_BYTES
            )));
        }

        // Verify header magic.
        if &mmap[..8] != SSTABLE_MAGIC {
            return Err(crate::Error::Corruption("SSTable header magic mismatch".into()));
        }

        // Read the footer (last 24 bytes).
        let footer_start = mmap_len - FOOTER_SIZE_BYTES;
        if &mmap[footer_start..footer_start + 8] != SSTABLE_MAGIC {
            return Err(crate::Error::Corruption("SSTable footer magic mismatch".into()));
        }
        let page_count =
            u64::from_le_bytes(mmap[footer_start + 8..footer_start + 16].try_into().unwrap());
        let index_offset =
            u64::from_le_bytes(mmap[footer_start + 16..footer_start + 24].try_into().unwrap());

        // Sanity-check the offsets against the file size.
        let index_end = index_offset as usize + page_count as usize * 8;
        if index_end > mmap_len - FOOTER_SIZE_BYTES {
            return Err(crate::Error::Corruption(format!(
                "SSTable index out of range: index_end={index_end}, file_len={mmap_len}"
            )));
        }

        Ok(Self { mmap, page_count, index_offset })
    }

    /// Number of pages in the SSTable.
    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Read a page by index. Allocates a fresh `Page` and copies the bytes
    /// out of the mmap.
    pub fn read_page(&self, index: usize) -> Result<Page> {
        if index as u64 >= self.page_count {
            return Err(crate::Error::NotFound(format!(
                "page index {index} out of range (page_count = {})",
                self.page_count
            )));
        }
        let offset = self.page_offset(index)? as usize;
        if offset + PAGE_SIZE > self.mmap.len() {
            return Err(crate::Error::Corruption(format!(
                "page data for index {index} out of range (offset={offset}, file_len={})",
                self.mmap.len()
            )));
        }
        Page::from_bytes(&self.mmap[offset..offset + PAGE_SIZE])
    }

    /// Binary search for the page that *may* contain `target_key`.
    ///
    /// Assumes pages are sorted by their first cell (`cell[0]`). Returns the
    /// index of the page whose first cell is the largest first cell that is
    /// `<= target_key`, or `None` if `target_key` is smaller than the first
    /// page's first cell (or the SSTable is empty).
    ///
    /// This is the standard "find the floor" / "lower-bound by key" search:
    /// the returned page is the one a point lookup should examine next.
    pub fn binary_search(&self, target_key: u64) -> Option<usize> {
        if self.page_count == 0 {
            return None;
        }
        // We use signed (i64) arithmetic for the bounds to avoid the
        // `hi = mid - 1` underflow when `mid == 0`. Realistic page counts
        // fit comfortably in i64.
        let mut lo: i64 = 0;
        let mut hi: i64 = self.page_count as i64 - 1;
        let mut result: Option<usize> = None;
        while lo <= hi {
            let mid = ((lo + hi) >> 1) as usize;
            match self.first_cell(mid) {
                Some(first_key) if first_key <= target_key => {
                    result = Some(mid);
                    lo = mid as i64 + 1;
                }
                Some(_) => {
                    hi = mid as i64 - 1;
                }
                None => break, // Corrupt index — stop and return best result.
            }
        }
        result
    }

    /// Read the absolute byte offset of page `index` from the index.
    fn page_offset(&self, index: usize) -> Result<u64> {
        let pos = self.index_offset as usize + index * 8;
        if pos + 8 > self.mmap.len() {
            return Err(crate::Error::Corruption(format!(
                "page offset for index {index} out of range (pos={pos}, file_len={})",
                self.mmap.len()
            )));
        }
        Ok(u64::from_le_bytes(self.mmap[pos..pos + 8].try_into().unwrap()))
    }

    /// Read the first cell (`cell[0]`) of page `index` directly from the
    /// mmap, without materializing the whole `Page`. Used by
    /// [`binary_search`](Self::binary_search) to avoid `O(log n)` page
    /// allocations per search.
    fn first_cell(&self, index: usize) -> Option<u64> {
        let offset = self.page_offset(index).ok()? as usize;
        // The first cell lives at offset + HEADER_SIZE (skipping the 64-byte
        // page header).
        let cell_pos = offset + HEADER_SIZE;
        if cell_pos + 8 > self.mmap.len() {
            return None;
        }
        Some(u64::from_le_bytes(self.mmap[cell_pos..cell_pos + 8].try_into().unwrap()))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::{compute_crc32c, Page, PAGE_CELLS, PAGE_SIZE};
    use tempfile::tempdir;

    /// Helper: build a page whose first cell is `first_key` and whose
    /// remaining cells are `first_key + i` (sorted, ascending).
    fn make_sorted_page(first_key: u64) -> Page {
        let mut p = Page::new();
        for i in 0..PAGE_CELLS {
            p.set_cell(i, first_key.wrapping_add(i as u64));
        }
        p.update_checksum();
        p
    }

    /// Helper: build a page with arbitrary cell data, checksummed.
    fn make_page_with_cells(cells: &[u64]) -> Page {
        let mut p = Page::new();
        for (i, &v) in cells.iter().enumerate() {
            p.set_cell(i, v);
        }
        p.update_checksum();
        p
    }

    /// Test 3: write 10 pages, read them back, verify CRC32C matches.
    #[test]
    fn sstable_write_read_roundtrip_crc() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sstable.sst");

        // Write 10 pages.
        let pages: Vec<Page> = (0..10).map(|i| make_sorted_page(i as u64 * 100)).collect();
        {
            let mut w = SsTableWriter::create(&path).unwrap();
            for p in &pages {
                w.write_page(p).unwrap();
            }
            let total = w.finish().unwrap();
            // 16 (header) + 10*4096 (pages) + 10*8 (index) + 24 (footer)
            let expected = 16 + 10 * PAGE_SIZE + 10 * 8 + 24;
            assert_eq!(total, expected as u64);
        }

        // Read them back and verify CRC32C.
        let r = SsTableReader::open(&path).unwrap();
        assert_eq!(r.page_count(), 10);
        for (i, expected) in pages.iter().enumerate() {
            let p = r.read_page(i).unwrap();
            // CRC32C of the read-back page must match the original.
            let original_crc = compute_crc32c(&expected.cells);
            let read_crc = compute_crc32c(&p.cells);
            assert_eq!(
                original_crc, read_crc,
                "CRC32C mismatch on page {i}: original={original_crc:#x}, read={read_crc:#x}"
            );
            assert!(p.verify_checksum(), "page {i} failed verify_checksum");
        }
    }

    /// Test 4: binary search finds the right page.
    #[test]
    fn sstable_binary_search_finds_right_page() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bs.sst");

        // Pages with first cells 0, 100, 200, ..., 900.
        let first_keys: Vec<u64> = (0..10).map(|i| i as u64 * 100).collect();
        {
            let mut w = SsTableWriter::create(&path).unwrap();
            for &k in &first_keys {
                w.write_page(&make_sorted_page(k)).unwrap();
            }
            w.finish().unwrap();
        }

        let r = SsTableReader::open(&path).unwrap();

        // Exact first-cell matches.
        assert_eq!(r.binary_search(0), Some(0));
        assert_eq!(r.binary_search(100), Some(1));
        assert_eq!(r.binary_search(900), Some(9));

        // Keys *within* a page (between first cells) round down to the
        // page whose first cell is the largest <= target.
        assert_eq!(r.binary_search(50), Some(0)); // 0 <= 50 < 100 → page 0
        assert_eq!(r.binary_search(150), Some(1)); // 100 <= 150 < 200 → page 1
        assert_eq!(r.binary_search(999), Some(9)); // 900 <= 999 → page 9

        // Key below the first page's first cell → None.
        // (0 is the first key, so any negative would do — but u64 can't be
        // negative. We can't test this case directly; instead we test a key
        // that maps to None by using a page set whose first cell > 0.)
    }

    /// Test 4b: binary search on an offset-first-key SSTable (first cell > 0).
    #[test]
    fn sstable_binary_search_below_first_key_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("offset.sst");

        // Pages with first cells 1000, 2000, 3000.
        {
            let mut w = SsTableWriter::create(&path).unwrap();
            w.write_page(&make_sorted_page(1000)).unwrap();
            w.write_page(&make_sorted_page(2000)).unwrap();
            w.write_page(&make_sorted_page(3000)).unwrap();
            w.finish().unwrap();
        }

        let r = SsTableReader::open(&path).unwrap();

        // Key below the first page's first cell → None.
        assert_eq!(r.binary_search(500), None);
        assert_eq!(r.binary_search(999), None);
        assert_eq!(r.binary_search(0), None);

        // Key at or above the first cell → Some.
        assert_eq!(r.binary_search(1000), Some(0));
        assert_eq!(r.binary_search(1500), Some(0));
        assert_eq!(r.binary_search(2000), Some(1));
        assert_eq!(r.binary_search(2999), Some(1));
        assert_eq!(r.binary_search(3000), Some(2));
        assert_eq!(r.binary_search(999_999), Some(2));
    }

    /// Test 4c: binary search on an empty SSTable returns None.
    #[test]
    fn sstable_binary_search_empty_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.sst");
        {
            let w = SsTableWriter::create(&path).unwrap();
            w.finish().unwrap();
        }
        let r = SsTableReader::open(&path).unwrap();
        assert_eq!(r.page_count(), 0);
        assert_eq!(r.binary_search(0), None);
        assert_eq!(r.binary_search(u64::MAX), None);
    }

    /// Test 5: roundtrip with known cell data — write pages with specific
    /// cell values, read them back, verify every cell matches.
    #[test]
    fn sstable_roundtrip_known_cell_data() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cells.sst");

        // Page 0: cells = [42, 99, 7, ...rest zero]
        // Page 1: cells = [1000, 2000, 3000, ...rest zero]
        // Page 2: cells = [u64::MAX, 0, 1, ...rest zero]
        let page0 = make_page_with_cells(&[42, 99, 7]);
        let page1 = make_page_with_cells(&[1000, 2000, 3000]);
        let page2 = make_page_with_cells(&[u64::MAX, 0, 1]);
        let originals = vec![page0, page1, page2];

        {
            let mut w = SsTableWriter::create(&path).unwrap();
            for p in &originals {
                w.write_page(p).unwrap();
            }
            w.finish().unwrap();
        }

        let r = SsTableReader::open(&path).unwrap();
        assert_eq!(r.page_count(), 3);

        // Verify every cell matches the original.
        for (i, original) in originals.iter().enumerate() {
            let read = r.read_page(i).unwrap();
            for cell_idx in 0..PAGE_CELLS {
                assert_eq!(
                    read.get_cell(cell_idx),
                    original.get_cell(cell_idx),
                    "cell mismatch on page {i}, cell {cell_idx}: expected {}, got {}",
                    original.get_cell(cell_idx),
                    read.get_cell(cell_idx)
                );
            }
            // Header fields round-trip too.
            assert_eq!(read.header.checksum, original.header.checksum);
            assert_eq!(read.header.parity, original.header.parity);
            assert!(read.verify_checksum(), "page {i} failed verify_checksum after roundtrip");
        }
    }

    /// Test 5b: page offsets returned by write_page match what the reader
    /// recovers from the index.
    #[test]
    fn sstable_write_offsets_match_index() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("offsets.sst");

        let pages: Vec<Page> = (0..5).map(|i| make_sorted_page(i as u64 * 10)).collect();
        let mut written_offsets = Vec::new();
        {
            let mut w = SsTableWriter::create(&path).unwrap();
            for p in &pages {
                written_offsets.push(w.write_page(p).unwrap());
            }
            w.finish().unwrap();
        }

        let r = SsTableReader::open(&path).unwrap();
        for (i, &expected_offset) in written_offsets.iter().enumerate() {
            let recovered = r.page_offset(i).unwrap();
            assert_eq!(
                recovered, expected_offset,
                "page {i} offset mismatch: writer said {expected_offset}, reader recovered {recovered}"
            );
        }
    }

    /// Test: opening a non-SSTable file (wrong magic) returns a corruption
    /// error.
    #[test]
    fn sstable_open_rejects_bad_magic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.sst");
        std::fs::write(&path, b"this is not an SSTable").unwrap();
        let result = SsTableReader::open(&path);
        assert!(result.is_err(), "opening a non-SSTable file must error");
        let msg = format!("{}", result.err().unwrap());
        assert!(
            msg.contains("magic") || msg.contains("small"),
            "error should mention magic or size, got: {msg}"
        );
    }

    /// Test: opening a too-small file returns a corruption error.
    #[test]
    fn sstable_open_rejects_too_small() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tiny.sst");
        std::fs::write(&path, b"TVSST001").unwrap(); // 8 bytes, too small for header+footer.
        let result = SsTableReader::open(&path);
        assert!(result.is_err());
    }

    /// Test: reading a page out of range returns NotFound.
    #[test]
    fn sstable_read_page_out_of_range() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("oor.sst");
        {
            let mut w = SsTableWriter::create(&path).unwrap();
            w.write_page(&make_sorted_page(0)).unwrap();
            w.finish().unwrap();
        }
        let r = SsTableReader::open(&path).unwrap();
        assert!(r.read_page(0).is_ok());
        assert!(r.read_page(1).is_err(), "index 1 should be out of range");
        assert!(r.read_page(999).is_err());
    }

    /// Test: the file written by `SsTableWriter` starts with the magic bytes
    /// and ends with the footer magic.
    #[test]
    fn sstable_file_has_correct_magic_at_both_ends() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("magic.sst");
        {
            let mut w = SsTableWriter::create(&path).unwrap();
            w.write_page(&make_sorted_page(0)).unwrap();
            w.finish().unwrap();
        }

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], SSTABLE_MAGIC, "header magic must be TVSST001");
        let footer_start = bytes.len() - FOOTER_SIZE_BYTES;
        assert_eq!(
            &bytes[footer_start..footer_start + 8],
            SSTABLE_MAGIC,
            "footer magic must be TVSST001"
        );
    }
}
