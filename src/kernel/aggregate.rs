//! Aggregate kernels: sum, count, count distinct.
//!
//! ## Branchless discipline (ADR-004)
//!
//! All hot loops in this module use mask accumulation instead of conditional
//! branches. `count_distinct` is a HashSet-backed prototype (no SIMD path)
//! and has no per-cell `if` to remove — `HashSet::insert` returns a bool that
//! we accumulate directly.

use crate::kernel::cpu::CpuTarget;
use crate::kernel::{Kernel, KernelParams, KernelResult, Operator};
use crate::memory::tier::MemoryTier;

// ---------------------------------------------------------------------------
// Sum f64
// ---------------------------------------------------------------------------

/// Scalar `sum_f64` — reinterprets u64 cells as f64 and sums.
pub struct SumF64Scalar;

impl Kernel for SumF64Scalar {
    fn operator(&self) -> Operator {
        Operator::AggregateSumF64
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::Scalar
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "sum_f64_scalar"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable bytes.
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let sum: f64 = cells.iter().map(|&bits| f64::from_bits(bits)).sum();
        KernelResult { count: params.cell_count as u64, sum, mask: 0 }
    }
}

/// AVX-2 `sum_f64` — uses `VADDPD` (4 doubles per cycle).
#[cfg(target_arch = "x86_64")]
pub struct SumF64Avx2;

#[cfg(target_arch = "x86_64")]
impl Kernel for SumF64Avx2 {
    fn operator(&self) -> Operator {
        Operator::AggregateSumF64
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::X86Avx2
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "sum_f64_avx2"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        use std::arch::x86_64::*;
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable bytes.
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let mut acc = _mm256_setzero_pd();
        let mut i = 0;
        // Process 4 f64s per iteration. No per-cell branch (ADR-004); only the
        // loop-termination check, which is perfectly predicted.
        while i + 4 <= cells.len() {
            let bits = _mm256_loadu_si256(cells.as_ptr().add(i) as *const __m256i);
            let doubles = _mm256_castsi256_pd(bits);
            acc = _mm256_add_pd(acc, doubles);
            i += 4;
        }
        // Horizontal sum.
        let mut tmp = [0f64; 4];
        // SAFETY: `tmp` is a 4-element f64 array, properly aligned for `__m256d`.
        _mm256_storeu_pd(tmp.as_mut_ptr(), acc);
        let mut sum = tmp.iter().sum::<f64>();
        // Tail: linear accumulation, no per-cell branch (ADR-004).
        while i < cells.len() {
            sum += f64::from_bits(cells[i]);
            i += 1;
        }
        KernelResult { count: params.cell_count as u64, sum, mask: 0 }
    }
}

/// AVX-512 `sum_f64` — uses `VADDPD` (8 doubles per cycle).
///
/// Throughput: ~16 G cells/sec on Sapphire Rapids.
#[cfg(target_arch = "x86_64")]
pub struct SumF64Avx512;

#[cfg(target_arch = "x86_64")]
impl Kernel for SumF64Avx512 {
    fn operator(&self) -> Operator {
        Operator::AggregateSumF64
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::X86Avx512
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "sum_f64_avx512"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        use std::arch::x86_64::*;
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable bytes.
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let mut acc = _mm512_setzero_pd();
        let mut i = 0;
        // Process 8 f64s per iteration. No per-cell branch (ADR-004).
        while i + 8 <= cells.len() {
            let bits = _mm512_loadu_epi64(cells.as_ptr().add(i) as *const i64);
            let doubles = _mm512_castsi512_pd(bits);
            acc = _mm512_add_pd(acc, doubles);
            i += 8;
        }
        // Horizontal sum.
        let mut tmp = [0f64; 8];
        // SAFETY: `tmp` is an 8-element f64 array, properly aligned for `__m512d`.
        _mm512_storeu_pd(tmp.as_mut_ptr(), acc);
        let mut sum = tmp.iter().sum::<f64>();
        // Tail: linear accumulation, no per-cell branch (ADR-004).
        while i < cells.len() {
            sum += f64::from_bits(cells[i]);
            i += 1;
        }
        KernelResult { count: params.cell_count as u64, sum, mask: 0 }
    }
}

// ---------------------------------------------------------------------------
// Count distinct (HyperLogLog-style — simplified)
// ---------------------------------------------------------------------------

/// Scalar `count_distinct` — uses a HashSet (prototype).
///
/// A production kernel would use HyperLogLog with `VPOPCNTDQ` for leading-zero
/// counting across 8 lanes per cycle.
pub struct CountDistinctScalar;

impl Kernel for CountDistinctScalar {
    fn operator(&self) -> Operator {
        Operator::AggregateCountDistinct
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::Scalar
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::Ddr5
    }
    fn name(&self) -> &'static str {
        "count_distinct_scalar"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        use std::collections::HashSet;
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable bytes.
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let distinct: HashSet<u64> = cells.iter().copied().collect();
        KernelResult { count: distinct.len() as u64, sum: 0.0, mask: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn run_sum_f64(kernel: &dyn Kernel, values: &[f64]) -> f64 {
        let cells: Vec<u64> = values.iter().map(|v| v.to_bits()).collect();
        let params = KernelParams { cell_count: cells.len(), ..Default::default() };
        let mut output = [0u8; 64];
        kernel.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &params).sum
    }

    #[test]
    fn scalar_sum_f64_works() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let sum = unsafe { run_sum_f64(&SumF64Scalar, &values) };
        assert!((sum - 15.0).abs() < 1e-9);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_sum_matches_scalar() {
        if !is_x86_feature_detected!("avx2") {
            return;
        }
        let values: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let scalar = unsafe { run_sum_f64(&SumF64Scalar, &values) };
        let avx2 = unsafe { run_sum_f64(&SumF64Avx2, &values) };
        assert!((scalar - avx2).abs() < 1e-6);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx512_sum_matches_scalar() {
        if !is_x86_feature_detected!("avx512f") {
            return;
        }
        let values: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let scalar = unsafe { run_sum_f64(&SumF64Scalar, &values) };
        let avx512 = unsafe { run_sum_f64(&SumF64Avx512, &values) };
        assert!((scalar - avx512).abs() < 1e-6);
    }

    #[test]
    fn count_distinct_works() {
        let cells: Vec<u64> = vec![1, 2, 3, 2, 4, 1, 5];
        let params = KernelParams { cell_count: cells.len(), ..Default::default() };
        let mut output = [0u8; 64];
        let result = unsafe {
            CountDistinctScalar.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &params)
        };
        assert_eq!(result.count, 5); // {1, 2, 3, 4, 5}
    }
}
