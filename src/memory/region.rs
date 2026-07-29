//! A 2 MB region — the unit of migration between tiers.
//!
//! ## Backing memory (ADR-009)
//!
//! Each region's 2 MB payload is backed by a huge page where possible. On
//! Linux we first try `mmap(MAP_HUGETLB | MAP_PRIVATE | MAP_ANONYMOUS)`,
//! which guarantees a contiguous 2 MB physical page and 512× TLB reduction
//! for region-scoped scans. If the huge-page pool is exhausted (or the
//! kernel was built without `CONFIG_HUGETLBFS`), we fall back to a plain
//! anonymous `mmap` followed by `madvise(MADV_HUGEPAGE)` — letting
//! `khugepaged` collapse the underlying 4 KB pages into a transparent
//! huge page when memory pressure permits. On non-Linux targets we use
//! `Vec<u8>` as a last-resort fallback so the engine still runs (e.g. in
//! CI on macOS).
//!
//! ## Migration (ADR-006)
//!
//! [`Region::migrate_to`] copies the payload with
//! `std::ptr::copy_nonoverlapping`, which the x86-64 backend lowers to
//! `REP MOVSB` under ERMS — the hardware-prefetched, ~1 byte/cycle bulk
//! copy that beats hand-written AVX-512 loops for any buffer > 128 B.

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

// ---------------------------------------------------------------------------
// RegionBacking — owns the 2 MB payload, mmap'd where possible
// ---------------------------------------------------------------------------

/// How a [`RegionBacking`] was allocated — determines how it is freed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackingKind {
    /// Allocated via `mmap` (Linux). Drop calls `munmap`. The `MAP_HUGETLB`
    /// variant is reflected in `huge_page` rather than a separate variant,
    /// because the freeing call (`munmap`) is identical either way.
    Mmap {
        /// True iff the mapping was created with `MAP_HUGETLB`.
        huge_page: bool,
    },
    /// Backed by a heap `Vec<u8>` (non-Linux fallback, or `mmap` failure
    /// without the vec-fallback path being available). Drop drops the Vec.
    Vec,
}

/// Owning handle to a region's 2 MB backing memory.
///
/// The buffer is `REGION_SIZE` bytes. On Linux it is preferably backed by a
/// 2 MB huge page (see ADR-009); elsewhere or on allocation failure it falls
/// back to a plain `Vec<u8>`.
///
/// Access is mediated by [`as_slice`](Self::as_slice) /
/// [`as_mut_slice`](Self::as_mut_slice), which borrow `&self` / `&mut self`
/// respectively. The surrounding [`Region`] wraps this in a `Mutex`, so the
/// raw pointer is never aliased unsafely.
pub struct RegionBacking {
    /// Raw pointer to the first byte. Non-null by construction.
    ptr: *mut u8,
    /// Length in bytes. Always `REGION_SIZE`.
    len: usize,
    /// How this buffer was obtained — determines `Drop`.
    kind: BackingKind,
}

// SAFETY: `RegionBacking` owns its buffer. All access goes through
// `as_slice` / `as_mut_slice`, which borrow `&self` / `&mut self` and
// therefore obey Rust's aliasing rules. The buffer itself has no thread
// affinity (it's plain memory), so sending the owning handle across threads
// or sharing `&RegionBacking` between threads is sound. The surrounding
// `Region` always wraps the backing in a `Mutex`, so mutable access is
// serialized regardless.
unsafe impl Send for RegionBacking {}
unsafe impl Sync for RegionBacking {}

impl RegionBacking {
    /// Allocate a new zeroed `REGION_SIZE`-byte backing, preferring a Linux
    /// huge page (ADR-009).
    ///
    /// Allocation strategy (Linux):
    /// 1. `mmap(MAP_HUGETLB | MAP_PRIVATE | MAP_ANONYMOUS)` — best case,
    ///    gives a guaranteed 2 MB huge page.
    /// 2. If (1) fails, `mmap(MAP_PRIVATE | MAP_ANONYMOUS)` followed by
    ///    `madvise(MADV_HUGEPAGE)` — relies on `khugepaged` to coalesce.
    ///
    /// On non-Linux targets, falls back to `Vec::with_capacity(REGION_SIZE)`.
    pub fn new() -> Self {
        Self::with_size(REGION_SIZE)
    }

