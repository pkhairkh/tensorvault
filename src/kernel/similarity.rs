//! Similarity kernels: Hamming distance.
//!
//! The Hamming distance between two u64 cells is the popcount of their XOR.
//! This works for ANY column type (because every cell is a 64-bit word) and is
//! the unified similarity primitive.

use crate::kernel::cpu::CpuTarget;
use crate::kernel::{Kernel, KernelParams, KernelResult, Operator};
use crate::memory::tier::MemoryTier;

/// Scalar Hamming similarity — counts cells within `max_distance` bits of target.
pub struct HammingScalar;

impl Kernel for HammingScalar {
    fn operator(&self) -> Operator {
        Operator::SimilarityHamming
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::Scalar
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "hamming_scalar"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let target = params.target_u64;
        let max_d = params.max_distance;
        let mut count = 0u64;
        for &c in cells {
            let dist = (c ^ target).count_ones();
            if dist <= max_d {
                count += 1;
            }
        }
        KernelResult { count, sum: 0.0, mask: 0 }
    }
}

/// AVX-512 Hamming similarity.
///
/// Uses `VPOPCNTDQ` (AVX-512_VPOPCNTDQ, Ice Lake+ / Zen 5) for vectorized
/// popcount across 8 lanes per cycle.
///
/// Throughput: ~8 G cells/sec on Sapphire Rapids with VPOPCNTDQ.
#[cfg(target_arch = "x86_64")]
pub struct HammingAvx512;

#[cfg(target_arch = "x86_64")]
impl Kernel for HammingAvx512 {
    fn operator(&self) -> Operator {
        Operator::SimilarityHamming
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::X86Avx512
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "hamming_avx512_l3"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let target = params.target_u64;
        let max_d = params.max_distance;

        // Try AVX-512 VPOPCNTDQ path if available.
        #[cfg(target_arch = "x86_64")]
        {
            if is_x86_feature_detected!("avx512vpopcntdq") {
                return hamming_avx512_vpopcntdq(cells, target, max_d);
            }
        }

        // Fallback: scalar.
        let mut count = 0u64;
        for &c in cells {
            let dist = (c ^ target).count_ones();
            if dist <= max_d {
                count += 1;
            }
        }
        KernelResult { count, sum: 0.0, mask: 0 }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512vpopcntdq")]
unsafe fn hamming_avx512_vpopcntdq(cells: &[u64], target: u64, max_d: u32) -> KernelResult {
    use std::arch::x86_64::*;
    let target_vec = _mm512_set1_epi64(target as i64);
    let max_vec = _mm512_set1_epi64(max_d as i64);
    let mut count = 0u64;
    let mut i = 0;
    while i + 8 <= cells.len() {
        let v = _mm512_loadu_epi64(cells.as_ptr().add(i) as *const i64);
        let xored = _mm512_xor_epi64(v, target_vec);
        let popcounts = _mm512_popcnt_epi64(xored);
        let mask = _mm512_cmple_epi64_mask(popcounts, max_vec);
        count += mask.count_ones() as u64;
        i += 8;
    }
    while i < cells.len() {
        let dist = (cells[i] ^ target).count_ones();
        if dist <= max_d {
            count += 1;
        }
        i += 1;
    }
    KernelResult { count, sum: 0.0, mask: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn run_hamming(kernel: &dyn Kernel, cells: &[u64], target: u64, max_d: u32) -> u64 {
        let params = KernelParams {
            target_u64: target,
            max_distance: max_d,
            cell_count: cells.len(),
            ..Default::default()
        };
        let mut output = [0u8; 64];
        kernel.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &params).count
    }

    #[test]
    fn scalar_hamming_finds_exact_matches() {
        let cells = vec![1u64, 2, 3, 1, 4, 1];
        let count = unsafe { run_hamming(&HammingScalar, &cells, 1, 0) };
        assert_eq!(count, 3);
    }

    #[test]
    fn scalar_hamming_finds_near_matches() {
        // 0b001 vs 0b011 → Hamming distance 1
        let cells = vec![0b001u64, 0b011, 0b101, 0b111, 0b000];
        let count = unsafe { run_hamming(&HammingScalar, &cells, 0b001, 1) };
        // 001 (d=0), 011 (d=1), 101 (d=1), 000 (d=1) → 4 matches within d=1
        assert_eq!(count, 4);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx512_hamming_matches_scalar() {
        if !is_x86_feature_detected!("avx512f") {
            return;
        }
        let cells: Vec<u64> = (0..1000).map(|i| i * 7).collect();
        let scalar = unsafe { run_hamming(&HammingScalar, &cells, 42, 5) };
        let avx512 = unsafe { run_hamming(&HammingAvx512, &cells, 42, 5) };
        assert_eq!(scalar, avx512);
    }

    #[test]
    fn hamming_empty_input() {
        let cells: Vec<u64> = vec![];
        let count = unsafe { run_hamming(&HammingScalar, &cells, 42, 100) };
        assert_eq!(count, 0);
    }
}
