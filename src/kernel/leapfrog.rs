//! Leapfrog triejoin kernel (Veldhuizen 2014).
//!
//! Leapfrog triejoin is a **worst-case optimal** join algorithm: it runs in
//! `O(IN + OUT + AGM)` time, where AGM is the Atserias-Grohe-Marx bound
//! ([`crate::planner::agm`]). This is asymptotically better than a cascade
//! of binary hash joins, which can blow up to `∏ |Ri|` on cyclic queries.
//!
//! ## Algorithm
//!
//! For the simple case of a multiway intersection on a single attribute (all
//! relations agree on the join key), the algorithm reduces to:
//!
//! 1. Each input is a **sorted iterator** over the join key.
//! 2. Find `max_key = max(it_i.current_key())` across all iterators.
//! 3. Seek every iterator to `≥ max_key`. Each iterator either lands on
//!    `max_key` (a match for that iterator) or some larger key (a miss).
//! 4. If **all** iterators are now positioned at `max_key`, emit `max_key`
//!    and advance exactly one iterator (any one — they all move to the
//!    next distinct key).
//! 5. Otherwise, repeat from step 2 with the new maximum.
//! 6. Stop as soon as any iterator is exhausted.
//!
//! The key insight is that **leapfrogging** (the seek-then-compare pattern)
//! is at least as fast as a merge join, and on cyclic queries it visits
//! only `O(AGM)` keys — never more.
//!
//! ## This module
//!
//! The [`LeapfrogJoin`] struct implements single-attribute multiway
//! intersection. It is **not** a [`Kernel`](crate::kernel::Kernel) trait
//! implementation, because kernels in turbogp's table take a single input
//! region; leapfrog takes N. It is instead a standalone struct that the
//! executor can call directly when the planner emits a worst-case-optimal
//! join node.
//!
//! A scalar kernel wrapper ([`LeapfrogScalar`]) is also provided so the
//! operator can be looked up in the kernel table by `(Operator, Cpu, Tier)`.
//! That wrapper runs leapfrog on a single pair of slices encoded into the
//! `input` pointer — useful for benchmarking and introspection.
//!
//! ## References
//!
//! - Veldhuizen, "Leapfrog Triejoin: a simple, worst-case optimal join
//!   algorithm", ICDT 2014.
//! - Ngo, Porat, Ré, Rudra, "Worst-case optimal join algorithms",
//!   PODS 2012.

use crate::kernel::cpu::CpuTarget;
use crate::kernel::{Kernel, KernelParams, KernelResult, Operator};
use crate::memory::tier::MemoryTier;

/// A sorted iterator over join keys.
///
/// Each iterator conceptually yields a strictly-increasing sequence of
/// `u64` keys. The trait provides:
///
/// - `current_key()` — the key the iterator is currently positioned at, or
///   `None` if exhausted.
/// - `seek(k)` — advance the iterator to the first key `≥ k`. Returns the
///   new current key, or `None` if no such key exists (iterator exhausted).
/// - `next()` — advance past the current key to the next distinct key.
///   Returns the new current key, or `None` if exhausted.
///
/// Implementations must guarantee that the sequence of keys returned is
/// strictly increasing — duplicates are not allowed.
pub trait SortedIterator: Send {
    /// The key the iterator is currently positioned at, or `None` if
    /// exhausted (no more keys).
    fn current_key(&self) -> Option<u64>;
    /// Advance to the first key `≥ key`. Returns the new current key, or
    /// `None` if the iterator is exhausted past `key`.
    fn seek(&mut self, key: u64) -> Option<u64>;
    /// Advance past the current key to the next key. Returns the new
    /// current key, or `None` if exhausted.
    fn next(&mut self) -> Option<u64>;
}

/// A simple `SortedIterator` over a sorted slice of `u64` keys.
///
/// The slice must be sorted in strictly ascending order (no duplicates).
/// The iterator does not validate this — callers must ensure it.
pub struct SliceSortedIterator<'a> {
    keys: &'a [u64],
    pos: usize,
}

