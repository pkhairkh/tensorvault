//! Scan kernels: count cells matching a predicate.
//!
//! Each kernel is hand-tuned for a specific `(CpuTarget, MemoryTier)` tuple.
//! The scalar fallback works everywhere; the AVX-512 kernels require Ice Lake+
//! or Zen 4+ and use `VPCMPEQQ` + `KMOVQ` for 8-lanes-per-cycle throughput.

use crate::kernel::cpu::CpuTarget;
use crate::kernel::{Kernel, KernelParams, KernelResult, Operator};
use crate::memory::tier::MemoryTier;

// ---------------------------------------------------------------------------
// Scalar fallbacks (work everywhere)
// ---------------------------------------------------------------------------

/// Scalar `scan_eq_u64` — works on any CPU, any tier.
pub struct ScanEqScalar;

impl Kernel for ScanEqScalar {
    fn operator(&self) -> Operator {
        Operator::ScanEqU64
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::Scalar
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "scan_eq_scalar"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let target = params.target_u64;
        let mut count = 0u64;
        let mut mask = 0u64;
        for (i, &c) in cells.iter().enumerate() {
            if c == target {
                count += 1;
                if i < 64 {
                    mask |= 1u64 << i;
                }
            }
        }
        KernelResult { count, sum: 0.0, mask }
    }
}

/// Scalar `scan_range_u64` — counts cells in [low, high].
pub struct ScanRangeScalar;

impl Kernel for ScanRangeScalar {
    fn operator(&self) -> Operator {
        Operator::ScanRangeU64
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::Scalar
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "scan_range_scalar"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let mut count = 0u64;
        for &c in cells {
            if c >= params.low_u64 && c <= params.high_u64 {
                count += 1;
            }
        }
        KernelResult { count, sum: 0.0, mask: 0 }
    }
}

// ---------------------------------------------------------------------------
// AVX-2 kernels (Haswell+, 2013+)
// ---------------------------------------------------------------------------

/// AVX-2 kernel for `ScanEqU64` on L3-resident data (Haswell+, 2013+).
#[cfg(target_arch = "x86_64")]
pub struct ScanEqAvx2;

#[cfg(target_arch = "x86_64")]
impl Kernel for ScanEqAvx2 {
    fn operator(&self) -> Operator {
        Operator::ScanEqU64
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::X86Avx2
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "scan_eq_avx2_l3"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        use std::arch::x86_64::*;
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let target = _mm256_set1_epi64x(params.target_u64 as i64);
        let mut count = 0u64;
        let mut i = 0;
        // Process 4 u64s per iteration (256-bit YMM).
        while i + 4 <= cells.len() {
            let v = _mm256_loadu_si256(cells.as_ptr().add(i) as *const __m256i);
            let cmp = _mm256_cmpeq_epi64(v, target);
            let mask = _mm256_movemask_epi8(cmp) as u32;
            // Each matching lane contributes 8 bits (8 bytes = 1 u64).
            count += (mask.count_ones() / 8) as u64;
            i += 4;
        }
        // Tail.
        while i < cells.len() {
            if cells[i] == params.target_u64 {
                count += 1;
            }
            i += 1;
        }
        KernelResult { count, sum: 0.0, mask: 0 }
    }
}

// ---------------------------------------------------------------------------
// AVX-512 kernels (Ice Lake+, Zen 4+)
// ---------------------------------------------------------------------------

/// AVX-512 `scan_eq_u64` for L3-resident data.
///
/// Throughput: ~19 G cells/sec on Sapphire Rapids.
/// Instruction sequence: `VMOVDQA64` + `VPCMPEQQ` + `KMOVQ`.
/// Processes 8 u64s per cycle.
#[cfg(target_arch = "x86_64")]
pub struct ScanEqAvx512L3;

#[cfg(target_arch = "x86_64")]
impl Kernel for ScanEqAvx512L3 {
    fn operator(&self) -> Operator {
        Operator::ScanEqU64
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::X86Avx512
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "scan_eq_avx512_l3"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        use std::arch::x86_64::*;
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let target = _mm512_set1_epi64(params.target_u64 as i64);
        let mut count = 0u64;
        let mut first_mask = 0u64;
        let mut i = 0;
        // Process 8 u64s per iteration (512-bit ZMM).
        while i + 8 <= cells.len() {
            let v = _mm512_loadu_epi64(cells.as_ptr().add(i) as *const i64);
            let mask = _mm512_cmpeq_epi64_mask(v, target);
            count += mask.count_ones() as u64;
            if i == 0 {
                first_mask = mask as u64;
            }
            i += 8;
        }
        // Tail.
        while i < cells.len() {
            if cells[i] == params.target_u64 {
                count += 1;
                if i < 64 {
                    first_mask |= 1u64 << i;
                }
            }
            i += 1;
        }
        KernelResult { count, sum: 0.0, mask: first_mask }
    }
}

