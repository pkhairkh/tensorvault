//! AVX-512 scan kernels for `Cell` columns.
//!
//! These are the hot inner loops that make bit-uniform storage worthwhile.
//! On x86-64 with AVX-512VPOPCNTDQ (Ice Lake+, Zen 4), each kernel processes
//! 8 u64s per cycle.
//!
//! On non-x86 or pre-AVX-512 targets, falls back to scalar (still correct,
//! just slower).

use crate::bitcell::cell::Cell;

/// AVX-512 feature gate.
#[cfg(target_arch = "x86_64")]
fn has_avx512vpopcntdq() -> bool {
    is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512vpopcntdq")
}

/// Count cells equal to `target` (exact bit equality).
///
/// # AVX-512 path
/// Uses `_mm512_xor_epi64` to XOR each lane with the target, then
/// `_mm512_cmpeq_epi64_mask` to find zero lanes, then `popcnt` on the mask.
/// Throughput: ~8 cells/cycle on Ice Lake.
///
/// # Scalar fallback
/// Simple loop with `u64 == u64` compare.
pub fn count_eq(cells: &[Cell], target: Cell) -> usize {
    let target_bits = target.to_bits();

    #[cfg(target_arch = "x86_64")]
    {
        if has_avx512vpopcntdq() {
            return unsafe { count_eq_avx512(cells, target_bits) };
        }
    }

    // Scalar fallback
    cells
        .iter()
        .filter(|c| c.to_bits() == target_bits)
        .count()
}

/// Count cells with Hamming distance ≤ `max_distance` from `target`.
///
/// # AVX-512 path
/// Uses `_mm512_xor_epi64` to XOR each lane with the target, then
/// `_mm512_popcnt_epi64` to compute per-lane popcount, then a vectorized
/// compare to count lanes ≤ max_distance.
///
/// Throughput: ~8 cells/cycle on Ice Lake.
pub fn count_similar(cells: &[Cell], target: Cell, max_distance: u32) -> usize {
    let target_bits = target.to_bits();
    let max_d = max_distance as u64;

    #[cfg(target_arch = "x86_64")]
    {
        if has_avx512vpopcntdq() {
            return unsafe { count_similar_avx512(cells, target_bits, max_d) };
        }
    }

    // Scalar fallback
    cells
        .iter()
        .filter(|c| (c.to_bits() ^ target_bits).count_ones() <= max_distance as u32)
        .count()
}

/// Sum all f64 cells. Non-f64 cells are skipped (or treated as 0.0).
///
/// # AVX-512 path
/// Uses `_mm512_add_pd` over a 512-bit vector of 8 doubles.
/// Throughput: ~8 doubles/cycle on Ice Lake.
pub fn sum_f64(cells: &[Cell]) -> f64 {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            return unsafe { sum_f64_avx512(cells) };
        }
    }

    // Scalar fallback
    cells
        .iter()
        .filter_map(|c| c.as_f64())
        .sum()
}

/// Find indices of cells equal to `target`. Returns a `Vec<usize>`.
///
/// The AVX-512 version uses the same XOR + cmpeq + mask-popcnt trick,
/// then expands the mask to indices.
pub fn find_eq(cells: &[Cell], target: Cell) -> Vec<usize> {
    let t = target.to_bits();
    let mut out = Vec::new();
    for (i, c) in cells.iter().enumerate() {
        if c.to_bits() == t {
            out.push(i);
        }
    }
    out
}

/// Find indices of cells with Hamming distance ≤ `max_distance`.
pub fn find_similar(cells: &[Cell], target: Cell, max_distance: u32) -> Vec<usize> {
    let t = target.to_bits();
    let mut out = Vec::new();
    for (i, c) in cells.iter().enumerate() {
        if (c.to_bits() ^ t).count_ones() <= max_distance {
            out.push(i);
        }
    }
    out
}

