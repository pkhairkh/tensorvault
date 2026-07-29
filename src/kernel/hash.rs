//! Hash join kernels: build a hash table, then probe it.
//!
//! The build kernel constructs a SwissTable-style hash table (1-byte
//! metadata per slot, scanned with `VPCMPEQB`). The probe kernel looks up
//! keys and returns matching (probe_idx, build_idx) pairs.

use crate::kernel::{Kernel, KernelParams, KernelResult, Operator};
use crate::kernel::cpu::CpuTarget;
use crate::memory::tier::MemoryTier;
use std::collections::HashMap;

/// A simple hash table for the build side.
///
/// In production this would be a SwissTable with 1-byte metadata and AVX-512
/// `VPCMPEQB` probing. For the prototype we use `std::HashMap`.
pub struct HashTable {
    /// Maps key → list of build-side row indices.
    pub map: HashMap<u64, Vec<usize>>,
    /// Number of slots (for diagnostics).
    pub slots: usize,
}

impl HashTable {
    /// Build a hash table from a slice of u64 keys.
    pub fn build(keys: &[u64]) -> Self {
        let mut map: HashMap<u64, Vec<usize>> = HashMap::with_capacity(keys.len());
        for (i, &k) in keys.iter().enumerate() {
            map.entry(k).or_default().push(i);
        }
        Self {
            map,
            slots: keys.len(),
        }
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
        let keys = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let table = HashTable::build(keys);
        // Write the table pointer to output (caller knows it's a Box<HashTable>).
        let boxed = Box::new(table);
        *(output as *mut *mut HashTable) = Box::into_raw(boxed);
        KernelResult {
            count: params.cell_count as u64,
            sum: 0.0,
            mask: 0,
        }
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
        // input layout: [HashTable pointer (8 bytes)] [probe keys (cell_count * 8 bytes)]
        let table_ptr = *(input as *const *const HashTable);
        let table = &*table_ptr;
        let probe_keys =
            std::slice::from_raw_parts(input.add(8) as *const u64, params.cell_count);
        let mut matches = 0u64;
        for &k in probe_keys {
            matches += table.probe(k).len() as u64;
        }
        KernelResult {
            count: matches,
            sum: 0.0,
            mask: 0,
        }
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
        HashProbeScalar.execute(input, _output, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_table_build_and_probe() {
        let keys = vec![1, 2, 3, 2, 4, 2, 5];
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
        let keys = vec![1u64, 2, 3, 2, 4];
        let mut output_buf = [0u8; 64];
        let build_params = KernelParams {
            cell_count: keys.len(),
            ..Default::default()
        };
        unsafe {
            HashBuildScalar.execute(
                keys.as_ptr() as *const u8,
                output_buf.as_mut_ptr(),
                &build_params,
            );
        }
        let table_ptr = unsafe { *(output_buf.as_ptr() as *const *mut HashTable) };
        let table = unsafe { &*table_ptr };
        assert_eq!(table.probe(2), &[1, 3]);

        // Probe.
        let probe_keys = vec![2u64, 4, 99];
        let mut probe_input = vec![0u8; 8 + probe_keys.len() * 8];
        probe_input[..8].copy_from_slice(&(table_ptr as u64).to_le_bytes());
        for (i, &k) in probe_keys.iter().enumerate() {
            probe_input[8 + i * 8..8 + (i + 1) * 8].copy_from_slice(&k.to_le_bytes());
        }
        let probe_params = KernelParams {
            cell_count: probe_keys.len(),
            ..Default::default()
        };
        let result = unsafe {
            HashProbeScalar.execute(
                probe_input.as_ptr(),
                output_buf.as_mut_ptr(),
                &probe_params,
            )
        };
        assert_eq!(result.count, 3); // 2 matches key 2 (2 build rows), 1 matches key 4

        // Free the table.
        unsafe { drop(Box::from_raw(table_ptr)) };
    }
}
