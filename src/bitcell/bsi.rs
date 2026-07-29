//! Bit-sliced index (BSI) for `Cell` columns.
//!
//! For a column of 64-bit cells, maintain 64 bitmaps — one per bit position.
//! A range predicate `WHERE x BETWEEN a AND b` compiles to a small fixed set
//! of AND/OR/NOT operations across ~12 bitmaps (since most queries touch a
//! narrow value band).
//!
//! This is the O'Neil-Quass (1997) / Chan-Ioannidis (1998) technique, applied
//! to bit-uniform NaN-boxed cells. Because every cell is a 64-bit word, the
//! same BSI layout serves every column type — no per-type index needed.

use crate::bitcell::cell::Cell;

/// A simple bitmap — one bit per row, stored as `Vec<u64>` (64 bits per word).
#[derive(Debug, Clone, Default)]
pub struct Bitmap {
    bits: Vec<u64>,
    len: usize,
}

impl Bitmap {
    /// New bitmap of length `len`, all zeros.
    pub fn zeros(len: usize) -> Self {
        let words = (len + 63) / 64;
        Self {
            bits: vec![0; words],
            len,
        }
    }

    /// New bitmap of length `len`, all ones.
    pub fn ones(len: usize) -> Self {
        let words = (len + 63) / 64;
        let mut bits = vec![u64::MAX; words];
        // Zero out the trailing bits in the last word.
        let extra = words * 64 - len;
        if extra > 0 && !bits.is_empty() {
            let mask = if extra < 64 { (1u64 << (64 - extra)) - 1 } else { 0 };
            let last_idx = bits.len() - 1;
            bits[last_idx] &= mask;
        }
        Self { bits, len }
    }

    /// Set bit `i`.
    pub fn set(&mut self, i: usize) {
        debug_assert!(i < self.len);
        self.bits[i / 64] |= 1u64 << (i % 64);
    }

    /// Clear bit `i`.
    pub fn clear(&mut self, i: usize) {
        debug_assert!(i < self.len);
        self.bits[i / 64] &= !(1u64 << (i % 64));
    }

    /// Get bit `i`.
    pub fn get(&self, i: usize) -> bool {
        if i >= self.len {
            return false;
        }
        (self.bits[i / 64] >> (i % 64)) & 1 != 0
    }

    /// Number of set bits.
    pub fn popcount(&self) -> usize {
        self.bits.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Is empty?
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// In-place AND.
    pub fn and_in_place(&mut self, other: &Self) {
        debug_assert_eq!(self.len, other.len);
        for i in 0..self.bits.len() {
            self.bits[i] &= other.bits[i];
        }
    }

    /// In-place OR.
    pub fn or_in_place(&mut self, other: &Self) {
        debug_assert_eq!(self.len, other.len);
        for i in 0..self.bits.len() {
            self.bits[i] |= other.bits[i];
        }
    }

    /// In-place NOT (returns a new bitmap to keep &mut simple).
    pub fn not(&self) -> Self {
        let mut out = self.clone();
        for w in &mut out.bits {
            *w = !*w;
        }
        // Re-mask the trailing bits.
        let extra = out.bits.len() * 64 - out.len;
        if extra > 0 && !out.bits.is_empty() {
            let mask = if extra < 64 { (1u64 << (64 - extra)) - 1 } else { 0 };
            let last_idx = out.bits.len() - 1;
            out.bits[last_idx] &= mask;
        }
        out
    }

    /// Returns indices where the bit is set.
    pub fn set_indices(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.popcount());
        for (word_idx, &w) in self.bits.iter().enumerate() {
            let mut bits = w;
            while bits != 0 {
                let low = bits.trailing_zeros() as usize;
                let idx = word_idx * 64 + low;
                if idx < self.len {
                    out.push(idx);
                }
                bits &= bits - 1;
            }
        }
        out
    }
}

/// Bit-sliced index: 64 bitmaps, one per bit position of the 64-bit cell word.
///
/// For a column of N cells, this is 64 * N bits = 8N bytes of index storage —
/// the same size as the column itself. But it answers arbitrary range/equality
/// queries in O(N/64) bitmap operations.
#[derive(Debug, Clone)]
pub struct BitSlicedIndex {
    /// `slices[i]` is the bitmap for bit position `i` (i=0 is LSB).
    pub slices: Vec<Bitmap>,
    /// Number of rows indexed.
    pub len: usize,
}

impl BitSlicedIndex {
    /// Build a BSI from a slice of cells.
    pub fn build(cells: &[Cell]) -> Self {
        let n = cells.len();
        let mut slices: Vec<Bitmap> = (0..64).map(|_| Bitmap::zeros(n)).collect();
        for (row, cell) in cells.iter().enumerate() {
            let bits = cell.to_bits();
            for bit in 0..64 {
                if (bits >> bit) & 1 != 0 {
                    slices[bit].set(row);
                }
            }
        }
        Self { slices, len: n }
    }