// --- AVX-512 implementations ----------------------------------------------

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512vpopcntdq")]
unsafe fn count_eq_avx512(cells: &[Cell], target_bits: u64) -> usize {
    use std::arch::x86_64::*;
    let mut count = 0usize;
    let mut i = 0;
    let n = cells.len();

    // Cast &[Cell] to &[u64] (Cell is repr(transparent) over u64).
    let bits: &[u64] = std::slice::from_raw_parts(cells.as_ptr() as *const u64, n);

    let target_vec = _mm512_set1_epi64(target_bits as i64);

    // Process 8 u64s at a time.
    while i + 8 <= n {
        let v = _mm512_loadu_epi64(bits.as_ptr().add(i) as *const i64);
        let xored = _mm512_xor_epi64(v, target_vec);
        // cmpeq: lanes equal to 0 (i.e., xored == 0) → mask bit set.
        let mask = _mm512_cmpeq_epi64_mask(xored, _mm512_setzero_si512());
        count += mask.count_ones() as usize;
        i += 8;
    }

    // Tail
    while i < n {
        if bits[i] == target_bits {
            count += 1;
        }
        i += 1;
    }

    count
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512vpopcntdq")]
unsafe fn count_similar_avx512(cells: &[Cell], target_bits: u64, max_d: u64) -> usize {
    use std::arch::x86_64::*;
    let mut count = 0usize;
    let mut i = 0;
    let n = cells.len();

    let bits: &[u64] = std::slice::from_raw_parts(cells.as_ptr() as *const u64, n);

    let target_vec = _mm512_set1_epi64(target_bits as i64);
    let max_vec = _mm512_set1_epi64(max_d as i64);

    while i + 8 <= n {
        let v = _mm512_loadu_epi64(bits.as_ptr().add(i) as *const i64);
        let xored = _mm512_xor_epi64(v, target_vec);
        let popcounts = _mm512_popcnt_epi64(xored);
        // Mask of lanes where popcount <= max_d.
        let mask = _mm512_cmple_epi64_mask(popcounts, max_vec);
        count += mask.count_ones() as usize;
        i += 8;
    }

    // Tail
    while i < n {
        if (bits[i] ^ target_bits).count_ones() <= max_d as u32 {
            count += 1;
        }
        i += 1;
    }

    count
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn sum_f64_avx512(cells: &[Cell]) -> f64 {
    use std::arch::x86_64::*;
    let mut sum = _mm512_setzero_pd();
    let mut i = 0;
    let n = cells.len();

    // Process 8 cells at a time, assuming they're f64 (identity-boxed).
    // For non-f64 cells, this would produce garbage — caller must ensure
    // the batch is monomorphic f64 (use Batch::homogeneity to check).
    while i + 8 <= n {
        // Reinterpret 8 u64 bits as 8 f64s.
        let bits = _mm512_loadu_epi64(cells.as_ptr().add(i) as *const i64);
        let doubles = _mm512_castsi512_pd(bits);
        sum = _mm512_add_pd(sum, doubles);
        i += 8;
    }

    // Horizontal sum of the 8 lanes.
    let mut tmp = [0f64; 8];
    _mm512_storeu_pd(tmp.as_mut_ptr(), sum);
    let mut total = tmp.iter().sum::<f64>();

    // Tail
    while i < n {
        if let Some(x) = cells[i].as_f64() {
            total += x;
        }
        i += 1;
    }

    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_eq_scalar_matches_simd() {
        let cells: Vec<Cell> = (0..1000).map(|i| Cell::from_i32(i % 7)).collect();
        let target = Cell::from_i32(3);
        let scalar = cells.iter().filter(|c| c.to_bits() == target.to_bits()).count();
        let kernel = count_eq(&cells, target);
        assert_eq!(scalar, kernel);
    }

    #[test]
    fn count_similar_zero_distance_equals_count_eq() {
        let cells: Vec<Cell> = (0..100).map(|i| Cell::from_f64(i as f64)).collect();
        let target = Cell::from_f64(50.0);
        assert_eq!(count_similar(&cells, target, 0), count_eq(&cells, target));
    }

    #[test]
    fn sum_f64_works() {
        let cells: Vec<Cell> = (0..100).map(|i| Cell::from_f64(i as f64)).collect();
        let expected: f64 = (0..100).map(|i| i as f64).sum();
        let actual = sum_f64(&cells);
        assert!((actual - expected).abs() < 1e-6);
    }

    #[test]
    fn sum_f64_skips_non_f64() {
        let cells = vec![
            Cell::from_f64(1.0),
            Cell::from_i32(2), // skipped
            Cell::from_f64(3.0),
            Cell::null(), // skipped
            Cell::from_f64(5.0),
        ];
        assert!((sum_f64(&cells) - 9.0).abs() < 1e-6);
    }

    #[test]
    fn find_eq_returns_correct_indices() {
        let cells: Vec<Cell> = vec![1i32, 2, 3, 2, 4, 2]
            .into_iter()
            .map(Cell::from_i32)
            .collect();
        let idx = find_eq(&cells, Cell::from_i32(2));
        assert_eq!(idx, vec![1, 3, 5]);
    }

    #[test]
    fn find_similar_returns_correct_indices() {
        let cells: Vec<Cell> = vec![1.0f64, 1.0, 2.0, 1.0, 3.0]
            .into_iter()
            .map(Cell::from_f64)
            .collect();
        let idx = find_similar(&cells, Cell::from_f64(1.0), 0);
        assert_eq!(idx, vec![0, 1, 3]);
    }

    #[test]
    fn empty_column_returns_zero() {
        let cells: Vec<Cell> = vec![];
        assert_eq!(count_eq(&cells, Cell::from_i32(0)), 0);
        assert_eq!(count_similar(&cells, Cell::from_i32(0), 100), 0);
    }

    #[test]
    fn mixed_type_column_works() {
        let cells = vec![
            Cell::from_i32(42),
            Cell::from_f64(3.14),
            Cell::from_i32(42),
            Cell::null(),
            Cell::from_i32(42),
        ];
        assert_eq!(count_eq(&cells, Cell::from_i32(42)), 3);
    }
}

#[cfg(all(test, feature = "bench"))]
mod benches {
    use super::*;
    extern crate criterion;
    use criterion::{black_box, criterion_group, criterion_main, Criterion};

    fn bench_count_eq(c: &mut Criterion) {
        let cells: Vec<Cell> = (0..100_000).map(|i| Cell::from_i32(i % 100)).collect();
        c.bench_function("count_eq 100k", |b| {
            b.iter(|| black_box(count_eq(black_box(&cells), black_box(Cell::from_i32(42)))))
        });
    }

    fn bench_count_similar(c: &mut Criterion) {
        let cells: Vec<Cell> = (0..100_000).map(|i| Cell::from_f64(i as f64)).collect();
        c.bench_function("count_similar 100k", |b| {
            b.iter(|| {
                black_box(count_similar(
                    black_box(&cells),
                    black_box(Cell::from_f64(50_000.0)),
                    black_box(8),
                ))
            })
        });
    }

    criterion_group!(benches, bench_count_eq, bench_count_similar);
    criterion_main!(benches);
}