impl<'a> SliceSortedIterator<'a> {
    /// Create a new iterator over the given sorted slice.
    ///
    /// The iterator starts positioned *before* the first key; the first
    /// call to `current_key` returns `None` until `seek(0)` or `next()` is
    /// called. To initialize the iterator at the first key, call
    /// [`Self::at_start`].
    #[must_use]
    pub fn new(keys: &'a [u64]) -> Self {
        Self { keys, pos: 0 }
    }

    /// Create a new iterator positioned at the first key (or exhausted if
    /// the slice is empty).
    #[must_use]
    pub fn at_start(keys: &'a [u64]) -> Self {
        Self { keys, pos: 0 }
    }
}

impl<'a> SortedIterator for SliceSortedIterator<'a> {
    fn current_key(&self) -> Option<u64> {
        if self.pos < self.keys.len() {
            Some(self.keys[self.pos])
        } else {
            None
        }
    }

    fn seek(&mut self, key: u64) -> Option<u64> {
        // Binary search for the first key ≥ `key`.
        // Since the slice is sorted ascending, we use `partition_point`.
        if self.pos >= self.keys.len() {
            return None;
        }
        // If the current key already satisfies the seek, no movement needed.
        if self.keys[self.pos] >= key {
            return Some(self.keys[self.pos]);
        }
        // Otherwise binary-search forward from the current position.
        let remaining = &self.keys[self.pos..];
        let off = remaining.partition_point(|&k| k < key);
        self.pos += off;
        self.current_key()
    }

    fn next(&mut self) -> Option<u64> {
        if self.pos < self.keys.len() {
            self.pos += 1;
        }
        self.current_key()
    }
}

/// Leapfrog triejoin: worst-case optimal, achieves the AGM bound.
///
/// Input: N sorted iterators on the same join key.
/// Output: all keys present in ALL iterators (intersection).
///
/// # Algorithm
///
/// 1. Initialize all iterators (call `seek(0)` on each so they have a
///    current key, or are exhausted).
/// 2. Loop:
///    a. Find `max_key = max(current_key)` across non-exhausted iterators.
///    b. If any iterator is exhausted, stop.
///    c. Seek every iterator to `≥ max_key`.
///    d. If all iterators are now at `max_key`, emit `max_key` and advance
///    one iterator (the first one). Continue.
///    e. Otherwise, the new max is larger; loop back to (a).
///
/// # Complexity
///
/// `O(Σ |Ri| · log |Ri|)` for the initial sort, plus `O(AGM)` for the join
/// itself — but since we assume the inputs are already sorted, the join
/// cost is `O(AGM)`. This is worst-case optimal.
pub struct LeapfrogJoin {
    iterators: Vec<Box<dyn SortedIterator>>,
}

impl LeapfrogJoin {
    /// Create a new leapfrog join over the given sorted iterators.
    ///
    /// Takes ownership of the iterators. The join consumes them during
    /// [`Self::run`].
    #[must_use]
    pub fn new(iterators: Vec<Box<dyn SortedIterator>>) -> Self {
        Self { iterators }
    }

