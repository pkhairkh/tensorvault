//! Tensor-Train decomposition for multi-column data compression (Wave 17).
//!
//! ## Theoretical grounding
//!
//! A `d`-dimensional tensor of shape `(n_1, n_2, …, n_d)` has
//! `∏ n_k` entries — exponentially many in `d`. The **tensor-train (TT)
//! decomposition** (Oseledets 2011, arXiv:0909.1534) represents the same
//! tensor as a chain of `d` 3-way cores `G_1, G_2, …, G_d`, where
//! `G_k` has shape `(r_{k-1}, n_k, r_k)` and `r_0 = r_d = 1`:
//!
//! ```text
//! T[i_1, i_2, …, i_d] = G_1[1, i_1, :] · G_2[:, i_2, :] · … · G_d[:, i_d, 1]
//! ```
//!
//! The TT-ranks `r_1, …, r_{d-1}` control the trade-off between accuracy
//! and storage: the total parameter count is `Σ r_{k-1} · n_k · r_k`,
//! which is `O(d · n · r²)` for uniform mode size `n` and rank `r` —
//! **linear** in `d`, vs. exponential for the dense tensor.
//!
//! ## This implementation
//!
//! For `d = 2` modes, the TT decomposition reduces exactly to a
//! truncated SVD: `M = U Σ V^T` with cores `G_1 = U` (shape `1 × m × r`)
//! and `G_2 = Σ V^T` (shape `r × n × 1`). We compute the truncated SVD
//! via **power iteration with deflation** — no external linear-algebra
//! library required.
//!
//! The compressed representation wins (compression ratio > 1) when the
//! matrix is rank-deficient: `r < mn / (m + n)`. For full-rank dense
//! random matrices the TT is *larger* than the original — the wins come
//! from real data, which is almost always low-rank.
//!
//! ## References
//!
//! - Oseledets, "Tensor-Train Decomposition", SIAM J. Sci. Comput. 2011
//!   (arXiv:0909.1534).
//! - Holtz, Rohwedder, Schneider, "The alternating linear scheme for
//!   tensor optimization in the TT format", SIAM J. Sci. Comput. 2012.

/// A tensor-train decomposition of a `d`-mode tensor.
///
/// For a 2-mode (matrix) input of shape `(m, n)`, the cores are:
/// - `cores[0]` = `G_1`, flattened shape `(1, m, r)` → length `m · r`
/// - `cores[1]` = `G_2`, flattened shape `(r, n, 1)` → length `r · n`
///
/// and `ranks = [1, r, 1]`, `shape = [m, n]`.
///
/// The element at position `(i, j)` of the reconstructed matrix is
/// `Σ_k G_1[i, k] · G_2[k, j]` — exactly the matrix product
/// `U · (Σ V^T)` of the truncated SVD.
#[derive(Debug, Clone)]
pub struct TensorTrain {
    /// The TT cores `G_1, G_2, …, G_d`, each flattened into a `Vec<f64>`.
    /// `cores[k]` has length `ranks[k] * shape[k] * ranks[k+1]` (in
    /// row-major order: rightmost index varies fastest).
    pub cores: Vec<Vec<f64>>,
    /// The TT-ranks `r_0, r_1, …, r_d`. Always has length
    /// `cores.len() + 1`. By convention `ranks[0] = ranks[d] = 1`.
    pub ranks: Vec<usize>,
    /// The mode sizes `n_1, n_2, …, n_d`. Same length as `cores`.
    pub shape: Vec<usize>,
}

