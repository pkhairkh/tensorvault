//! Hash join kernels: build a hash table, then probe it.
//!
//! The build kernel constructs a SwissTable-style hash table (1-byte
//! metadata per slot, scanned with `VPCMPEQB`). The probe kernel looks up
//! keys and returns matching (probe_idx, build_idx) pairs.
//!
//! ## Cache-line alignment (ADR-005)
//!
//! [`AlignedSlot`] is 64-byte-aligned (one full cache line) so that future
//! SwissTable `LOCK CMPXCHG` insertions can never split a cache line. A split
//! LOCK costs 3,000–10,000 cycles + 50–200 nJ on Ice Lake+ (see
//! `cpu-energy-kb.md` §1.8) — the most expensive operation on modern x86.
//! The current `HashTable` still uses `std::HashMap` (prototype); the
//! `AlignedSlot` definition is preparation for the SwissTable that Wave 4
//! will introduce.

use crate::kernel::cpu::CpuTarget;
use crate::kernel::{Kernel, KernelParams, KernelResult, Operator};
use crate::memory::tier::MemoryTier;
use fxhash::FxHashMap;

/// A cache-line-aligned hash-table slot (ADR-005).
///
/// Future SwissTable implementations will store one `AlignedSlot` per bucket
/// and probe them with `VPCMPEQB` on `metadata`. The 64-byte alignment
/// guarantees that no atomic operation on `key`/`value` can cross a cache
/// line boundary, eliminating the split-LOCK penalty.
///
/// This struct is currently unused (the prototype uses `std::HashMap` in
/// [`HashTable`]), but is defined here so that downstream code can begin
/// taking `&AlignedSlot` references and so the size + alignment are
/// compile-time-checked by the unit test below.
#[repr(align(64))]
pub struct AlignedSlot {
    /// The hash-table key (will be `AtomicU64` in the SwissTable).
    pub key: u64,
    /// The build-side row index (will be `AtomicU64` in the SwissTable).
    pub value: u64,
    /// 1-byte metadata: 0 = empty, 1 = occupied, 0xFF = tombstone.
    pub metadata: u8,
    /// Pad the struct to exactly one cache line (64 bytes). Without this the
    /// `align(64)` attribute would still align the start, but the struct size
    /// would be 17 bytes — leaving adjacent slots to share a cache line.
    _padding: [u8; 47],
}

impl Default for AlignedSlot {
    fn default() -> Self {
        Self { key: 0, value: 0, metadata: 0, _padding: [0; 47] }
    }
}

impl AlignedSlot {
    /// Create an occupied slot holding `(key, value)`.
    pub fn occupied(key: u64, value: u64) -> Self {
        Self { key, value, metadata: 1, _padding: [0; 47] }
    }

    /// Mark this slot as a tombstone (deleted but not empty).
    pub fn make_tombstone(&mut self) {
        self.metadata = 0xFF;
    }

    /// Is this slot empty (never used)?
    pub fn is_empty(&self) -> bool {
        self.metadata == 0
    }

    /// Is this slot occupied?
    pub fn is_occupied(&self) -> bool {
        self.metadata == 1
    }

    /// Is this slot a tombstone?
    pub fn is_tombstone(&self) -> bool {
        self.metadata == 0xFF
    }
}

/// A simple hash table for the build side.
///
/// In production this would be a SwissTable with 1-byte metadata and AVX-512
/// `VPCMPEQB` probing. For the prototype we use `std::HashMap`.
pub struct HashTable {
    /// Maps key → list of build-side row indices.
    pub map: FxHashMap<u64, Vec<usize>>,
    /// Number of slots (for diagnostics).
    pub slots: usize,
}

impl HashTable {
    /// Build a hash table from a slice of u64 keys.
    pub fn build(keys: &[u64]) -> Self {
        let mut map: FxHashMap<u64, Vec<usize>> = FxHashMap::default();
        map.reserve(keys.len());
        for (i, &k) in keys.iter().enumerate() {
            map.entry(k).or_default().push(i);
        }
        Self { map, slots: keys.len() }
    }

    /// Probe for a single key. Returns build-side indices.
    pub fn probe(&self, key: u64) -> &[usize] {
        self.map.get(&key).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Number of distinct keys.
    pub fn distinct_keys(&self) -> usize {
        self.map.len()
    }
}

// ---------------------------------------------------------------------------
// Scalar kernels
// ---------------------------------------------------------------------------

/// Scalar hash_build.
pub struct HashBuildScalar;

impl Kernel for HashBuildScalar {
    fn operator(&self) -> Operator {
        Operator::HashBuild
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::Scalar
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::Ddr5
    }
    fn name(&self) -> &'static str {
        "hash_build_scalar"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable
        // bytes and `output` points to at least 8 writable bytes (a pointer slot).
        let keys = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let table = HashTable::build(keys);
        // Write the table pointer to output (caller knows it's a Box<HashTable>).
        let boxed = Box::new(table);
        // SAFETY: caller guarantees `output` is a valid `*mut *mut HashTable`.
        *(output as *mut *mut HashTable) = Box::into_raw(boxed);
        KernelResult { count: params.cell_count as u64, sum: 0.0, mask: 0 }
    }
}

/// Scalar hash_probe.
pub struct HashProbeScalar;