    /// Run the join and return all matching keys.
    ///
    /// The keys are returned in ascending order (the leapfrog algorithm
    /// naturally emits them in sorted order).
    ///
    /// # Edge cases
    ///
    /// - Empty iterator list → returns `vec![]`.
    /// - Any single iterator exhausted at start → returns `vec![]`.
    /// - Single iterator → returns all of its keys.
    /// - All iterators share no keys → returns `vec![]`.
    pub fn run(&mut self) -> Vec<u64> {
        let n = self.iterators.len();
        if n == 0 {
            return Vec::new();
        }
        if n == 1 {
            // Single iterator → emit all keys.
            let mut out = Vec::new();
            // Position at start.
            if self.iterators[0].current_key().is_none() {
                self.iterators[0].seek(0);
            }
            while let Some(k) = self.iterators[0].current_key() {
                out.push(k);
                self.iterators[0].next();
            }
            return out;
        }

        // Initialize: position every iterator at its first key.
        for it in &mut self.iterators {
            if it.current_key().is_none() {
                it.seek(0);
            }
            // If any iterator is empty after seek, the intersection is empty.
            if it.current_key().is_none() {
                return Vec::new();
            }
        }

        let mut out = Vec::new();
        loop {
            // Step 1: find the max current key across all iterators.
            let mut max_key = u64::MIN;
            for it in &self.iterators {
                match it.current_key() {
                    Some(k) if k > max_key => max_key = k,
                    None => return out, // exhausted
                    _ => {}
                }
            }

            // Step 2: seek every iterator to ≥ max_key. Track whether they
            // all converge on max_key (a match) or some iterator moves past
            // (a miss → retry with the new max).
            let mut all_match = true;
            for it in &mut self.iterators {
                match it.current_key() {
                    Some(k) if k == max_key => {
                        // Already at max_key — no seek needed.
                    }
                    Some(k) if k > max_key => {
                        // Impossible: max_key is the max. (Defensive.)
                        // Treat as a miss.
                        all_match = false;
                    }
                    _ => {
                        // Need to seek.
                        match it.seek(max_key) {
                            Some(k) if k == max_key => {
                                // Converged on max_key.
                            }
                            Some(k) => {
                                // Landed past max_key — this is a miss; the
                                // new max will be at least `k` on the next
                                // iteration.
                                if k > max_key {
                                    max_key = k;
                                }
                                all_match = false;
                            }
                            None => return out, // exhausted
                        }
                    }
                }
            }

            if all_match {
                // All iterators at max_key → emit it and advance one.
                out.push(max_key);
                // Advance the first iterator (any one would do).
                if self.iterators[0].next().is_none() {
                    return out;
                }
            }
            // Else: loop back with the new (larger) max_key.
        }
    }
}

// ---------------------------------------------------------------------------
// Kernel trait implementation (for table registration)
// ---------------------------------------------------------------------------

/// Scalar leapfrog join kernel — wraps the leapfrog algorithm in a
/// [`Kernel`] trait implementation so it can be looked up in the kernel
/// table.
///
/// ## Input encoding
///
/// Because the [`Kernel`] trait takes a single `input` pointer, this kernel
/// encodes a **2-way** leapfrog join: the input buffer holds the two slices
/// concatenated, with the boundary passed via `params.cell_count` (length
/// of the first slice) and `params.target_u64` reinterpreted as the length
/// of the second slice (cast to `usize`).
///
/// The kernel runs the leapfrog intersection and stores the count of
/// matching keys in `KernelResult::count`. The matching keys themselves
/// are not returned through the result struct (it has no vector slot) —
/// callers wanting the keys should use [`LeapfrogJoin`] directly.
///
/// This kernel exists primarily for symmetry with the rest of the kernel
/// table and for the planner's introspection. Real leapfrog joins with
/// more than 2 inputs use the standalone [`LeapfrogJoin`] struct.
pub struct LeapfrogScalar;