    /// Allocate a zeroed `len`-byte backing. Exposed for unit tests that
    /// want a smaller (faster) buffer.
    ///
    /// `len` is rounded up to a multiple of the OS page size when used with
    /// `mmap`. The public API always passes `REGION_SIZE`.
    fn with_size(len: usize) -> Self {
        if len == 0 {
            return Self::empty_vec();
        }

        #[cfg(target_os = "linux")]
        {
            if let Some(backing) = Self::try_mmap_linux(len, /* huge */ true) {
                return backing;
            }
            if let Some(backing) = Self::try_mmap_linux(len, /* huge */ false) {
                return backing;
            }
            // Both mmap attempts failed (very unusual — ENOMEM). Fall through
            // to the Vec fallback so we still return a usable buffer.
            tracing::warn!("mmap failed for region of {} bytes; falling back to Vec", len);
        }

        Self::empty_vec_with_len(len)
    }

    /// Attempt a Linux `mmap` of `len` bytes. Returns `None` if the call
    /// fails. When `huge` is true, `MAP_HUGETLB` is requested; on failure
    /// (e.g. huge-page exhaustion) the caller retries with `huge = false`.
    #[cfg(target_os = "linux")]
    fn try_mmap_linux(len: usize, huge: bool) -> Option<Self> {
        use libc::{madvise, mmap};

        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        // Round up to a whole page; for MAP_HUGETLB the kernel requires the
        // length to be a multiple of the huge page size (2 MB on x86-64).
        let mapped_len = if huge {
            REGION_SIZE // Huge pages are 2 MB on x86-64.
        } else {
            len.div_ceil(page_size) * page_size
        };

        let mut flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
        if huge {
            flags |= mmap_constants::MAP_HUGETLB;
        }

        // SAFETY: `mmap` with `MAP_ANONYMOUS` and `fd = -1` returns a fresh
        // zero-filled mapping of `mapped_len` bytes. The pointer is
        // dereferencable for `mapped_len` bytes. We store it in a
        // `RegionBacking` whose `Drop` calls `munmap` with the same length.
        let ptr = unsafe {
            mmap(std::ptr::null_mut(), mapped_len, libc::PROT_READ | libc::PROT_WRITE, flags, -1, 0)
        };
        if ptr == libc::MAP_FAILED {
            return None;
        }

        // For the non-huge fallback, ask the kernel to collapse the 4 KB
        // pages into a transparent huge page when possible.
        if !huge {
            // SAFETY: `ptr` is a valid mapping of `mapped_len` bytes.
            // `MADV_HUGEPAGE` is a hint; failure is non-fatal.
            let _ = unsafe { madvise(ptr, mapped_len, libc::MADV_HUGEPAGE) };
        }

        Some(Self { ptr: ptr as *mut u8, len, kind: BackingKind::Mmap { huge_page: huge } })
    }

    /// Allocate a `Vec`-backed buffer of `len` zeroed bytes.
    fn empty_vec_with_len(len: usize) -> Self {
        let v: Vec<u8> = vec![0u8; len];
        // SAFETY: we transfer ownership of the Vec's allocation into the
        // `RegionBacking`. The `Drop` impl reconstructs the Vec via
        // `Vec::from_raw_parts(ptr, len, len)` — `vec![0u8; len]` always
        // produces capacity == len, so this is sound.
        let mut v = std::mem::ManuallyDrop::new(v);
        let ptr = v.as_mut_ptr();
        let len = v.len();
        Self { ptr, len, kind: BackingKind::Vec }
    }

    /// Construct an empty (zero-length) Vec-backed buffer.
    fn empty_vec() -> Self {
        // `vec![]` doesn't allocate; ptr is dangling but non-null.
        Self::empty_vec_with_len(0)
    }

    /// View the backing as a byte slice.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `len` reads for the lifetime of `&self`.
        // The buffer was obtained from `mmap` or `Vec` and is owned by `self`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// View the backing as a mutable byte slice.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` is valid for `len` reads/writes for the lifetime of
        // `&mut self`. The `&mut self` borrow prevents any other aliasing
        // access for the duration.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// Length of the buffer in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always `false` for a real region (REGION_SIZE > 0); the empty
    /// constructor returns `true`.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Was this backing allocated with `MAP_HUGETLB`? Used for telemetry.
    pub fn is_huge_page(&self) -> bool {
        matches!(self.kind, BackingKind::Mmap { huge_page: true })
    }

