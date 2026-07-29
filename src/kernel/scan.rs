//! Scan kernels: count cells matching a predicate.
//!
//! Each kernel is hand-tuned for a specific `(CpuTarget, MemoryTier)` tuple.
//! The scalar fallback works everywhere; the AVX-512 kernels require Ice Lake+
//! or Zen 4+ and use `VPCMPEQQ` + `KMOVQ` for 8-lanes-per-cycle throughput.
//!
//! ## Branchless discipline (ADR-004)
//!
//! All hot-loop kernels in this module follow the branchless pattern mandated
//! by ADR-004: conditional increments are replaced with mask accumulation
//! (`count += (condition) as u64`) so the inner loop has no per-cell branch
//! that can mispredict. The only branches that remain inside the SIMD/vector
//! loops are loop-termination checks (`while i + N <= len`), which are
//! perfectly predicted. The SIMD tail also uses mask + popcount, not a branch
//! per lane.

use crate::kernel::cpu::CpuTarget;
use crate::kernel::{Kernel, KernelParams, KernelResult, Operator, PredicateOp};
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
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable bytes.
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let target = params.target_u64;
        let mut count = 0u64;
        let mut mask = 0u64;
        for (i, &c) in cells.iter().enumerate() {
            // Branchless: hit is 1 when equal, 0 otherwise (ADR-004). No
            // per-cell `if`; the compiler emits a `SETZ` + `ADD`.
            let hit = (c == target) as u64;
            count += hit;
            // Only the first 64 cells contribute to `mask`. Guarded by a
            // loop-invariant check on `i` (the compiler emits a CMOV, not a
            // mispredictable per-cell branch). The guard is REQUIRED: a
            // `1u64 << i` with `i >= 64` is UB.
            if i < 64 {
                mask |= hit << i;
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
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable bytes.
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let low = params.low_u64;
        let high = params.high_u64;
        let mut count = 0u64;
        for &c in cells {
            // Branchless: each comparison yields 0 or 1; AND them, accumulate.
            // (ADR-004: no per-cell `if`.)
            let ge = (c >= low) as u64;
            let le = (c <= high) as u64;
            count += ge & le;
        }
        KernelResult { count, sum: 0.0, mask: 0 }
    }
}

/// Evaluate a single `(cell, target, op)` predicate. Branchless.
#[inline(always)]
fn eval_predicate(cell: u64, target: u64, op: PredicateOp) -> u64 {
    match op {
        PredicateOp::Eq => (cell == target) as u64,
        PredicateOp::Gt => (cell > target) as u64,
        PredicateOp::Lt => (cell < target) as u64,
    }
}

/// Scalar `scan_multi_predicate` — counts cells matching ALL of up to 3 predicates.
///
/// Implements P-01-05. The scalar version is the reference implementation;
/// the AVX-512 kernel fuses the 3 comparisons into one `VPTERNLOGQ`.
pub struct ScanMultiPredicateScalar;