impl Kernel for LeapfrogScalar {
    fn operator(&self) -> Operator {
        Operator::LeapfrogJoin
    }
    fn cpu(&self) -> CpuTarget {
        CpuTarget::Scalar
    }
    fn tier(&self) -> MemoryTier {
        MemoryTier::L3
    }
    fn name(&self) -> &'static str {
        "leapfrog_scalar"
    }
    unsafe fn execute(
        &self,
        input: *const u8,
        _output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult {
        // Decode the input: two concatenated u64 slices.
        // - First slice: `params.cell_count` cells starting at `input`.
        // - Second slice: `params.target_u64 as usize` cells starting at
        //   `input + cell_count * 8`.
        let n1 = params.cell_count;
        let n2 = params.target_u64 as usize;
        let total = n1 + n2;
        // Edge case: empty input. Avoid `slice::from_raw_parts(null, 0)`
        // which trips the debug precondition that the pointer is non-null.
        if total == 0 {
            return KernelResult { count: 0, sum: 0.0, mask: 0 };
        }
        // SAFETY: caller guarantees `input` points to `total * 8` readable bytes.
        let cells = std::slice::from_raw_parts(input as *const u64, total);
        let (left, right) = cells.split_at(n1);

        // Build sorted iterators. The slices are assumed pre-sorted; if not,
        // the leapfrog algorithm will silently produce wrong results.
        let it_left = SliceSortedIterator::at_start(left);
        let it_right = SliceSortedIterator::at_start(right);
        let mut join = LeapfrogJoin::new(vec![
            Box::new(it_left) as Box<dyn SortedIterator>,
            Box::new(it_right) as Box<dyn SortedIterator>,
        ]);
        let matches = join.run();
        KernelResult { count: matches.len() as u64, sum: 0.0, mask: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a `Box<dyn SortedIterator>` from a slice.
    fn boxed_iter(keys: &[u64]) -> Box<dyn SortedIterator> {
        // We need a 'static-ish lifetime for the trait object. In tests we
        // leak the slice to get a 'static reference; this is acceptable for
        // a test helper.
        let leaked: &'static [u64] = Box::leak(keys.to_vec().into_boxed_slice());
        Box::new(SliceSortedIterator::at_start(leaked))
    }

    /// Two sorted iterators [1,2,3,4,5] and [2,4,6] → intersection [2,4].
    #[test]
    fn leapfrog_two_iterators_intersection() {
        let mut join =
            LeapfrogJoin::new(vec![boxed_iter(&[1, 2, 3, 4, 5]), boxed_iter(&[2, 4, 6])]);
        let out = join.run();
        assert_eq!(out, vec![2, 4]);
    }

    /// Three iterators with disjoint keys → empty result.
    #[test]
    fn leapfrog_three_disjoint_iterators_empty() {
        let mut join = LeapfrogJoin::new(vec![
            boxed_iter(&[1, 2, 3]),
            boxed_iter(&[10, 20, 30]),
            boxed_iter(&[100, 200, 300]),
        ]);
        let out = join.run();
        assert!(out.is_empty(), "disjoint iterators should produce empty intersection");
    }

    /// Single iterator → returns all keys.
    #[test]
    fn leapfrog_single_iterator_returns_all() {
        let keys = vec![5u64, 10, 15, 20, 25];
        let mut join = LeapfrogJoin::new(vec![boxed_iter(&keys)]);
        let out = join.run();
        assert_eq!(out, keys);
    }

    /// Empty iterator → empty result.
    #[test]
    fn leapfrog_empty_iterator_empty_result() {
        let mut join = LeapfrogJoin::new(vec![
            boxed_iter(&[1, 2, 3]),
            boxed_iter(&[]),
            boxed_iter(&[2, 3, 4]),
        ]);
        let out = join.run();
        assert!(out.is_empty(), "any empty iterator should produce empty intersection");
    }

    /// No iterators at all → empty result.
    #[test]
    fn leapfrog_no_iterators_empty_result() {
        let mut join = LeapfrogJoin::new(vec![]);
        let out = join.run();
        assert!(out.is_empty());
    }

    /// Three iterators with partial overlap.
    /// [1,2,3,4,5] ∩ [2,3,4,5,6] ∩ [3,4,5,6,7] = [3,4,5]
    #[test]
    fn leapfrog_three_iterators_partial_overlap() {
        let mut join = LeapfrogJoin::new(vec![
            boxed_iter(&[1, 2, 3, 4, 5]),
            boxed_iter(&[2, 3, 4, 5, 6]),
            boxed_iter(&[3, 4, 5, 6, 7]),
        ]);
        let out = join.run();
        assert_eq!(out, vec![3, 4, 5]);
    }

    /// Identical iterators → intersection equals the iterator.
    #[test]
    fn leapfrog_identical_iterators() {
        let keys = vec![1u64, 5, 10, 15, 20, 100];
        let mut join =
            LeapfrogJoin::new(vec![boxed_iter(&keys), boxed_iter(&keys), boxed_iter(&keys)]);
        let out = join.run();
        assert_eq!(out, keys);
    }

    /// Leapfrog matches brute-force intersection on random data.
    #[test]
    fn leapfrog_matches_brute_force_random() {
        // Deterministic PRNG so the test is reproducible.
        let mut rng = 12345u64;
        let mut next = || {
            // Simple xorshift.
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for _trial in 0..20 {
            // Generate 3 random sorted iterators with keys in [0, 200).
            let mut iters: Vec<Vec<u64>> = Vec::new();
            for _ in 0..3 {
                let n = (next() % 30) as usize;
                let mut keys: Vec<u64> = (0..n).map(|_| next() % 200).collect();
                keys.sort_unstable();
                keys.dedup();
                iters.push(keys);
            }

            // Brute-force intersection: keys present in all three.
            let brute: Vec<u64> = iters[0]
                .iter()
                .filter(|&&k| {
                    iters[1].binary_search(&k).is_ok() && iters[2].binary_search(&k).is_ok()
                })
                .copied()
                .collect();

            // Leapfrog intersection.
            let mut join = LeapfrogJoin::new(vec![
                boxed_iter(&iters[0]),
                boxed_iter(&iters[1]),
                boxed_iter(&iters[2]),
            ]);
            let leap = join.run();

            assert_eq!(leap, brute, "trial mismatch: iters = {iters:?}");
        }
    }

    /// SliceSortedIterator seeks correctly.
    #[test]
    fn slice_iterator_seek() {
        let keys = [1u64, 5, 10, 15, 20, 25];
        let mut it = SliceSortedIterator::at_start(&keys);
        assert_eq!(it.current_key(), Some(1));
        // Seek to ≥ 12 → 15.
        assert_eq!(it.seek(12), Some(15));
        assert_eq!(it.current_key(), Some(15));
        // Seek backward (no-op since iterator only moves forward).
        assert_eq!(it.seek(5), Some(15));
        // Seek past end.
        assert_eq!(it.seek(100), None);
        assert_eq!(it.current_key(), None);
    }

    /// SliceSortedIterator next advances.
    #[test]
    fn slice_iterator_next() {
        let keys = [1u64, 5, 10];
        let mut it = SliceSortedIterator::at_start(&keys);
        assert_eq!(it.current_key(), Some(1));
        assert_eq!(it.next(), Some(5));
        assert_eq!(it.next(), Some(10));
        assert_eq!(it.next(), None);
        assert_eq!(it.next(), None); // stays exhausted
    }

    /// Empty slice iterator is exhausted from the start.
    #[test]
    fn slice_iterator_empty() {
        let keys: [u64; 0] = [];
        let mut it = SliceSortedIterator::at_start(&keys);
        assert_eq!(it.current_key(), None);
        assert_eq!(it.seek(0), None);
        assert_eq!(it.next(), None);
    }

    /// The scalar kernel wrapper counts matches correctly.
    #[test]
    fn leapfrog_scalar_kernel_counts_matches() {
        let left = vec![1u64, 2, 3, 4, 5];
        let right = vec![2u64, 4, 6];
        let mut buf: Vec<u8> = Vec::with_capacity((left.len() + right.len()) * 8);
        for &k in &left {
            buf.extend_from_slice(&k.to_le_bytes());
        }
        for &k in &right {
            buf.extend_from_slice(&k.to_le_bytes());
        }
        let params = KernelParams {
            cell_count: left.len(),
            target_u64: right.len() as u64,
            ..Default::default()
        };
        let mut output = [0u8; 64];
        let result = unsafe { LeapfrogScalar.execute(buf.as_ptr(), output.as_mut_ptr(), &params) };
        assert_eq!(result.count, 2, "intersection of [1,2,3,4,5] & [2,4,6] has 2 keys");
    }

    /// The scalar kernel handles empty inputs.
    #[test]
    fn leapfrog_scalar_kernel_empty() {
        // Use a non-null pointer even when there are zero cells, to avoid
        // tripping `slice::from_raw_parts`'s debug precondition. We pass
        // `&aligned_dummy` as the input pointer — it is never dereferenced
        // because `total == 0` triggers the early-return.
        let aligned_dummy: u64 = 0;
        let params = KernelParams { cell_count: 0, target_u64: 0, ..Default::default() };
        let mut output = [0u8; 64];
        let result = unsafe {
            LeapfrogScalar.execute(
                &aligned_dummy as *const u64 as *const u8,
                output.as_mut_ptr(),
                &params,
            )
        };
        assert_eq!(result.count, 0);
    }
}