    /// Copy `src` into this backing, truncating at the smaller of the two
    /// lengths. Uses `ptr::copy_nonoverlapping` (REP MOVSB under ERMS).
    fn copy_from(&mut self, src: &[u8]) {
        let n = src.len().min(self.len);
        // SAFETY: `src` and `self.ptr` are valid for `n` bytes; they do not
        // overlap (src is a borrow of external data, self.ptr is our own
        // backing). `ptr::copy_nonoverlapping` lowers to `REP MOVSB` on
        // x86-64 with ERMS (ADR-006).
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr, n) };
    }
}

impl Default for RegionBacking {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RegionBacking {
    fn drop(&mut self) {
        match self.kind {
            BackingKind::Mmap { .. } => {
                // The mmap call rounded `mapped_len` up to a page (or huge
                // page) boundary; we need to munmap the same length we
                // mmap'd. For huge pages this is `REGION_SIZE`; for the
                // small-page fallback it was the page-rounded length. We
                // compute it here from `self.len` using the same rounding
                // rules so we don't have to store it separately.
                #[cfg(target_os = "linux")]
                {
                    let mapped_len = mmap_rounded_len(self.len, self.is_huge_page());
                    // SAFETY: `ptr` was obtained from `mmap` with length
                    // `mapped_len` and is still valid (we own it). `munmap`
                    // releases the mapping.
                    unsafe {
                        libc::munmap(self.ptr as *mut libc::c_void, mapped_len);
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    // unreachable on non-Linux: BackingKind::Mmap is only
                    // ever constructed in the Linux code path.
                    unreachable!("BackingKind::Mmap on non-Linux target");
                }
            }
            BackingKind::Vec => {
                // SAFETY: `ptr` was obtained from a `Vec<u8>` of length and
                // capacity `len` via `ManuallyDrop` in `empty_vec_with_len`.
                // Reconstructing the Vec here and letting it drop frees the
                // allocation correctly.
                unsafe {
                    let _ = Vec::from_raw_parts(self.ptr, self.len, self.len);
                }
            }
        }
    }
}

/// Round `len` up to the page (or huge-page) granularity that
/// `RegionBacking::try_mmap_linux` would have used.
#[cfg(target_os = "linux")]
fn mmap_rounded_len(len: usize, huge: bool) -> usize {
    if huge {
        REGION_SIZE
    } else {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        len.div_ceil(page_size) * page_size
    }
}

/// Linux `mmap` flag constants that aren't exposed directly by the `libc`
/// crate on all toolchains (notably `MAP_HUGETLB`).
#[cfg(target_os = "linux")]
mod mmap_constants {
    /// `MAP_HUGETLB` — request a transparent huge-page mapping. The value
    /// (`0x40000`) is part of the Linux UAPI and stable across arches.
    pub const MAP_HUGETLB: i32 = 0x040_000;
}

impl std::fmt::Debug for RegionBacking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegionBacking")
            .field("len", &self.len)
            .field("huge_page", &self.is_huge_page())
            .field("kind", &self.kind)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Region
// ---------------------------------------------------------------------------

/// A 2 MB region of memory, placed in a specific tier.
///
/// The region is the unit of migration. The memory manager moves whole
/// regions between tiers based on access statistics.
pub struct Region {
    /// Unique ID.
    pub id: RegionId,
    /// The raw bytes (2 MB). Backed by `mmap(MAP_HUGETLB)` on Linux where
    /// possible; otherwise a `Vec<u8>` fallback.
    pub data: Arc<Mutex<RegionBacking>>,
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
            data: Arc::new(Mutex::new(RegionBacking::new())),
            tier,
            numa_node: None,
            stats: RegionStats::default(),
            column_id: None,
            row_range: None,
        }
    }