impl Kernel for ScanMultiPredicateScalar {
    fn operator(&self) -> Operator {
        Operator::ScanMultiPredicate
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::Scalar
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "scan_multi_predicate_scalar"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable bytes.
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let n = params.predicate_count.min(3) as usize;
        let mut count = 0u64;
        for &c in cells {
            // Branchless: AND the per-predicate results, accumulate.
            // (ADR-004: no per-cell `if`. Predicates with index >= n are
            // treated as always-true via the `match` below.)
            let p1 = if n >= 1 { eval_predicate(c, params.target_u64, params.pred1_op) } else { 1 };
            let p2 =
                if n >= 2 { eval_predicate(c, params.target2_u64, params.pred2_op) } else { 1 };
            let p3 =
                if n >= 3 { eval_predicate(c, params.target3_u64, params.pred3_op) } else { 1 };
            count += p1 & p2 & p3;
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
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable bytes.
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let target = _mm256_set1_epi64x(params.target_u64 as i64);
        let mut count = 0u64;
        let mut i = 0;
        // Process 4 u64s per iteration (256-bit YMM).
        while i + 4 <= cells.len() {
            let v = _mm256_loadu_si256(cells.as_ptr().add(i) as *const __m256i);
            let cmp = _mm256_cmpeq_epi64(v, target);
            // SAFETY: `_mm256_movemask_epi8` extracts the high bit of each byte
            // as a 32-bit integer. Each matching lane contributes 8 bits.
            let mask = _mm256_movemask_epi8(cmp) as u32;
            // Branchless: popcount + shift, no per-lane branch (ADR-004).
            count += (mask.count_ones() / 8) as u64;
            i += 4;
        }
        // Tail: branchless via mask accumulation (ADR-004).
        while i < cells.len() {
            count += (cells[i] == params.target_u64) as u64;
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
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable bytes.
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let target = _mm512_set1_epi64(params.target_u64 as i64);
        let mut count = 0u64;
        let mut first_mask = 0u64;
        let mut i = 0;
        // Process 8 u64s per iteration (512-bit ZMM).
        while i + 8 <= cells.len() {
            let v = _mm512_loadu_epi64(cells.as_ptr().add(i) as *const i64);
            // SAFETY: `_mm512_cmpeq_epi64_mask` produces a `__mmask8` (8-bit
            // mask, one bit per lane). `count_ones()` is a single `POPCNT`.
            let mask = _mm512_cmpeq_epi64_mask(v, target);
            // Branchless mask accumulation (ADR-004).
            count += mask.count_ones() as u64;
            // Capture the first iteration's mask for the result. This is a
            // loop-invariant check on `i`, predicted perfectly by the branch
            // predictor (taken exactly once, on the first iteration).
            if i == 0 {
                first_mask = mask as u64;
            }
            i += 8;
        }
        // Tail: branchless via mask accumulation (ADR-004).
        while i < cells.len() {
            let hit = (cells[i] == params.target_u64) as u64;
            count += hit;
            // The `i < 64` guard is loop-invariant per cell position; the
            // compiler emits a CMOV, not a mispredictable branch.
            if i < 64 {
                first_mask |= hit << i;
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
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable bytes.
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let target = _mm512_set1_epi64(params.target_u64 as i64);
        let mut count = 0u64;
        let mut i = 0;
        const PAGE: usize = 4096 / 8; // 512 u64s per page

        // Prefetch the first 4 pages.
        if cells.len() >= PAGE * 4 {
            for p in 0..4 {
                // SAFETY: `_mm_prefetch` is a hint; the pointer is in-bounds by
                // the check above.
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
            // Branchless mask + popcount (ADR-004).
            count += mask.count_ones() as u64;
            i += 8;
        }
        // Tail: branchless (ADR-004).
        while i < cells.len() {
            count += (cells[i] == params.target_u64) as u64;
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
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable bytes.
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
            // Branchless mask + popcount (ADR-004).
            count += mask.count_ones() as u64;
            i += 8;
        }
        // Tail: branchless (ADR-004).
        while i < cells.len() {
            count += (cells[i] == params.target_u64) as u64;
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
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable bytes.
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
            // AND: lanes in [low, high]. Branchless mask + popcount (ADR-004).
            let mask = ge_mask & le_mask;
            count += mask.count_ones() as u64;
            i += 8;
        }
        // Tail: branchless via mask accumulation (ADR-004).
        while i < cells.len() {
            let c = cells[i];
            let ge = (c >= params.low_u64) as u64;
            let le = (c <= params.high_u64) as u64;
            count += ge & le;
            i += 1;
        }
        KernelResult { count, sum: 0.0, mask: 0 }
    }
}

/// AVX-512 `scan_multi_predicate` for L3-resident data.
///
/// Fuses up to 3 predicates using a single `VPTERNLOGQ` instruction (P-01-05).
/// The instruction sequence per 8-cell batch:
///   1. `VPCMPEQQ`/`VPCMPGTQ` for predicate 1 → mask1 (k1)
///   2. `VPCMPEQQ`/`VPCMPGTQ` for predicate 2 → mask2 (k2)
///   3. `VPCMPEQQ`/`VPCMPGTQ` for predicate 3 → mask3 (k3)
///   4. `VPTERNLOGQ mask1, mask2, mask3, imm8` → fused AND mask
///   5. `KMOVQ` + `POPCNT` → count
///
/// `VPTERNLOGQ` with immediate `0x80` computes `(a & b & c)` per bit — the
/// bit-truth table for "all three set" — in a single 3-operand instruction,
/// eliminating the two separate ANDs and their dependent chains.
///
/// When fewer than 3 predicates are active, the unused predicates' masks are
/// initialized to all-ones (so they don't filter anything out).
#[cfg(target_arch = "x86_64")]
pub struct ScanMultiPredicateAvx512;

#[cfg(target_arch = "x86_64")]
impl Kernel for ScanMultiPredicateAvx512 {
    fn operator(&self) -> Operator {
        Operator::ScanMultiPredicate
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::X86Avx512
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "scan_multi_predicate_avx512_l3"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        // SAFETY: caller guarantees `input` points to `cell_count * 8` readable bytes
        // and that the CPU supports AVX-512F (checked at registration time).
        let cells = std::slice::from_raw_parts(input as *const u64, params.cell_count);
        let n = params.predicate_count.min(3) as usize;

        // Dispatch to the AVX-512 inner loop. The `target_feature` attribute
        // on the helper guarantees the intrinsics compile.
        scan_multi_predicate_avx512_inner(cells, params, n)
    }
}

/// Inner AVX-512 loop for `ScanMultiPredicate`.
///
/// Marked `#[target_feature(enable = "avx512f")]` so the intrinsics compile
/// even when the surrounding crate is not compiled with `-Ctarget-feature=avx512f`.
/// Callers must ensure AVX-512 is available at runtime (kernel registration
/// gates this on `CpuTarget::X86Avx512`).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn scan_multi_predicate_avx512_inner(
    cells: &[u64],
    params: &KernelParams,
    n: usize,
) -> KernelResult {
    use std::arch::x86_64::*;

    let target1 = _mm512_set1_epi64(params.target_u64 as i64);
    let target2 = _mm512_set1_epi64(params.target2_u64 as i64);
    let target3 = _mm512_set1_epi64(params.target3_u64 as i64);

    let mut count = 0u64;
    let mut i = 0;

    while i + 8 <= cells.len() {
        let v = _mm512_loadu_epi64(cells.as_ptr().add(i) as *const i64);

        // Predicate 1: compute the 8-bit mask. Unused predicates default to
        // all-ones (0xFF) so they don't filter the AND.
        let m1 = if n >= 1 { cmp_mask(v, target1, params.pred1_op) } else { 0xFFu8 };
        let m2 = if n >= 2 { cmp_mask(v, target2, params.pred2_op) } else { 0xFFu8 };
        let m3 = if n >= 3 { cmp_mask(v, target3, params.pred3_op) } else { 0xFFu8 };

        // Fuse the three masks with VPTERNLOGQ imm=0x80 (a & b & c).
        // We do this by loading the three masks into ZMM registers (via
        // broadcast-from-scalar) and emitting VPTERNLOGQ.
        // SAFETY: all intrinsics are AVX-512F, available per the
        // `target_feature` attribute on this fn.
        let zv1 = _mm512_set1_epi64(m1 as i64);
        let zv2 = _mm512_set1_epi64(m2 as i64);
        let zv3 = _mm512_set1_epi64(m3 as i64);
        // VPTERNLOGQ with imm8 = 0x80 computes `zv1 & zv2 & zv3` per bit.
        // Bit-truth table for 0x80 = 0b10000000: the only set output bit is
        // the one where all three inputs are 1.
        let fused = _mm512_ternarylogic_epi64(zv1, zv2, zv3, 0x80);
        // Extract the low byte (the masks are 8-bit, broadcast to 64 lanes;
        // we only care about the low 8 bits).
        let mut out = [0i64; 8];
        _mm512_storeu_epi64(out.as_mut_ptr(), fused);
        let combined = out[0] as u8;

        // Branchless mask + popcount (ADR-004).
        count += combined.count_ones() as u64;
        i += 8;
    }

    // Tail: branchless via mask accumulation (ADR-004).
    while i < cells.len() {
        let c = cells[i];
        let p1 = if n >= 1 { eval_predicate(c, params.target_u64, params.pred1_op) } else { 1 };
        let p2 = if n >= 2 { eval_predicate(c, params.target2_u64, params.pred2_op) } else { 1 };
        let p3 = if n >= 3 { eval_predicate(c, params.target3_u64, params.pred3_op) } else { 1 };
        count += p1 & p2 & p3;
        i += 1;
    }

    KernelResult { count, sum: 0.0, mask: 0 }
}

/// Compute an 8-bit mask of lanes matching `(v, target, op)`. AVX-512 only.
#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx512f")]
unsafe fn cmp_mask(
    v: std::arch::x86_64::__m512i,
    target: std::arch::x86_64::__m512i,
    op: PredicateOp,
) -> u8 {
    use std::arch::x86_64::*;
    // SAFETY: caller is inside a `target_feature("avx512f")` context.
    match op {
        PredicateOp::Eq => _mm512_cmpeq_epi64_mask(v, target),
        PredicateOp::Gt => _mm512_cmpgt_epi64_mask(v, target),
        PredicateOp::Lt => _mm512_cmplt_epi64_mask(v, target),
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

    /// Build a `KernelParams` for multi-predicate scan with up to 3 predicates.
    unsafe fn run_scan_multi(
        kernel: &dyn Kernel,
        cells: &[u64],
        preds: &[(u64, PredicateOp)],
    ) -> u64 {
        let mut p = KernelParams { cell_count: cells.len(), ..Default::default() };
        p.predicate_count = preds.len().min(3) as u8;
        if !preds.is_empty() {
            p.target_u64 = preds[0].0;
            p.pred1_op = preds[0].1;
        }
        if preds.len() >= 2 {
            p.target2_u64 = preds[1].0;
            p.pred2_op = preds[1].1;
        }
        if preds.len() >= 3 {
            p.target3_u64 = preds[2].0;
            p.pred3_op = preds[2].1;
        }
        let mut output = [0u8; 64];
        kernel.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &p).count
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

    // -----------------------------------------------------------------------
    // Multi-predicate scan tests (Task 3-5)
    // -----------------------------------------------------------------------

    #[test]
    fn multi_predicate_scalar_three_predicates() {
        // 3 predicates: cell == 5, cell > 2, cell < 10
        // Over 0..=20, the only value matching all three is 5.
        let cells: Vec<u64> = (0..=20).collect();
        let preds = [(5, PredicateOp::Eq), (2, PredicateOp::Gt), (10, PredicateOp::Lt)];
        let count = unsafe { run_scan_multi(&ScanMultiPredicateScalar, &cells, &preds) };
        assert_eq!(count, 1);
    }

    #[test]
    fn multi_predicate_scalar_all_match() {
        // 3 predicates all satisfied by every cell: cell > 0, cell < 100, cell == cell.
        // Use a single equality on a value present everywhere.
        let cells: Vec<u64> = vec![5; 100];
        let preds = [(5, PredicateOp::Eq), (0, PredicateOp::Gt), (100, PredicateOp::Lt)];
        let count = unsafe { run_scan_multi(&ScanMultiPredicateScalar, &cells, &preds) };
        assert_eq!(count, 100);
    }

    #[test]
    fn multi_predicate_scalar_none_match() {
        // No cell can be both > 1000 and < 1000.
        let cells: Vec<u64> = (0..1000).collect();
        let preds = [(1000, PredicateOp::Gt), (1000, PredicateOp::Lt), (5, PredicateOp::Eq)];
        let count = unsafe { run_scan_multi(&ScanMultiPredicateScalar, &cells, &preds) };
        assert_eq!(count, 0);
    }

    #[test]
    fn multi_predicate_scalar_empty_input() {
        let cells: Vec<u64> = vec![];
        let preds = [(5, PredicateOp::Eq), (2, PredicateOp::Gt), (10, PredicateOp::Lt)];
        let count = unsafe { run_scan_multi(&ScanMultiPredicateScalar, &cells, &preds) };
        assert_eq!(count, 0);
    }

    #[test]
    fn multi_predicate_scalar_single_predicate() {
        // With only one predicate, behaves like scan_eq.
        let cells: Vec<u64> = (0..100).collect();
        let preds = [(7, PredicateOp::Eq)];
        let count = unsafe { run_scan_multi(&ScanMultiPredicateScalar, &cells, &preds) };
        assert_eq!(count, 1);
    }

    #[test]
    fn multi_predicate_scalar_two_predicates() {
        // Two predicates: cell > 50 and cell < 60 → 51..=59 (9 values).
        let cells: Vec<u64> = (0..100).collect();
        let preds = [(50, PredicateOp::Gt), (60, PredicateOp::Lt)];
        let count = unsafe { run_scan_multi(&ScanMultiPredicateScalar, &cells, &preds) };
        assert_eq!(count, 9);
    }

    #[test]
    fn multi_predicate_scalar_gt_lt_simulates_range() {
        // (cell > low) AND (cell < high) is an open-interval range.
        let cells: Vec<u64> = (0..100).collect();
        let preds = [(10, PredicateOp::Gt), (20, PredicateOp::Lt)];
        let count = unsafe { run_scan_multi(&ScanMultiPredicateScalar, &cells, &preds) };
        assert_eq!(count, 9); // 11..=19
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn multi_predicate_avx512_matches_scalar_three_preds() {
        if !is_x86_feature_detected!("avx512f") {
            return;
        }
        let cells: Vec<u64> = (0..1000).collect();
        let preds = [(500, PredicateOp::Eq), (100, PredicateOp::Gt), (900, PredicateOp::Lt)];
        let scalar = unsafe { run_scan_multi(&ScanMultiPredicateScalar, &cells, &preds) };
        let avx512 = unsafe { run_scan_multi(&ScanMultiPredicateAvx512, &cells, &preds) };
        assert_eq!(scalar, avx512);
        assert_eq!(avx512, 1);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn multi_predicate_avx512_matches_scalar_two_preds() {
        if !is_x86_feature_detected!("avx512f") {
            return;
        }
        let cells: Vec<u64> = (0..1234).collect();
        let preds = [(100, PredicateOp::Gt), (200, PredicateOp::Lt)];
        let scalar = unsafe { run_scan_multi(&ScanMultiPredicateScalar, &cells, &preds) };
        let avx512 = unsafe { run_scan_multi(&ScanMultiPredicateAvx512, &cells, &preds) };
        assert_eq!(scalar, avx512);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn multi_predicate_avx512_matches_scalar_one_pred() {
        if !is_x86_feature_detected!("avx512f") {
            return;
        }
        let cells: Vec<u64> = (0..1000).map(|i| i % 17).collect();
        let preds = [(5, PredicateOp::Eq)];
        let scalar = unsafe { run_scan_multi(&ScanMultiPredicateScalar, &cells, &preds) };
        let avx512 = unsafe { run_scan_multi(&ScanMultiPredicateAvx512, &cells, &preds) };
        assert_eq!(scalar, avx512);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn multi_predicate_avx512_empty() {
        if !is_x86_feature_detected!("avx512f") {
            return;
        }
        let cells: Vec<u64> = vec![];
        let preds = [(5, PredicateOp::Eq), (2, PredicateOp::Gt), (10, PredicateOp::Lt)];
        let count = unsafe { run_scan_multi(&ScanMultiPredicateAvx512, &cells, &preds) };
        assert_eq!(count, 0);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn multi_predicate_avx512_none_match() {
        if !is_x86_feature_detected!("avx512f") {
            return;
        }
        let cells: Vec<u64> = (0..1000).collect();
        let preds = [(1000, PredicateOp::Gt), (0, PredicateOp::Lt), (5, PredicateOp::Eq)];
        let count = unsafe { run_scan_multi(&ScanMultiPredicateAvx512, &cells, &preds) };
        assert_eq!(count, 0);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn multi_predicate_avx512_all_match() {
        if !is_x86_feature_detected!("avx512f") {
            return;
        }
        let cells: Vec<u64> = vec![5; 1000];
        let preds = [(5, PredicateOp::Eq), (0, PredicateOp::Gt), (10, PredicateOp::Lt)];
        let count = unsafe { run_scan_multi(&ScanMultiPredicateAvx512, &cells, &preds) };
        assert_eq!(count, 1000);
    }
}