/// AVX-512 `scan_eq_u64` for DDR5-resident data.
///
/// Throughput: ~5 G cells/sec on Sapphire Rapids.
/// Uses 4-page prefetching to hide DRAM latency.
#[cfg(target_arch = "x86_64")]
pub struct ScanEqAvx512Ddr5;

#[cfg(target_arch = "x86_64")]
impl Kernel for ScanEqAvx512Ddr5 {
    fn operator(&self) -> Operator {
        Operator::ScanEqU64
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::X86Avx512
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::Ddr5
    }
    fn name(&self) -> &'static str {
        "scan_eq_avx512_ddr5"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        use std::arch::x86_64::*;
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let target = _mm512_set1_epi64(params.target_u64 as i64);
        let mut count = 0u64;
        let mut i = 0;
        const PAGE: usize = 4096 / 8; // 512 u64s per page

        // Prefetch the first 4 pages.
        if cells.len() >= PAGE * 4 {
            for p in 0..4 {
                _mm_prefetch(cells.as_ptr().add(p * PAGE) as *const i8, _MM_HINT_T0);
            }
        }

        while i + 8 <= cells.len() {
            // Prefetch 4 pages ahead.
            if i + PAGE * 4 < cells.len() {
                _mm_prefetch(cells.as_ptr().add(i + PAGE * 4) as *const i8, _MM_HINT_T0);
            }
            let v = _mm512_loadu_epi64(cells.as_ptr().add(i) as *const i64);
            let mask = _mm512_cmpeq_epi64_mask(v, target);
            count += mask.count_ones() as u64;
            i += 8;
        }
        while i < cells.len() {
            if cells[i] == params.target_u64 {
                count += 1;
            }
            i += 1;
        }
        KernelResult { count, sum: 0.0, mask: 0 }
    }
}

/// AVX-512 `scan_eq_u64` for CXL-resident data.
///
/// Throughput: ~3 G cells/sec on Sapphire Rapids.
/// Uses 8-page prefetching and smaller batches to tolerate CXL's ~250 ns
/// latency and variable tail.
#[cfg(target_arch = "x86_64")]
pub struct ScanEqAvx512Cxl;

#[cfg(target_arch = "x86_64")]
impl Kernel for ScanEqAvx512Cxl {
    fn operator(&self) -> Operator {
        Operator::ScanEqU64
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::X86Avx512
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::Cxl
    }
    fn name(&self) -> &'static str {
        "scan_eq_avx512_cxl"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        use std::arch::x86_64::*;
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let target = _mm512_set1_epi64(params.target_u64 as i64);
        let mut count = 0u64;
        let mut i = 0;
        const PAGE: usize = 4096 / 8;

        // Prefetch the first 8 pages — CXL needs deeper prefetch.
        if cells.len() >= PAGE * 8 {
            for p in 0..8 {
                _mm_prefetch(cells.as_ptr().add(p * PAGE) as *const i8, _MM_HINT_T0);
            }
        }

        while i + 8 <= cells.len() {
            // Prefetch 8 pages ahead.
            if i + PAGE * 8 < cells.len() {
                _mm_prefetch(cells.as_ptr().add(i + PAGE * 8) as *const i8, _MM_HINT_T0);
            }
            let v = _mm512_loadu_epi64(cells.as_ptr().add(i) as *const i64);
            let mask = _mm512_cmpeq_epi64_mask(v, target);
            count += mask.count_ones() as u64;
            i += 8;
        }
        while i < cells.len() {
            if cells[i] == params.target_u64 {
                count += 1;
            }
            i += 1;
        }
        KernelResult { count, sum: 0.0, mask: 0 }
    }
}

/// AVX-512 `scan_range_u64` for L3-resident data.
#[cfg(target_arch = "x86_64")]
pub struct ScanRangeAvx512L3;

