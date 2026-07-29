//! HyperLogLog (ADR-015).
//!
//! Standard HLL with stochastic averaging over `m = 2^p` registers
//! (`p ∈ [4, 14]`). Insertion splits a 64-bit hash into:
//!
//! 1. `p` index bits (the register address);
//! 2. `64 - p` body bits, from which we count leading zeros `+1`.
//!
//! The register holds the running maximum of `(leading_zeros + 1)` over all
//! hashes that mapped to it.
//!
//! ## Estimation
//!
//! The standard harmonic-mean estimator with small-range and large-range
//! corrections:
//!
//! - raw: `α * m² / Σ 2^{-r_j}` with `α = 0.7213 / (1 + 1.079/m)`;
//! - small-range (`E ≤ 2.5 m`): linear counting when there are zero registers;
//! - large-range (`E > 2^32 / 30`): `2^32 * log2(1 - E / 2^32)`.
//!
//! For our 64-bit hashes the large-range correction is essentially never
//! triggered, but is kept for fidelity.
//!
//! Relative standard error: `1.04 / sqrt(m)`.

/// The base α for HLL (`m ≥ 128`). The per-instance correction
/// `1 + 1.079/m` is applied in [`HyperLogLog::estimate`].
const HLL_ALPHA_BASE: f64 = 0.7213;

/// HyperLogLog over 64-bit hashes.
#[derive(Clone, Debug)]
pub struct HyperLogLog {
    /// `m = 2^precision` registers, each storing the max leading-zero count+1.
    registers: Vec<u8>,
    /// `log2(m)`, must be in `[4, 14]`.
    precision: u32,
}

impl HyperLogLog {
    /// Create an HLL with `m = 2^precision` registers. `precision` is clamped
    /// to `[4, 14]`.
    pub fn new(precision: u32) -> Self {
        let p = precision.clamp(4, 14);
        let m = 1usize << p;
        Self { registers: vec![0u8; m], precision: p }
    }

    /// Number of registers (`m = 2^precision`).
    pub fn len(&self) -> usize {
        self.registers.len()
    }

    /// Whether the index is empty (zero registers — never, given the clamp).
    pub fn is_empty(&self) -> bool {
        self.registers.is_empty()
    }

    /// Update with a 64-bit hash. The low `p` bits select the register; the
    /// remaining high bits feed the leading-zero count.
    pub fn add(&mut self, hash: u64) {
        let m = self.registers.len() as u64;
        let p = self.precision;
        // Register index from low p bits.
        let idx = (hash & (m - 1)) as usize;
        // `body` is the high (64 - p) bits of the hash, stored in a u64 with
        // its top `p` bits zero (a side-effect of the right shift). For HLL
        // we want `rank = (leading zeros of body within its 64-p active
        // window) + 1`. Since `body.leading_zeros()` counts all 64 bits,
        // including the `p` always-zero high bits, we subtract them.
        let body = hash >> p;
        let rank: u8 = if body == 0 {
            // All (64 - p) body bits are zero → rank = (64 - p) + 1.
            (65 - p) as u8
        } else {
            // Leftmost set bit at position k (LSB = 0). leading_zeros = 63 - k
            // (counting all 64 bits of body, including the top p zero bits).
            // Within the (64-p)-bit active window, leading zeros = (63 - k) - p.
            // rank = leading_zeros_in_window + 1 = (63 - k - p) + 1.
            (body.leading_zeros() - p + 1) as u8
        };
        if rank > self.registers[idx] {
            self.registers[idx] = rank;
        }
    }

    /// Estimate the cardinality.
    pub fn estimate(&self) -> f64 {
        let m = self.registers.len() as f64;
        let mut sum: f64 = 0.0;
        let mut zeros: u64 = 0;
        for &r in &self.registers {
            sum += 2f64.powi(-(r as i32));
            if r == 0 {
                zeros += 1;
            }
        }
        // α_m = 0.7213 / (1 + 1.079/m) for m ≥ 128.
        let alpha = HLL_ALPHA_BASE / (1.0 + 1.079 / m);
        let raw = alpha * m * m / sum;

        // Small-range correction: linear counting when raw ≤ 2.5m and there
        // exist zero registers.
        if raw <= 2.5 * m && zeros > 0 {
            return m * (m / zeros as f64).ln();
        }

        // Large-range correction — essentially never triggers with 64-bit
        // hashes, but kept for fidelity.
        let two_32 = 1u64 << 32;
        if raw > (two_32 as f64) / 30.0 {
            let raw_u64 = raw as u64;
            return -(two_32 as f64) * (1.0 - (raw_u64 as f64) / (two_32 as f64)).ln();
        }

        raw
    }