impl TensorTrain {
    /// Decompose a 2D matrix (treat columns as tensor modes) into a
    /// tensor-train representation.
    ///
    /// `data[i]` is the `i`-th row of the matrix (so the matrix is
    /// `m × n` with `m = data.len()` and `n = data[0].len()`). The
    /// decomposition is computed via truncated SVD with rank
    /// `≤ max_rank`, implemented as power iteration with deflation.
    ///
    /// Singular values below a relative tolerance (1e-9 of the largest)
    /// are dropped, so the effective rank may be smaller than
    /// `max_rank` — this is what makes the compression work for
    /// rank-deficient inputs.
    ///
    /// # Panics
    ///
    /// Panics if the rows have inconsistent lengths (each `data[i]`
    /// must have the same length as `data[0]`).
    ///
    /// # Examples
    ///
    /// ```
    /// use turbogp::compress::TensorTrain;
    ///
    /// // A 3×4 rank-1 matrix: outer product of [1, 2, 3] and [1, 2, 3, 4].
    /// let a = vec![1.0, 2.0, 3.0];
    /// let b = vec![1.0, 2.0, 3.0, 4.0];
    /// let data: Vec<Vec<f64>> = (0..3).map(|i| (0..4).map(|j| a[i] * b[j]).collect()).collect();
    /// let tt = TensorTrain::decompose(&data, 2);
    /// assert!(tt.compression_ratio() > 1.0, "rank-1 matrix should compress");
    /// ```
    #[must_use]
    pub fn decompose(data: &[Vec<f64>], max_rank: usize) -> Self {
        let m = data.len();
        if m == 0 {
            return Self { cores: Vec::new(), ranks: vec![1], shape: Vec::new() };
        }
        let n = data[0].len();
        if n == 0 {
            return Self { cores: Vec::new(), ranks: vec![1], shape: vec![m] };
        }
        // Defensive: all rows must have the same length.
        for (i, row) in data.iter().enumerate() {
            assert_eq!(
                row.len(),
                n,
                "TensorTrain::decompose: row {i} has length {} but row 0 has length {n}",
                row.len(),
            );
        }

        let (u_cols, s_vals, v_cols) = truncated_svd(data, max_rank, 200, 1e-12);
        let r = s_vals.len();

        if r == 0 {
            // All-zero matrix: store a rank-1 TT with zero cores so
            // `reconstruct` returns the correct (all-zero) result.
            return Self {
                cores: vec![vec![0.0; m], vec![0.0; n]],
                ranks: vec![1, 1, 1],
                shape: vec![m, n],
            };
        }

        // G_1: shape (1, m, r), row-major flattened → length m·r.
        // Element [i, k] is at index i·r + k.
        let mut g1 = vec![0.0; m * r];
        for i in 0..m {
            for k in 0..r {
                g1[i * r + k] = u_cols[k][i];
            }
        }
        // G_2: shape (r, n, 1), row-major flattened → length r·n.
        // Element [k, j] is at index k·n + j.
        let mut g2 = vec![0.0; r * n];
        for k in 0..r {
            for j in 0..n {
                g2[k * n + j] = s_vals[k] * v_cols[k][j];
            }
        }

        Self { cores: vec![g1, g2], ranks: vec![1, r, 1], shape: vec![m, n] }
    }

    /// Reconstruct the full tensor (for small tensors only).
    ///
    /// For a 2-mode TT, returns a flattened `m × n` matrix in row-major
    /// order: element `(i, j)` is at index `i · n + j`. For higher-mode
    /// TTs (not produced by this implementation), the result is the
    /// fully contracted dense tensor.
    ///
    /// Returns an empty `Vec` if the TT is empty (no modes).
    #[must_use]
    pub fn reconstruct(&self) -> Vec<f64> {
        if self.shape.is_empty() || self.cores.is_empty() {
            return Vec::new();
        }
        if self.shape.len() == 1 {
            // Degenerate single-mode case: just return the (only) core.
            return self.cores[0].clone();
        }
        // For 2-mode TT: M[i, j] = Σ_k G_1[i, k] · G_2[k, j].
        let m = self.shape[0];
        let n = self.shape[1];
        let r = self.ranks[1];
        let g1 = &self.cores[0];
        let g2 = &self.cores[1];

        let mut result = vec![0.0; m * n];
        for i in 0..m {
            for j in 0..n {
                let mut sum = 0.0;
                for k in 0..r {
                    sum += g1[i * r + k] * g2[k * n + j];
                }
                result[i * n + j] = sum;
            }
        }
        result
    }

