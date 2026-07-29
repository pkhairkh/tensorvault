//! Count-Min Sketch (ADR-015).
//!
//! A `d × w` table of counters. Each insert updates `d` counters, one per
//! row, using `d` independent hash functions. A point estimate returns the
//! minimum of those `d` counters — never underestimates (every row sees the
//! full stream), overestimates only via hash collisions with heavy items.
//!
//! ## Parameters
//!
//! - `width` `w` — `O(1/ε)`. Error bound: ≤ `ε * N` with probability
//!   `1 - δ`, where `N` is total stream weight.
//! - `depth` `d` — `O(log(1/δ))`.
//!
//! ## Merge
//!
//! Point-wise counter addition — semantically equivalent to running the
//! merged streams through one sketch.

use xxhash_rust::xxh3;

/// Count-Min sketch.
#[derive(Clone, Debug)]
pub struct CountMin {
    /// Number of hash functions (`d`).
    depth: usize,
    /// Number of counters per row (`w`).
    width: usize,
    /// `d × w` counters.
    counts: Vec<Vec<u64>>,
    /// `d` seed values, used to derive independent hash functions.
    seeds: Vec<u64>,
}

impl CountMin {
    /// Build a `depth × width` sketch. Seeds are derived deterministically
    /// from a fixed base.
    pub fn new(depth: usize, width: usize) -> Self {
        assert!(depth > 0, "CountMin depth must be > 0");
        assert!(width > 0, "CountMin width must be > 0");
        let counts = vec![vec![0u64; width]; depth];
        // Independent seeds per row — `i * 0x9E37... + 1` keeps them nonzero
        // and well-spread.
        let seeds: Vec<u64> =
            (0..depth).map(|i| (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1).collect();
        Self { depth, width, counts, seeds }
    }

    /// Hash `key` under row `r` to a column index in `[0, width)`.
    #[inline]
    fn hash(&self, key: u64, r: usize) -> usize {
        let mut buf = [0u8; 16];
        buf[..8].copy_from_slice(&key.to_le_bytes());
        buf[8..16].copy_from_slice(&self.seeds[r].to_le_bytes());
        let h = xxh3::xxh3_64(&buf);
        (h as usize) % self.width
    }

    /// Add `count` to key `key`.
    pub fn add(&mut self, key: u64, count: u64) {
        for r in 0..self.depth {
            let c = self.hash(key, r);
            self.counts[r][c] = self.counts[r][c].saturating_add(count);
        }
    }

    /// Estimate the frequency of `key`. Returns the min across all `d` rows.
    pub fn estimate(&self, key: u64) -> u64 {
        let mut min = u64::MAX;
        for r in 0..self.depth {
            let c = self.hash(key, r);
            if self.counts[r][c] < min {
                min = self.counts[r][c];
            }
        }
        if min == u64::MAX {
            0
        } else {
            min
        }
    }

    /// Merge another sketch by point-wise counter addition. Both must share
    /// the same `depth` and `width`.
    pub fn merge(&mut self, other: &CountMin) {
        assert_eq!(self.depth, other.depth, "CountMin merge depth mismatch");
        assert_eq!(self.width, other.width, "CountMin merge width mismatch");
        for r in 0..self.depth {
            for c in 0..self.width {
                self.counts[r][c] = self.counts[r][c].saturating_add(other.counts[r][c]);
            }
        }
    }

    /// Depth (`d`).
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Width (`w`).
    pub fn width(&self) -> usize {
        self.width
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cm_add_1000_of_42_estimates_at_least_1000() {
        let mut cm = CountMin::new(5, 1 << 14);
        for _ in 0..1000 {
            cm.add(42, 1);
        }
        let est = cm.estimate(42);
        assert!(est >= 1000, "CountMin underestimated 42: {est}");
    }

    #[test]
    fn cm_unseen_key_estimate_is_much_smaller() {
        let mut cm = CountMin::new(5, 1 << 14);
        // Heavy stream on key=42 only.
        for _ in 0..1000 {
            cm.add(42, 1);
        }
        let seen = cm.estimate(42);
        let unseen = cm.estimate(99);
        assert!(
            unseen * 10 < seen,
            "unseen key estimate {unseen} not much smaller than seen {seen}"
        );
    }

    #[test]
    fn cm_never_underestimates() {
        let mut cm = CountMin::new(4, 1024);
        // Fill with many keys to induce collisions.
        for k in 0..2000u64 {
            cm.add(k, 1);
        }
        // Every key was inserted once → estimate ≥ 1.
        for k in 0..2000u64 {
            let e = cm.estimate(k);
            assert!(e >= 1, "CountMin underestimated key {k}: {e}");
        }
    }

    #[test]
    fn cm_merge_doubles_estimate() {
        let mut a = CountMin::new(5, 1 << 14);
        let mut b = CountMin::new(5, 1 << 14);
        for _ in 0..500 {
            a.add(7, 1);
            b.add(7, 1);
        }
        a.merge(&b);
        let est = a.estimate(7);
        assert!(est >= 1000, "merged estimate {est} < 1000");
    }

    #[test]
    fn cm_add_with_count_arg() {
        let mut cm = CountMin::new(5, 1 << 14);
        cm.add(123, 50);
        assert_eq!(cm.estimate(123), 50);
    }
}