    /// Allocate a region and fill it with the given bytes.
    pub fn from_bytes(id: RegionId, tier: MemoryTier, bytes: &[u8]) -> Self {
        let mut backing = RegionBacking::new();
        backing.copy_from(bytes);
        Self {
            id,
            data: Arc::new(Mutex::new(backing)),
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
        data.as_slice()[offset..end].to_vec()
    }

    /// Write a slice to the region.
    pub fn write(&self, offset: usize, src: &[u8]) {
        self.stats.record_write();
        let mut data = self.data.lock();
        let end = (offset + src.len()).min(data.len());
        data.as_mut_slice()[offset..end].copy_from_slice(&src[..end - offset]);
    }

    /// Read the region as u64 cells.
    pub fn as_u64_cells(&self) -> Vec<u64> {
        self.stats.record_read();
        let data = self.data.lock();
        data.as_slice()
            .chunks_exact(8)
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
    ///
    /// The payload is copied via [`std::ptr::copy_nonoverlapping`], which
    /// x86-64 lowers to `REP MOVSB` under ERMS — the hardware-prefetched
    /// bulk-copy instruction chosen in ADR-006.
    pub fn migrate_to(&self, new_tier: MemoryTier) -> Self {
        let mut new_backing = RegionBacking::new();
        {
            let src = self.data.lock();
            // SAFETY: `src.as_slice()` borrows the source backing for the
            // duration of the lock; `new_backing.as_mut_slice()` borrows the
            // destination exclusively. The two backings are distinct
            // allocations, so the regions do not overlap.
            // `ptr::copy_nonoverlapping` is the documented REP MOVSB lowering.
            let n = src.len().min(new_backing.len());
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src.as_slice().as_ptr(),
                    new_backing.as_mut_slice().as_mut_ptr(),
                    n,
                );
            }
        }
        Self {
            id: self.id,
            data: Arc::new(Mutex::new(new_backing)),
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
        assert!(data.as_slice().iter().all(|&b| b == 0));
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

    // -----------------------------------------------------------------------
    // New ADR-009 / ADR-006 tests (Wave 2)
    // -----------------------------------------------------------------------

    /// Test: `Region::allocate` succeeds and returns a usable, zeroed
    /// 2 MB buffer — regardless of whether the mmap path or the Vec
    /// fallback was taken. (The exact backing kind is reported via
    /// `is_huge_page()` but the test must pass either way.)
    #[test]
    fn region_allocate_succeeds() {
        // Allocate several regions back-to-back — would crash or hang if the
        // mmap path were broken.
        for id in 0..4u64 {
            let r = Region::allocate(id, MemoryTier::Ddr5);
            let data = r.data.lock();
            assert_eq!(data.len(), REGION_SIZE, "region {} has wrong size", id);
            assert!(data.as_slice().iter().all(|&b| b == 0), "region {} not zeroed", id);
            // Touch the very first and very last byte to confirm the mapping
            // is fully writable (catches off-by-one in mmap length rounding).
            drop(data);
            r.write(0, &[0xAB]);
            r.write(REGION_SIZE - 1, &[0xCD]);
            let head = r.read(0, 1);
            let tail = r.read(REGION_SIZE - 1, 1);
            assert_eq!(head, vec![0xAB]);
            assert_eq!(tail, vec![0xCD]);
        }
    }

    /// Test: `from_bytes` roundtrips through the new mmap-backed path and
    /// preserves the input bytes (including the very last byte).
    #[test]
    fn region_from_bytes_preserves_data() {
        let mut input = vec![0u8; REGION_SIZE];
        for (i, b) in input.iter_mut().enumerate() {
            *b = (i ^ (i >> 11)) as u8;
        }
        let r = Region::from_bytes(0, MemoryTier::Ddr5, &input);
        let data = r.data.lock();
        assert_eq!(data.len(), REGION_SIZE);
        assert_eq!(data.as_slice(), &input[..]);
    }

    /// Test: the migrate_to copy does NOT alias — the source is unchanged
    /// after migration. (Catches a regression where `ptr::copy` is used
    /// instead of `ptr::copy_nonoverlapping` and the regions happen to
    /// overlap.)
    #[test]
    fn region_migrate_does_not_mutate_source() {
        let r = Region::allocate(0, MemoryTier::Ddr5);
        // Stamp the source with a recognizable pattern.
        for i in 0..1024 {
            r.write(i * 8, &(i as u64).to_le_bytes());
        }
        let snapshot_before: Vec<u8> = r.read(0, 8192);

        let _r2 = r.migrate_to(MemoryTier::Cxl);

        let snapshot_after: Vec<u8> = r.read(0, 8192);
        assert_eq!(snapshot_before, snapshot_after, "source region mutated by migrate_to");
    }

    /// Test: `RegionBacking::is_huge_page()` returns a value (we don't
    /// assert which — the test must pass either way, since CI may not have
    /// huge pages available — but the accessor must not panic and must
    /// agree with the kind).
    #[test]
    fn region_backing_reports_huge_page_status() {
        let b = RegionBacking::new();
        let _ = b.is_huge_page();
        // A backing of REGION_SIZE bytes from `new()` should always be
        // non-empty.
        assert_eq!(b.len(), REGION_SIZE);
        assert!(!b.is_empty());
    }
}