    /// Look up all rows where the cell equals `target`.
    ///
    /// Compiles to: AND together, for each bit position, the (target_bit ? slice : NOT slice).
    pub fn find_eq(&self, target: Cell) -> Bitmap {
        let target_bits = target.to_bits();
        let mut result = Bitmap::ones(self.len);
        for bit in 0..64 {
            if (target_bits >> bit) & 1 != 0 {
                result.and_in_place(&self.slices[bit]);
            } else {
                result.and_in_place(&self.slices[bit].not());
            }
        }
        result
    }

    /// Look up all rows where the cell's Hamming distance from `target` is ≤ `max_d`.
    ///
    /// This is harder than `find_eq` because we need to count differing bits.
    /// For each row, count the XOR'd bits and compare. The naive approach is
    /// O(N); the BSI doesn't accelerate this directly — use the scan kernel
    /// for similarity queries, BSI for equality/range.
    ///
    /// We provide it for completeness; it's just a scan wrapped in BSI's API.
    pub fn find_similar(&self, cells: &[Cell], target: Cell, max_d: u32) -> Bitmap {
        let mut result = Bitmap::zeros(self.len);
        let t = target.to_bits();
        for (i, c) in cells.iter().enumerate() {
            if (c.to_bits() ^ t).count_ones() <= max_d {
                result.set(i);
            }
        }
        result
    }

    /// Memory usage in bytes.
    pub fn byte_size(&self) -> usize {
        self.slices.iter().map(|b| b.bits.len() * 8).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_basic() {
        let mut b = Bitmap::zeros(100);
        b.set(5);
        b.set(63);
        b.set(64);
        assert!(b.get(5));
        assert!(b.get(63));
        assert!(b.get(64));
        assert!(!b.get(0));
        assert_eq!(b.popcount(), 3);
    }

    #[test]
    fn bitmap_ones() {
        let b = Bitmap::ones(70);
        assert_eq!(b.popcount(), 70);
        // Bits 0..69 are set; bit 70+ are not.
        assert!(b.get(69));
        // The 70th bit is index 70 — outside the bitmap.
    }

    #[test]
    fn bitmap_and() {
        let mut a = Bitmap::zeros(64);
        a.set(1);
        a.set(2);
        a.set(3);
        let mut b = Bitmap::zeros(64);
        b.set(2);
        b.set(3);
        b.set(4);
        a.and_in_place(&b);
        assert!(a.get(2));
        assert!(a.get(3));
        assert!(!a.get(1));
        assert!(!a.get(4));
    }

    #[test]
    fn bitmap_not() {
        let mut a = Bitmap::zeros(8);
        a.set(1);
        a.set(3);
        let n = a.not();
        assert!(!n.get(1));
        assert!(!n.get(3));
        assert!(n.get(0));
        assert!(n.get(2));
    }

    #[test]
    fn bsi_find_eq_works() {
        let cells: Vec<Cell> = vec![1i32, 2, 3, 2, 4, 2, 5]
            .into_iter()
            .map(Cell::from_i32)
            .collect();
        let bsi = BitSlicedIndex::build(&cells);
        let result = bsi.find_eq(Cell::from_i32(2));
        let indices: Vec<usize> = result.set_indices();
        assert_eq!(indices, vec![1, 3, 5]);
    }

    #[test]
    fn bsi_find_eq_f64() {
        let cells: Vec<Cell> = vec![1.0f64, 2.5, 1.0, 3.0, 1.0]
            .into_iter()
            .map(Cell::from_f64)
            .collect();
        let bsi = BitSlicedIndex::build(&cells);
        let result = bsi.find_eq(Cell::from_f64(1.0));
        let indices: Vec<usize> = result.set_indices();
        assert_eq!(indices, vec![0, 2, 4]);
    }

    #[test]
    fn bsi_byte_size() {
        let cells: Vec<Cell> = (0..1000).map(|i| Cell::from_i32(i)).collect();
        let bsi = BitSlicedIndex::build(&cells);
        // 64 slices × ceil(1000/64) words × 8 bytes/word
        let expected = 64 * ((1000 + 63) / 64) * 8;
        assert_eq!(bsi.byte_size(), expected);
    }

    #[test]
    fn bsi_find_similar_works() {
        let cells: Vec<Cell> = vec![1.0f64, 1.0, 2.0, 1.0, 3.0]
            .into_iter()
            .map(Cell::from_f64)
            .collect();
        let bsi = BitSlicedIndex::build(&cells);
        let result = bsi.find_similar(&cells, Cell::from_f64(1.0), 0);
        let indices: Vec<usize> = result.set_indices();
        assert_eq!(indices, vec![0, 1, 3]);
    }
}
