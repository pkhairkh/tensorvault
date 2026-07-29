//! t-Digest (ADR-015) — simplified merging-centroid variant.
//!
//! A streaming structure for approximate quantile estimation. The digest
//! holds up to `max_centroids` weighted centroids `(mean, weight)`. When
//! the budget is exceeded we compress by merging adjacent centroids.
//!
//! ## Compression policy
//!
//! We use a uniform merge policy (not the true t-digest scale function) —
//! when the budget is exceeded we merge the two centroids with the smallest
//! gap between their means. This keeps the structure simple, bounded, and
//! reasonably accurate for the test tolerance (±5% at p50, ±5% at p99 on
//! uniform input).
//!
//! ## Quantile query
//!
//! Walk the sorted centroids accumulating weight; the centroid whose
//! cumulative weight straddles `q * total_weight` contributes the answer.
//! We linearly interpolate between the previous centroid's mean and the
//! current centroid's mean.

/// A weighted centroid: a mean and the number of points it represents.
#[derive(Clone, Copy, Debug)]
struct Centroid {
    mean: f64,
    weight: u64,
}

/// Streaming approximate quantile estimator.
#[derive(Clone, Debug)]
pub struct TDigest {
    /// Sorted by `mean`.
    centroids: Vec<Centroid>,
    /// Maximum number of centroids before forced compression.
    max_centroids: usize,
    /// Total weight across all centroids.
    total_weight: u64,
}

impl TDigest {
    /// Create a digest that keeps at most `max_centroids` centroids.
    pub fn new(max_centroids: usize) -> Self {
        assert!(max_centroids >= 2, "max_centroids must be ≥ 2");
        Self { centroids: Vec::with_capacity(max_centroids + 1), max_centroids, total_weight: 0 }
    }

    /// Add a single observation.
    ///
    /// Inserts as a unit-weight centroid at the correct sorted position; if
    /// the budget is exceeded, compresses by merging the closest pair.
    pub fn add(&mut self, value: f64) {
        let c = Centroid { mean: value, weight: 1 };
        let pos = self.centroids.partition_point(|existing| existing.mean < value);
        self.centroids.insert(pos, c);
        self.total_weight += 1;
        if self.centroids.len() > self.max_centroids {
            self.compress();
        }
    }

    /// Merge the two adjacent centroids with the smallest mean-gap.
    fn compress(&mut self) {
        if self.centroids.len() < 2 {
            return;
        }
        // Find the adjacent pair with the smallest gap.
        let mut best = 0usize;
        let mut best_gap = f64::INFINITY;
        for i in 0..self.centroids.len() - 1 {
            let gap = self.centroids[i + 1].mean - self.centroids[i].mean;
            if gap < best_gap {
                best_gap = gap;
                best = i;
            }
        }
        // Merge centroids[best] and centroids[best+1].
        let a = self.centroids[best];
        let b = self.centroids[best + 1];
        let w = a.weight + b.weight;
        let mean = (a.mean * a.weight as f64 + b.mean * b.weight as f64) / w as f64;
        let merged = Centroid { mean, weight: w };
        self.centroids[best] = merged;
        self.centroids.remove(best + 1);
    }

    /// Estimate the `q`-quantile (`0.0 ≤ q ≤ 1.0`).
    ///
    /// Walks centroids accumulating weight; linearly interpolates between
    /// the bracketing centroids' means at the target cumulative weight.
    pub fn quantile(&self, q: f64) -> f64 {
        assert!((0.0..=1.0).contains(&q), "quantile must be in [0, 1], got {q}");
        if self.centroids.is_empty() {
            return f64::NAN;
        }
        if self.centroids.len() == 1 {
            return self.centroids[0].mean;
        }
        let total = self.total_weight as f64;
        // Edge: q = 0 or 1 — return the min / max centroid mean.
        if q == 0.0 {
            return self.centroids[0].mean;
        }
        if q == 1.0 {
            return self.centroids[self.centroids.len() - 1].mean;
        }

        let target = q * total;
        let mut cum_prev: f64 = 0.0;
        for window in self.centroids.windows(2) {
            let (a, b) = (window[0], window[1]);
            let cum_cur = cum_prev + a.weight as f64;
            if cum_cur >= target {
                // Linearly interpolate between mean(a) and mean(b).
                let span = a.weight as f64; // weight of a
                let frac =
                    if span > 0.0 { ((target - cum_prev) / span).clamp(0.0, 1.0) } else { 0.0 };
                return a.mean + frac * (b.mean - a.mean);
            }
            cum_prev = cum_cur;
        }
        // Fallback: return last centroid's mean.
        self.centroids[self.centroids.len() - 1].mean
    }