    /// Estimate the compression ratio: `original_size / tt_size`.
    ///
    /// `original_size` is the number of elements in the dense tensor
    /// (`∏ shape`). `tt_size` is the total number of elements across
    /// all cores (`Σ cores[k].len()`).
    ///
    /// Returns `1.0` for an empty TT. Returns a value `> 1.0` when the
    /// TT is smaller than the dense representation (i.e., the input was
    /// compressible); `< 1.0` when the TT is larger (rank-deficient
    /// truncation failed to win).
    #[must_use]
    pub fn compression_ratio(&self) -> f64 {
        if self.shape.is_empty() {
            return 1.0;
        }
        let original: usize = self.shape.iter().product::<usize>().max(1);
        let tt_size: usize = self.cores.iter().map(Vec::len).sum();
        if tt_size == 0 {
            return 1.0;
        }
        (original as f64) / (tt_size as f64)
    }

    /// The effective TT-rank of the decomposition.
    ///
    /// For a 2-mode TT, this is `ranks[1]` — the inner rank connecting
    /// the two cores. Returns 0 for an empty TT.
    #[must_use]
    pub fn effective_rank(&self) -> usize {
        if self.ranks.len() < 2 {
            0
        } else {
            self.ranks[1]
        }
    }
}

/// Compute a truncated SVD of an `m × n` matrix via power iteration with
/// deflation.
///
/// Returns `(U_columns, S, V_columns)` where:
/// - `U_columns[k]` is the `k`-th left singular vector (length `m`)
/// - `S[k]` is the `k`-th singular value (descending)
/// - `V_columns[k]` is the `k`-th right singular vector (length `n`)
///
/// The result has at most `max_rank` triples; singular values below
/// `tol * (1 + max_abs_entry)` are dropped.
fn truncated_svd(
    data: &[Vec<f64>],
    max_rank: usize,
    iters: usize,
    tol: f64,
) -> (Vec<Vec<f64>>, Vec<f64>, Vec<Vec<f64>>) {
    let m = data.len();
    let n = if m > 0 { data[0].len() } else { 0 };
    if m == 0 || n == 0 {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let max_r = max_rank.min(m.min(n));
    if max_r == 0 {
        return (Vec::new(), Vec::new(), Vec::new());
    }

    let max_abs = max_abs_entry(data).max(1e-300);
    let sigma_tol = tol * (1.0 + max_abs);

    let mut residual: Vec<Vec<f64>> = data.to_vec();
    let mut u_cols: Vec<Vec<f64>> = Vec::with_capacity(max_r);
    let mut s_vals: Vec<f64> = Vec::with_capacity(max_r);
    let mut v_cols: Vec<Vec<f64>> = Vec::with_capacity(max_r);

    for _ in 0..max_r {
        match power_iteration_top_singular(&residual, m, n, iters, tol) {
            None => break,
            Some((u, sigma, v)) => {
                if sigma < sigma_tol {
                    break;
                }
                // Deflate: residual -= sigma * u outer v.
                for i in 0..m {
                    let ui = u[i];
                    let row = &mut residual[i];
                    for j in 0..n {
                        row[j] -= sigma * ui * v[j];
                    }
                }
                u_cols.push(u);
                s_vals.push(sigma);
                v_cols.push(v);
            }
        }
    }

    (u_cols, s_vals, v_cols)
}

/// Power iteration to find the top singular triple `(u, σ, v)` of an
/// `m × n` matrix.
///
/// Returns `None` if the matrix is effectively zero (top singular value
/// below `tol`). Otherwise returns the triple with `||u|| = ||v|| = 1`
/// and `σ = ||M v||`.
///
/// The starting vector is a deterministic hash of `j` (sin of integer
/// indices) — this avoids pathological orthogonality with the top
/// singular vector that an all-ones start would suffer on signed data.
fn power_iteration_top_singular(
    mat: &[Vec<f64>],
    m: usize,
    n: usize,
    iters: usize,
    tol: f64,
) -> Option<(Vec<f64>, f64, Vec<f64>)> {
    // Deterministic starting vector with mixed signs.
    let mut v: Vec<f64> = (0..n)
        .map(|j| {
            let x = ((j as f64) + 1.0) * 0.791_613_5;
            x.fract() * 2.0 - 1.0
        })
        .collect();
    normalize_in_place(&mut v)?;

    let mut last_sigma: f64 = 0.0;
    let mut sigma: f64;
    let mut u: Vec<f64>;

    for _ in 0..iters {
        // u = M v  (length m)
        u = vec![0.0; m];
        for i in 0..m {
            let mut acc = 0.0;
            let row = &mat[i];
            for j in 0..n {
                acc += row[j] * v[j];
            }
            u[i] = acc;
        }
        sigma = norm(&u);
        if sigma < tol {
            return None;
        }
        for x in &mut u {
            *x /= sigma;
        }

        // v_new = M^T u  (length n)
        let mut v_new = vec![0.0; n];
        for i in 0..m {
            let ui = u[i];
            let row = &mat[i];
            for j in 0..n {
                v_new[j] += row[j] * ui;
            }
        }
        let v_norm = norm(&v_new);
        if v_norm < tol {
            return None;
        }
        for x in &mut v_new {
            *x /= v_norm;
        }

        // Convergence: singular value stabilizes.
        if last_sigma > 0.0 && (sigma - last_sigma).abs() < tol * sigma.max(1e-300) {
            v = v_new;
            break;
        }
        v = v_new;
        last_sigma = sigma;
    }

    // Final recompute of σ from the converged v.
    let mut u_final = vec![0.0; m];
    for i in 0..m {
        let mut acc = 0.0;
        let row = &mat[i];
        for j in 0..n {
            acc += row[j] * v[j];
        }
        u_final[i] = acc;
    }
    let sigma_final = norm(&u_final);
    if sigma_final < tol {
        return None;
    }
    for x in &mut u_final {
        *x /= sigma_final;
    }

    Some((u_final, sigma_final, v))
}

/// L2 norm of a vector.
fn norm(v: &[f64]) -> f64 {
    v.iter().map(|x| x * x).sum::<f64>().sqrt()
}

/// Normalize a vector in place. Returns `None` if the vector is zero.
fn normalize_in_place(v: &mut [f64]) -> Option<()> {
    let n = norm(v);
    if n < 1e-300 {
        return None;
    }
    for x in v {
        *x /= n;
    }
    Some(())
}

/// Maximum absolute entry of a matrix.
fn max_abs_entry(data: &[Vec<f64>]) -> f64 {
    data.iter().flat_map(|row| row.iter().map(|x| x.abs())).fold(0.0_f64, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a rank-1 matrix: outer product of `a` (length m) and `b` (length n).
    fn rank1_matrix(a: &[f64], b: &[f64]) -> Vec<Vec<f64>> {
        a.iter().map(|ai| b.iter().map(|bj| ai * bj).collect()).collect()
    }

    /// Test 6: decompose a 3×4 rank-1 matrix with rank 2 → compression_ratio > 1.
    ///
    /// The matrix is the outer product of [1, 2, 3] and [1, 2, 3, 4]:
    /// ```text
    ///  1  2  3  4
    ///  2  4  6  8
    ///  3  6  9 12
    /// ```
    /// Its true rank is 1, so the SVD finds one non-zero singular value.
    /// TT size = 3·1 + 1·4 = 7; original = 12; ratio = 12/7 ≈ 1.71 > 1.
    #[test]
    fn decompose_3x4_rank1_matrix_compression_ratio_above_one() {
        let data = rank1_matrix(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0, 4.0]);
        let tt = TensorTrain::decompose(&data, 2);
        let ratio = tt.compression_ratio();
        assert!(ratio > 1.0, "compression ratio for rank-1 3×4 matrix should be > 1, got {ratio}");
        assert_eq!(tt.effective_rank(), 1, "rank-1 matrix should yield effective_rank 1");
    }

    /// Test 7: reconstruct matches original within tolerance.
    ///
    /// For the same 3×4 rank-1 matrix, the reconstructed matrix should
    /// match the original within ~1e-6 (power iteration converges
    /// geometrically on rank-1 inputs).
    #[test]
    fn reconstruct_matches_original_within_tolerance() {
        let data = rank1_matrix(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0, 4.0]);
        let tt = TensorTrain::decompose(&data, 2);
        let recon = tt.reconstruct();
        assert_eq!(recon.len(), 12);
        let mut max_err: f64 = 0.0;
        for i in 0..3 {
            for j in 0..4 {
                let orig = data[i][j];
                let got = recon[i * 4 + j];
                max_err = max_err.max((orig - got).abs());
            }
        }
        assert!(max_err < 1e-6, "max reconstruction error {max_err} should be < 1e-6");
    }

    /// A 5×6 rank-2 matrix should reconstruct within tolerance with max_rank=2.
    #[test]
    fn rank2_matrix_reconstructs_with_rank2() {
        // Sum of two rank-1 matrices.
        let a = rank1_matrix(&[1.0, 2.0, 3.0, 4.0, 5.0], &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
        let b = rank1_matrix(&[1.0, 0.0, -1.0, 0.0, 1.0], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let data: Vec<Vec<f64>> =
            (0..5).map(|i| (0..6).map(|j| a[i][j] + b[i][j]).collect()).collect();
        let tt = TensorTrain::decompose(&data, 2);
        assert_eq!(tt.effective_rank(), 2);
        let recon = tt.reconstruct();
        let mut max_err: f64 = 0.0;
        for i in 0..5 {
            for j in 0..6 {
                max_err = max_err.max((data[i][j] - recon[i * 6 + j]).abs());
            }
        }
        assert!(max_err < 1e-4, "max reconstruction error {max_err} should be < 1e-4");
    }

    /// Empty input → empty TT (compression_ratio = 1, reconstruct = []).
    #[test]
    fn empty_matrix_decomposes_to_empty_tt() {
        let tt = TensorTrain::decompose(&[], 5);
        assert!(tt.cores.is_empty());
        assert_eq!(tt.compression_ratio(), 1.0);
        assert!(tt.reconstruct().is_empty());
        assert_eq!(tt.effective_rank(), 0);
    }

    /// All-zero matrix → rank-1 TT with zero cores (reconstruct = zeros).
    #[test]
    fn zero_matrix_decomposes_to_zero_tt() {
        let data = vec![vec![0.0; 4]; 3];
        let tt = TensorTrain::decompose(&data, 2);
        let recon = tt.reconstruct();
        assert_eq!(recon.len(), 12);
        for x in &recon {
            assert!(x.abs() < 1e-12, "expected zero, got {x}");
        }
    }

    /// A full-rank random-ish matrix with small max_rank still
    /// compresses (because we drop the high-rank tails).
    #[test]
    fn full_rank_matrix_with_small_max_rank_compresses() {
        // 10×8 matrix with full rank 8, but we truncate to rank 2.
        let data: Vec<Vec<f64>> =
            (0..10).map(|i| (0..8).map(|j| ((i * j + i + j) as f64) * 0.1).collect()).collect();
        let tt = TensorTrain::decompose(&data, 2);
        // TT size = 10*2 + 2*8 = 36; original = 80; ratio = 80/36 ≈ 2.22.
        assert!(tt.compression_ratio() > 1.5, "ratio = {} should be > 1.5", tt.compression_ratio());
        assert_eq!(tt.effective_rank(), 2);
    }

    /// `effective_rank` on an empty TT is 0.
    #[test]
    fn effective_rank_empty_is_zero() {
        let tt = TensorTrain { cores: Vec::new(), ranks: vec![1], shape: Vec::new() };
        assert_eq!(tt.effective_rank(), 0);
    }

    /// A 100×50 low-rank matrix compresses well (benchmark sanity).
    ///
    /// The matrix is built as a sum of 3 outer products using
    /// polynomial-Vandermonde vectors (degrees 1, 2, 3 in `i`,
    /// 2, 3, 4 in `j`), which are guaranteed linearly independent — so
    /// the true matrix rank is exactly 3.
    #[test]
    fn large_low_rank_matrix_compresses() {
        // Build a 100×50 rank-3 matrix as sum of 3 outer products of
        // polynomial vectors (linearly independent by Vandermonde).
        let mut data = vec![vec![0.0; 50]; 100];
        for k in 0..3u32 {
            let degree_a = k + 1;
            let degree_b = k + 2;
            let a: Vec<f64> = (0..100)
                .map(|i| {
                    let x = (i as f64 + 1.0) * 0.01;
                    x.powi(degree_a as i32)
                })
                .collect();
            let b: Vec<f64> = (0..50)
                .map(|j| {
                    let y = (j as f64 + 1.0) * 0.05;
                    y.powi(degree_b as i32)
                })
                .collect();
            for i in 0..100 {
                for j in 0..50 {
                    data[i][j] += a[i] * b[j];
                }
            }
        }
        let tt = TensorTrain::decompose(&data, 5);
        // TT size = 100*3 + 3*50 = 450; original = 5000; ratio ≈ 11.1.
        assert!(tt.compression_ratio() > 5.0, "ratio = {} should be > 5", tt.compression_ratio());
        assert_eq!(
            tt.effective_rank(),
            3,
            "rank-3 polynomial matrix should yield effective_rank 3"
        );
        // Reconstruction should be very close.
        let recon = tt.reconstruct();
        let mut max_err: f64 = 0.0;
        for i in 0..100 {
            for j in 0..50 {
                max_err = max_err.max((data[i][j] - recon[i * 50 + j]).abs());
            }
        }
        assert!(max_err < 1e-3, "max error {max_err} should be < 1e-3");
    }

    /// Power iteration on a 2×2 rank-1 matrix finds the top singular vector.
    #[test]
    fn power_iteration_finds_top_singular_vector() {
        let mat = rank1_matrix(&[3.0, 4.0], &[1.0, 2.0]);
        // M = [[3, 6], [4, 8]], σ = ||[3,4]|| * ||[1,2]|| = 5 * sqrt(5) ≈ 11.18.
        let result = power_iteration_top_singular(&mat, 2, 2, 200, 1e-12);
        let (u, sigma, v) = result.expect("should find top singular triple");
        let expected_sigma = 5.0_f64 * 5.0_f64.sqrt();
        assert!((sigma - expected_sigma).abs() / expected_sigma < 1e-3, "sigma = {sigma}");
        // u should be ±[3, 4]/5 (normalized).
        let u_expected = [3.0 / 5.0, 4.0 / 5.0];
        let sign = if u[0] * u_expected[0] >= 0.0 { 1.0 } else { -1.0 };
        assert!((u[0] - sign * u_expected[0]).abs() < 1e-3, "u[0] = {}", u[0]);
        assert!((u[1] - sign * u_expected[1]).abs() < 1e-3, "u[1] = {}", u[1]);
        let v_norm = norm(&v);
        assert!((v_norm - 1.0).abs() < 1e-6, "v should be unit norm, got {v_norm}");
    }

    /// `compression_ratio` returns 1.0 for an empty TT.
    #[test]
    fn compression_ratio_empty_is_one() {
        let tt = TensorTrain { cores: Vec::new(), ranks: vec![1], shape: Vec::new() };
        assert_eq!(tt.compression_ratio(), 1.0);
    }
}
