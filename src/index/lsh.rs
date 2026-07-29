//! Locality-Sensitive Hash index (ADR-017).
//!
//! Random-hyperplane LSH for cosine similarity over floating-point vectors.
//! We maintain `L` independent hash tables; each table hashes a vector via
//! `k` random hyperplanes into a `k`-bit signature. Buckets group vectors
//! whose signatures collide — high collision probability ⇒ high cosine
//! similarity with high probability.
//!
//! ## Query model
//!
//! A point query computes its signature in each of the `L` tables and returns
//! the union of all buckets it lands in. The candidate set is a strict
//! superset of the true top-k (modulo hash collisions); the user filters by
//! exact distance afterwards.
//!
//! ## Seeded RNG
//!
//! Hyperplanes are drawn from a seeded split-mix64 RNG so two indexes built
//! with the same seed share identical partitions — a prerequisite for
//! distributed/parallel builds.

use std::collections::HashMap;

/// SplitMix64 — fast, deterministic, good enough for LSH hyperplane sampling.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Convert a `u64` to a standard-normal `f64` via Box-Muller.
fn randn(state: &mut u64) -> f64 {
    let u1 = {
        let r = splitmix64(state);
        // Map to (0, 1] — never zero, to avoid log(0).
        (r as f64) / (u64::MAX as f64) + f64::MIN_POSITIVE
    };
    let u2 = (splitmix64(state) as f64) / (u64::MAX as f64);
    let two_pi = 2.0 * std::f64::consts::PI;
    let mag = (-2.0 * u1.ln()).sqrt();
    mag * (two_pi * u2).cos()
}

/// Random-hyperplane LSH index.
///
/// `tables[t]` is a `HashMap` from the `k`-bit signature (packed in a `u64`)
/// to the list of inserted ids that hashed there.
pub struct LshIndex {
    /// `L` tables, each `signature → [row_ids]`.
    tables: Vec<HashMap<u64, Vec<u64>>>,
    /// `hyperplanes[t][h]` is the `h`-th hyperplane of table `t` — a vector
    /// of length `dim`.
    hyperplanes: Vec<Vec<Vec<f64>>>,
    /// Vector dimensionality.
    dim: usize,
    /// Number of tables (`L`).
    num_tables: usize,
    /// Number of hyperplanes per table (`k`).
    num_hashes: usize,
}

impl LshIndex {
    /// Build an empty LSH index with `num_tables` tables, `num_hashes`
    /// hyperplanes per table, all of length `dim`. Hyperplanes are sampled
    /// from a standard normal seeded by `seed`.
    pub fn new(dim: usize, num_tables: usize, num_hashes: usize, seed: u64) -> Self {
        assert!(dim > 0, "LSH dimension must be > 0");
        assert!(num_tables > 0, "LSH num_tables must be > 0");
        assert!(num_hashes > 0 && num_hashes <= 63, "num_hashes must be in 1..=63");

        let mut state = seed.max(1);
        let mut hyperplanes = Vec::with_capacity(num_tables);
        for _ in 0..num_tables {
            let mut table_planes = Vec::with_capacity(num_hashes);
            for _ in 0..num_hashes {
                let plane: Vec<f64> = (0..dim).map(|_| randn(&mut state)).collect();
                table_planes.push(plane);
            }
            hyperplanes.push(table_planes);
        }

        let tables = (0..num_tables).map(|_| HashMap::new()).collect();

        Self { tables, hyperplanes, dim, num_tables, num_hashes }
    }

    /// Dimensionality of vectors this index accepts.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Compute the `k`-bit signature of `vector` under table `table_idx`.
    ///
    /// Bit `h` is set iff the inner product of `vector` with hyperplane `h`
    /// of table `table_idx` is non-negative.
    pub fn signature(&self, vector: &[f64], table_idx: usize) -> u64 {
        debug_assert_eq!(vector.len(), self.dim, "LSH vector dim mismatch");
        let planes = &self.hyperplanes[table_idx];
        let mut sig: u64 = 0;
        for (h, plane) in planes.iter().enumerate() {
            let mut dot = 0.0;
            for (v, p) in vector.iter().zip(plane.iter()) {
                dot += *v * *p;
            }
            if dot >= 0.0 {
                sig |= 1u64 << h;
            }
        }
        sig
    }

    /// Insert a vector under id `id` into every table.
    pub fn insert(&mut self, id: u64, vector: &[f64]) {
        debug_assert_eq!(vector.len(), self.dim, "LSH insert dim mismatch");
        for t in 0..self.num_tables {
            let sig = self.signature(vector, t);
            self.tables[t].entry(sig).or_default().push(id);
        }
    }

    /// Query: return the union of buckets the vector hashes to across all
    /// `L` tables. Order is unspecified; duplicates are removed.
    pub fn query(&self, vector: &[f64]) -> Vec<u64> {
        debug_assert_eq!(vector.len(), self.dim, "LSH query dim mismatch");
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for t in 0..self.num_tables {
            let sig = self.signature(vector, t);
            if let Some(bucket) = self.tables[t].get(&sig) {
                for &id in bucket {
                    if seen.insert(id) {
                        out.push(id);
                    }
                }
            }
        }
        out
    }

    /// Number of tables (`L`).
    pub fn num_tables(&self) -> usize {
        self.num_tables
    }

    /// Number of hyperplanes per table (`k`).
    pub fn num_hashes(&self) -> usize {
        self.num_hashes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_index(n: usize, dim: usize) -> LshIndex {
        // L=8, k=4 → 16 buckets/table, 128 total buckets.
        let mut idx = LshIndex::new(dim, 8, 4, 0xC0FFEE);
        for i in 0..n as u64 {
            let mut v = vec![0.0f64; dim];
            v[0] = i as f64;
            idx.insert(i, &v);
        }
        idx
    }

    #[test]
    fn lsh_query_returns_self() {
        let idx = make_index(100, 8);
        // Query with vector identical to id=42.
        let mut v = vec![0.0f64; 8];
        v[0] = 42.0;
        let candidates = idx.query(&v);
        assert!(candidates.contains(&42), "expected id=42 in candidates");
    }

    #[test]
    fn lsh_query_returns_neighbours() {
        // Identical vectors must collide in every table.
        let mut idx = LshIndex::new(8, 4, 4, 7);
        idx.insert(1, &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        idx.insert(2, &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        idx.insert(3, &[-1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let cands = idx.query(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        // Both 1 and 2 must be retrieved (identical signatures).
        assert!(cands.contains(&1));
        assert!(cands.contains(&2));
    }

    #[test]
    fn lsh_signature_is_deterministic() {
        let idx = LshIndex::new(4, 3, 5, 12345);
        let v = [0.5, -0.5, 1.0, -2.0];
        let s0 = idx.signature(&v, 0);
        let s0_again = idx.signature(&v, 0);
        assert_eq!(s0, s0_again);
        // Different tables have different hyperplanes → usually different sigs.
        let different = (0..idx.num_tables()).any(|t| idx.signature(&v, t) != s0);
        assert!(different, "expected at least one table to differ");
    }

    #[test]
    fn lsh_zero_vector_signature_is_table_dependent() {
        let idx = LshIndex::new(4, 4, 4, 99);
        let v = [0.0; 4];
        // Dot product is 0 → every bit is set (>= 0).
        for t in 0..idx.num_tables() {
            let sig = idx.signature(&v, t);
            assert_eq!(sig, (1u64 << idx.num_hashes()) - 1);
        }
    }
}