#[cfg(target_arch = "x86_64")]
impl Kernel for ScanRangeAvx512L3 {
    fn operator(&self) -> Operator {
        Operator::ScanRangeU64
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::X86Avx512
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "scan_range_avx512_l3"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        use std::arch::x86_64::*;
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let low = _mm512_set1_epi64(params.low_u64 as i64);
        let high = _mm512_set1_epi64(params.high_u64 as i64);
        let mut count = 0u64;
        let mut i = 0;
        while i + 8 <= cells.len() {
            let v = _mm512_loadu_epi64(cells.as_ptr().add(i) as *const i64);
            // >= low: mask of lanes where v >= low
            let ge_mask = _mm512_cmpge_epi64_mask(v, low);
            // <= high: mask of lanes where v <= high
            let le_mask = _mm512_cmple_epi64_mask(v, high);
            // AND: lanes in [low, high]
            let mask = ge_mask & le_mask;
            count += mask.count_ones() as u64;
            i += 8;
        }
        while i < cells.len() {
            let c = cells[i];
            if c >= params.low_u64 && c <= params.high_u64 {
                count += 1;
            }
            i += 1;
        }
        KernelResult { count, sum: 0.0, mask: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cells(values: &[u64]) -> Vec<u64> {
        values.to_vec()
    }

    unsafe fn run_scan_eq(kernel: &dyn Kernel, cells: &[u64], target: u64) -> u64 {
        let params =
            KernelParams { target_u64: target, cell_count: cells.len(), ..Default::default() };
        let mut output = [0u8; 64];
        kernel.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &params).count
    }

    #[test]
    fn scalar_scan_eq_finds_matches() {
        let cells = make_cells(&[1, 2, 3, 2, 4, 2, 5]);
        let count = unsafe { run_scan_eq(&ScanEqScalar, &cells, 2) };
        assert_eq!(count, 3);
    }

    #[test]
    fn scalar_scan_range_finds_matches() {
        let cells = make_cells(&[1, 5, 10, 15, 20, 25, 30]);
        let params = KernelParams {
            low_u64: 10,
            high_u64: 20,
            cell_count: cells.len(),
            ..Default::default()
        };
        let mut output = [0u8; 64];
        let result = unsafe {
            ScanRangeScalar.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &params)
        };
        assert_eq!(result.count, 3); // 10, 15, 20
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_scan_eq_matches_scalar() {
        if !is_x86_feature_detected!("avx2") {
            return;
        }
        let cells: Vec<u64> = (0..1000).map(|i| i % 7).collect();
        let scalar_count = unsafe { run_scan_eq(&ScanEqScalar, &cells, 3) };
        let avx2_count = unsafe { run_scan_eq(&ScanEqAvx2, &cells, 3) };
        assert_eq!(scalar_count, avx2_count);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx512_scan_eq_matches_scalar() {
        if !is_x86_feature_detected!("avx512f") {
            return;
        }
        let cells: Vec<u64> = (0..1000).map(|i| i % 7).collect();
        let scalar_count = unsafe { run_scan_eq(&ScanEqScalar, &cells, 3) };
        let avx512_count = unsafe { run_scan_eq(&ScanEqAvx512L3, &cells, 3) };
        assert_eq!(scalar_count, avx512_count);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx512_scan_eq_ddr5_matches_scalar() {
        if !is_x86_feature_detected!("avx512f") {
            return;
        }
        let cells: Vec<u64> = (0..10000).map(|i| i % 7).collect();
        let scalar_count = unsafe { run_scan_eq(&ScanEqScalar, &cells, 3) };
        let ddr5_count = unsafe { run_scan_eq(&ScanEqAvx512Ddr5, &cells, 3) };
        assert_eq!(scalar_count, ddr5_count);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx512_scan_eq_cxl_matches_scalar() {
        if !is_x86_feature_detected!("avx512f") {
            return;
        }
        let cells: Vec<u64> = (0..10000).map(|i| i % 7).collect();
        let scalar_count = unsafe { run_scan_eq(&ScanEqScalar, &cells, 3) };
        let cxl_count = unsafe { run_scan_eq(&ScanEqAvx512Cxl, &cells, 3) };
        assert_eq!(scalar_count, cxl_count);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx512_scan_range_matches_scalar() {
        if !is_x86_feature_detected!("avx512f") {
            return;
        }
        let cells: Vec<u64> = (0..1000).collect();
        let params = KernelParams {
            low_u64: 100,
            high_u64: 200,
            cell_count: cells.len(),
            ..Default::default()
        };
        let mut output = [0u8; 64];
        let scalar = unsafe {
            ScanRangeScalar.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &params)
        };
        let avx512 = unsafe {
            ScanRangeAvx512L3.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &params)
        };
        assert_eq!(scalar.count, avx512.count);
        assert_eq!(scalar.count, 101); // 100..=200 inclusive
    }

    #[test]
    fn empty_input_returns_zero() {
        let cells: Vec<u64> = vec![];
        let count = unsafe { run_scan_eq(&ScanEqScalar, &cells, 42) };
        assert_eq!(count, 0);
    }
}