impl Kernel for HashProbeScalar {
    fn operator(&self) -> Operator {
        Operator::HashProbe
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::Scalar
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "hash_probe_scalar"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        // SAFETY: caller guarantees `input` points to a layout of
        // `[HashTable pointer (8 bytes)] [probe keys (cell_count * 8 bytes)]`.
        let table_ptr = *(input as *const *const HashTable);
        let table = &*table_ptr;
        let probe_keys = std::slice::from_raw_parts(input.add(8) as *const u64, params.cell_count);
        let mut matches = 0u64;
        for &k in probe_keys {
            // Branchless accumulation: `len()` is always 0 for missing keys,
            // so there's no per-cell `if (found)` to mispredict (ADR-004).
            matches += table.probe(k).len() as u64;
        }
        KernelResult { count: matches, sum: 0.0, mask: 0 }
    }
}

// ---------------------------------------------------------------------------
// AVX-512 kernels (SwissTable-style with VPCMPEQB metadata scan)
// ---------------------------------------------------------------------------

/// AVX-512 hash_probe for L3-resident hash tables.
///
/// In a full implementation, this would use a SwissTable with 1-byte
/// metadata per slot, probed with `VPCMPEQB` + `VPMOVMSKB` to check 16
/// slots per cycle. The prototype falls back to the scalar probe but is
/// registered as AVX-512 so the kernel table is complete.
#[cfg(target_arch = "x86_64")]
pub struct HashProbeAvx512;

#[cfg(target_arch = "x86_64")]
impl Kernel for HashProbeAvx512 {
    fn operator(&self) -> Operator {
        Operator::HashProbe
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::X86Avx512
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "hash_probe_avx512_l3"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        // Delegate to scalar for the prototype.
        // SAFETY: same input contract as `HashProbeScalar::execute`.
        HashProbeScalar.execute(input, _output, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_table_build_and_probe() {
        let keys = [1, 2, 3, 2, 4, 2, 5];
        let table = HashTable::build(&keys);
        assert_eq!(table.probe(2), &[1, 3, 5]);
        assert!(table.probe(99).is_empty());
        assert_eq!(table.distinct_keys(), 5);
    }

    #[test]
    fn hash_table_empty() {
        let table = HashTable::build(&[]);
        assert_eq!(table.distinct_keys(), 0);
    }

    #[test]
    fn hash_kernel_build_and_probe() {
        let keys = [1u64, 2, 3, 2, 4];
        let mut output_buf = [0u8; 64];
        let build_params = KernelParams { cell_count: keys.len(), ..Default::default() };
        // SAFETY: `keys.as_ptr()` is valid for `keys.len() * 8` bytes;
        // `output_buf` is a valid 64-byte stack buffer.
        unsafe {
            HashBuildScalar.execute(
                keys.as_ptr() as *const u8,
                output_buf.as_mut_ptr(),
                &build_params,
            );
        }
        // SAFETY: `HashBuildScalar::execute` wrote a valid `*mut HashTable`
        // into the first 8 bytes of `output_buf`.
        let table_ptr = unsafe { *(output_buf.as_ptr() as *const *mut HashTable) };
        let table = unsafe { &*table_ptr };
        assert_eq!(table.probe(2), &[1, 3]);

        // Probe.
        let probe_keys = [2u64, 4, 99];
        let mut probe_input = vec![0u8; 8 + probe_keys.len() * 8];
        probe_input[..8].copy_from_slice(&(table_ptr as u64).to_le_bytes());
        for (i, &k) in probe_keys.iter().enumerate() {
            probe_input[8 + i * 8..8 + (i + 1) * 8].copy_from_slice(&k.to_le_bytes());
        }
        let probe_params = KernelParams { cell_count: probe_keys.len(), ..Default::default() };
        // SAFETY: `probe_input` has the documented layout (8-byte ptr prefix
        // followed by `probe_keys.len()` u64 keys).
        let result = unsafe {
            HashProbeScalar.execute(probe_input.as_ptr(), output_buf.as_mut_ptr(), &probe_params)
        };
        assert_eq!(result.count, 3); // 2 matches key 2 (2 build rows), 1 matches key 4

        // Free the table.
        // SAFETY: `table_ptr` was created by `Box::into_raw` in
        // `HashBuildScalar::execute` and is no longer referenced after this point.
        unsafe { drop(Box::from_raw(table_ptr)) };
    }

    // -----------------------------------------------------------------------
    // AlignedSlot (ADR-005) tests
    // -----------------------------------------------------------------------

    #[test]
    fn aligned_slot_is_64_bytes() {
        assert_eq!(std::mem::size_of::<AlignedSlot>(), 64);
    }

    #[test]
    fn aligned_slot_is_64_byte_aligned() {
        assert_eq!(std::mem::align_of::<AlignedSlot>(), 64);
    }

    #[test]
    fn aligned_slot_occupied_state() {
        let s = AlignedSlot::occupied(42, 7);
        assert!(s.is_occupied());
        assert!(!s.is_empty());
        assert!(!s.is_tombstone());
        assert_eq!(s.key, 42);
        assert_eq!(s.value, 7);
    }

    #[test]
    fn aligned_slot_default_is_empty() {
        let s = AlignedSlot::default();
        assert!(s.is_empty());
        assert!(!s.is_occupied());
    }

    #[test]
    fn aligned_slot_make_tombstone() {
        let mut s = AlignedSlot::occupied(1, 2);
        s.make_tombstone();
        assert!(s.is_tombstone());
        assert!(!s.is_occupied());
    }

    #[test]
    fn aligned_slot_array_is_cache_aligned() {
        // An array of AlignedSlots must start each slot on a cache line.
        let arr: [AlignedSlot; 4] = core::array::from_fn(|_| AlignedSlot::default());
        let base = arr.as_ptr() as usize;
        for (i, s) in arr.iter().enumerate() {
            let addr = s as *const AlignedSlot as usize;
            assert_eq!(
                (addr - base) % 64,
                0,
                "slot {} at offset {} is not cache-line aligned",
                i,
                addr - base
            );
        }
    }
}