    /// Merge another HLL by taking the per-register maximum.
    ///
    /// Both HLLs must share the same precision.
    pub fn merge(&mut self, other: &HyperLogLog) {
        assert_eq!(self.precision, other.precision, "HLL merge precision mismatch");
        for (a, b) in self.registers.iter_mut().zip(other.registers.iter()) {
            if *b > *a {
                *a = *b;
            }
        }
    }

    /// Precision parameter `p` (`m = 2^p`).
    pub fn precision(&self) -> u32 {
        self.precision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xxhash_rust::xxh3;

    fn build_hll(n: u64, p: u32) -> HyperLogLog {
        let mut h = HyperLogLog::new(p);
        for i in 0..n {
            h.add(xxh3::xxh3_64(&i.to_le_bytes()));
        }
        h
    }

    #[test]
    fn hll_estimate_within_5pct_of_10000() {
        let h = build_hll(10_000, 14);
        let est = h.estimate();
        let rel = (est - 10_000.0).abs() / 10_000.0;
        assert!(rel < 0.05, "HLL estimate {est} not within 5% of 10000 (rel={rel:.4})");
    }

    #[test]
    fn hll_merge_is_additive_within_tolerance() {
        // Merge two HLLs built over *disjoint* id spaces (offset by 1e6) so
        // their union cardinality is 10000. With same-stream inputs HLL
        // correctly reports ~5000 (idempotent merge) — that would fail a
        // 10000-target test, so we make the streams disjoint.
        let mut a = HyperLogLog::new(14);
        let mut b = HyperLogLog::new(14);
        for i in 0..5_000u64 {
            a.add(xxh3::xxh3_64(&i.to_le_bytes()));
            b.add(xxh3::xxh3_64(&(i + 1_000_000).to_le_bytes()));
        }
        a.merge(&b);
        let est = a.estimate();
        let rel = (est - 10_000.0).abs() / 10_000.0;
        assert!(rel < 0.05, "merged HLL estimate {est} not within 5% of 10000 (rel={rel:.4})");
    }

    #[test]
    fn hll_merge_of_identical_streams_is_idempotent() {
        // Sanity check: merging a sketch with itself (same stream) should
        // not double the estimate.
        let mut a = HyperLogLog::new(14);
        let mut b = HyperLogLog::new(14);
        for i in 0..5_000u64 {
            let h = xxh3::xxh3_64(&i.to_le_bytes());
            a.add(h);
            b.add(h);
        }
        let before = a.estimate();
        a.merge(&b);
        let after = a.estimate();
        let rel = (after - before).abs() / before.max(1.0);
        assert!(
            rel < 0.05,
            "merge of identical streams changed estimate: before={before}, after={after}"
        );
    }

    #[test]
    fn hll_empty_estimate_zero() {
        let h = HyperLogLog::new(12);
        assert_eq!(h.estimate(), 0.0);
    }

    #[test]
    fn hll_merge_requires_same_precision() {
        let mut a = HyperLogLog::new(12);
        let b = HyperLogLog::new(14);
        // Should panic.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| a.merge(&b)));
        assert!(result.is_err());
    }

    #[test]
    fn hll_precision_clamped() {
        let h_low = HyperLogLog::new(2);
        assert_eq!(h_low.precision(), 4);
        let h_high = HyperLogLog::new(20);
        assert_eq!(h_high.precision(), 14);
    }

    #[test]
    fn hll_small_count_is_accurate() {
        // For very small counts HLL should still be in the right ballpark.
        let mut h = HyperLogLog::new(14);
        for i in 0..50u64 {
            h.add(xxh3::xxh3_64(&i.to_le_bytes()));
        }
        let est = h.estimate();
        assert!(est >= 25.0 && est <= 75.0, "small count estimate {est} off");
    }

    #[test]
    fn hll_relative_error_scales_with_m() {
        // RSE ≈ 1.04 / sqrt(m). With m=2^14 ≈ 16k, RSE ≈ 0.81% → 100k-element
        // estimate within 3%. Use precision=14 (max) for best accuracy.
        let h = build_hll(100_000, 14);
        let est = h.estimate();
        let rel = (est - 100_000.0).abs() / 100_000.0;
        assert!(rel < 0.03, "100k estimate {est} rel={rel:.4}");
    }
}