    /// Merge another digest by replaying its centroids as weighted inserts.
    pub fn merge(&mut self, other: &TDigest) {
        // Insert each foreign centroid in sorted order; reuse add() with
        // weight-aware logic by inserting and then re-compressing.
        for &c in &other.centroids {
            let pos = self.centroids.partition_point(|existing| existing.mean < c.mean);
            self.centroids.insert(pos, c);
            self.total_weight += c.weight;
            if self.centroids.len() > self.max_centroids {
                self.compress();
            }
        }
    }

    /// Number of centroids currently held.
    pub fn num_centroids(&self) -> usize {
        self.centroids.len()
    }

    /// Total weight observed.
    pub fn total_weight(&self) -> u64 {
        self.total_weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tdigest_p50_of_1_to_1000() {
        let mut td = TDigest::new(100);
        for v in 1..=1000i64 {
            td.add(v as f64);
        }
        let q50 = td.quantile(0.5);
        let rel = (q50 - 500.0).abs() / 500.0;
        assert!(rel < 0.05, "p50 = {q50}, expected ~500 (rel={rel:.4})");
    }

    #[test]
    fn tdigest_p99_of_1_to_1000() {
        let mut td = TDigest::new(100);
        for v in 1..=1000i64 {
            td.add(v as f64);
        }
        let q99 = td.quantile(0.99);
        let rel = (q99 - 990.0).abs() / 990.0;
        assert!(rel < 0.05, "p99 = {q99}, expected ~990 (rel={rel:.4})");
    }

    #[test]
    fn tdigest_merge_two_digests() {
        let mut a = TDigest::new(100);
        let mut b = TDigest::new(100);
        for v in 1..=500i64 {
            a.add(v as f64);
        }
        for v in 501..=1000i64 {
            b.add(v as f64);
        }
        a.merge(&b);
        let q50 = a.quantile(0.5);
        let rel = (q50 - 500.0).abs() / 500.0;
        assert!(rel < 0.05, "merged p50 = {q50}, expected ~500 (rel={rel:.4})");
        let q99 = a.quantile(0.99);
        let rel99 = (q99 - 990.0).abs() / 990.0;
        assert!(rel99 < 0.05, "merged p99 = {q99}, expected ~990 (rel={rel99:.4})");
    }

    #[test]
    fn tdigest_single_centroid() {
        let mut td = TDigest::new(50);
        td.add(42.0);
        assert_eq!(td.quantile(0.5), 42.0);
        assert_eq!(td.quantile(0.0), 42.0);
        assert_eq!(td.quantile(1.0), 42.0);
    }

    #[test]
    fn tdigest_extremes() {
        // Use a generous centroid budget so no compression happens — the
        // min/max centroids are then the exact stream min/max.
        let mut td = TDigest::new(2000);
        for v in 1..=1000i64 {
            td.add(v as f64);
        }
        let q0 = td.quantile(0.0);
        let q1 = td.quantile(1.0);
        assert!((q0 - 1.0).abs() < 1e-9, "p0 = {q0}, expected 1");
        assert!((q1 - 1000.0).abs() < 1e-9, "p1 = {q1}, expected 1000");
    }

    #[test]
    fn tdigest_extremes_compressed() {
        // With heavy compression, p0/p1 only approximate the true min/max —
        // but should still land within 5% of the stream range.
        let mut td = TDigest::new(50);
        for v in 1..=1000i64 {
            td.add(v as f64);
        }
        let q0 = td.quantile(0.0);
        let q1 = td.quantile(1.0);
        assert!(q0 <= 50.0, "compressed p0 = {q0}, expected <= 50 (within 5% of stream min=1)");
        assert!(
            q1 >= 950.0,
            "compressed p1 = {q1}, expected >= 950 (within 5% of stream max=1000)"
        );
    }

    #[test]
    fn tdigest_empty_returns_nan() {
        let td = TDigest::new(50);
        assert!(td.quantile(0.5).is_nan());
    }
}
