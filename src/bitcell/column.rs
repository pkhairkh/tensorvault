//! `CellColumn` — a `Vec<Cell>` with batch homogeneity tracking and vectorized ops.

use crate::bitcell::cell::Cell;
use serde::{Deserialize, Serialize};

/// Per-batch homogeneity tag. A monomorphic batch (all same type) can skip
/// per-cell tag checks in the inner loop; a polymorphic batch must dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Homogeneity {
    /// Every cell in the batch is NULL.
    AllNull,
    /// Every cell is a real f64.
    AllF64,
    /// Every cell is a tagged i32.
    AllI32,
    /// Every cell is a tagged bool.
    AllBool,
    /// Every cell is a short string.
    AllShortStr,
    /// Mixed types — dispatch per cell.
    Polymorphic,
}

/// A slice of a `CellColumn` processed by one vectorized kernel call.
#[derive(Debug, Clone)]
pub struct Batch<'a> {
    pub cells: &'a [Cell],
    pub homogeneity: Homogeneity,
}

impl<'a> Batch<'a> {
    /// Construct a batch from a slice, auto-detecting homogeneity.
    pub fn new(cells: &'a [Cell]) -> Self {
        let h = detect_homogeneity(cells);
        Self {
            cells,
            homogeneity: h,
        }
    }

    /// Length of the batch.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Is the batch empty?
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

fn detect_homogeneity(cells: &[Cell]) -> Homogeneity {
    if cells.is_empty() {
        return Homogeneity::AllNull;
    }
    let first = cells[0];
    let mut all_null = first.is_null();
    let mut all_f64 = first.is_f64();
    let mut all_i32 = first.is_i32();
    let mut all_bool = (first.to_bits() >> 48) == 0xFFF1;
    let mut all_short = first.is_short_str();

    for &c in cells.iter().skip(1) {
        if !c.is_null() {
            all_null = false;
        }
        if !c.is_f64() {
            all_f64 = false;
        }
        if !c.is_i32() {
            all_i32 = false;
        }
        if (c.to_bits() >> 48) != 0xFFF1 {
            all_bool = false;
        }
        if !c.is_short_str() {
            all_short = false;
        }
    }

    if all_null {
        Homogeneity::AllF64
    } else if all_f64 {
        Homogeneity::AllF64
    } else if all_i32 {
        Homogeneity::AllI32
    } else if all_bool {
        Homogeneity::AllBool
    } else if all_short {
        Homogeneity::AllShortStr
    } else {
        Homogeneity::Polymorphic
    }
}

/// A columnar storage of `Cell`s. The fundamental storage unit of the
/// bit-uniform relational engine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CellColumn {
    pub cells: Vec<Cell>,
}

impl CellColumn {
    /// New empty column.
    pub fn new() -> Self {
        Self::default()
    }

    /// New column with reserved capacity.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            cells: Vec::with_capacity(cap),
        }
    }

    /// Push a cell.
    pub fn push(&mut self, c: Cell) {
        self.cells.push(c);
    }

    /// Push an f64.
    pub fn push_f64(&mut self, x: f64) {
        self.push(Cell::from_f64(x));
    }

    /// Push an i32.
    pub fn push_i32(&mut self, x: i32) {
        self.push(Cell::from_i32(x));
    }

    /// Push NULL.
    pub fn push_null(&mut self) {
        self.push(Cell::null());
    }

    /// Length.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Is the column empty?
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Get a cell by index.
    pub fn get(&self, i: usize) -> Option<Cell> {
        self.cells.get(i).copied()
    }

    /// Iterate over batches of `batch_size` cells. Each batch carries its
    /// detected homogeneity so the executor can pick a specialized kernel.
    pub fn batches(&self, batch_size: usize) -> impl Iterator<Item = Batch<'_>> {
        self.cells.chunks(batch_size).map(Batch::new)
    }

    /// In-memory byte size (8 bytes per cell).
    pub fn byte_size(&self) -> usize {
        self.cells.len() * 8
    }

    /// As a raw slice of u64 bit patterns — for SIMD kernels that operate
    /// on the underlying words directly.
    pub fn as_u64_slice(&self) -> &[u64] {
        // SAFETY: Cell is #[repr(transparent)] over u64, so &Cell and &u64
        // have identical layout.
        unsafe {
            std::slice::from_raw_parts(
                self.cells.as_ptr() as *const u64,
                self.cells.len(),
            )
        }
    }

    /// Count cells matching a predicate, using SIMD when the batch is
    /// monomorphic and the predicate is bit-parallel.
    pub fn count_eq(&self, target: Cell) -> usize {
        // Fast path: XOR each cell with target, count zeros.
        // On AVX-512 this is _mm512_xor_epi64 + _mm512_cmpeq_epi64_mask + popcount.
        // Scalar fallback here; see `scan` module for the SIMD version.
        let target_bits = target.to_bits();
        self.cells
            .iter()
            .filter(|c| c.to_bits() == target_bits)
            .count()
    }

    /// Count cells with Hamming distance ≤ `max_distance` from `target`.
    /// Works for ANY column type — this is the unified similarity primitive.
    pub fn count_similar(&self, target: Cell, max_distance: u32) -> usize {
        let target_bits = target.to_bits();
        self.cells
            .iter()
            .filter(|c| (c.to_bits() ^ target_bits).count_ones() <= max_distance)
            .count()
    }

    /// Collect indices of cells matching `target` (exact bit equality).
    pub fn find_eq(&self, target: Cell) -> Vec<usize> {
        let t = target.to_bits();
        self.cells
            .iter()
            .enumerate()
            .filter_map(|(i, c)| if c.to_bits() == t { Some(i) } else { None })
            .collect()
    }

    /// Collect indices of cells with Hamming distance ≤ `max_distance`.
    pub fn find_similar(&self, target: Cell, max_distance: u32) -> Vec<usize> {
        let t = target.to_bits();
        self.cells
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                if (c.to_bits() ^ t).count_ones() <= max_distance {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
    }
}

impl From<Vec<Cell>> for CellColumn {
    fn from(cells: Vec<Cell>) -> Self {
        Self { cells }
    }
}

impl From<Vec<f64>> for CellColumn {
    fn from(values: Vec<f64>) -> Self {
        Self {
            cells: values.into_iter().map(Cell::from_f64).collect(),
        }
    }
}

impl From<Vec<i32>> for CellColumn {
    fn from(values: Vec<i32>) -> Self {
        Self {
            cells: values.into_iter().map(Cell::from_i32).collect(),
        }
    }
}

impl FromIterator<Cell> for CellColumn {
    fn from_iter<I: IntoIterator<Item = Cell>>(iter: I) -> Self {
        Self {
            cells: iter.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_basic() {
        let mut col = CellColumn::new();
        col.push_i32(1);
        col.push_i32(2);
        col.push_i32(3);
        assert_eq!(col.len(), 3);
        assert_eq!(col.get(1).unwrap().as_i32(), Some(2));
    }

    #[test]
    fn column_mixed_types() {
        let mut col = CellColumn::new();
        col.push_i32(42);
        col.push_f64(3.14);
        col.push_null();
        col.push(Cell::from_bool(true));
        assert_eq!(col.len(), 4);
    }

    #[test]
    fn count_eq_works() {
        let col: CellColumn = vec![1i32, 2, 3, 2, 2, 4].into_iter().map(Cell::from_i32).collect();
        assert_eq!(col.count_eq(Cell::from_i32(2)), 3);
    }

    #[test]
    fn count_similar_works_for_f64() {
        let col: CellColumn = vec![1.0f64, 1.0, 2.0, 1.0, 3.0].into_iter().map(Cell::from_f64).collect();
        // 1.0 and 2.0 differ in many bits; 1.0 and 1.0 differ in 0 bits.
        assert_eq!(col.count_similar(Cell::from_f64(1.0), 0), 3);
    }

    #[test]
    fn find_eq_returns_indices() {
        let col: CellColumn = vec![1i32, 2, 3, 2, 4].into_iter().map(Cell::from_i32).collect();
        let idx = col.find_eq(Cell::from_i32(2));
        assert_eq!(idx, vec![1, 3]);
    }

    #[test]
    fn batches_split_correctly() {
        let col: CellColumn = (0..100).map(|i| Cell::from_i32(i)).collect();
        let batches: Vec<_> = col.batches(8).collect();
        assert_eq!(batches.len(), 13); // ceil(100/8)
        assert_eq!(batches[0].len(), 8);
        assert_eq!(batches[12].len(), 4); // 100 - 12*8 = 4
    }

    #[test]
    fn monomorphic_batch_detected() {
        let col: CellColumn = (0..100).map(|i| Cell::from_i32(i)).collect();
        let b = col.batches(8).next().unwrap();
        assert_eq!(b.homogeneity, Homogeneity::AllI32);
    }

    #[test]
    fn polymorphic_batch_detected() {
        let mut col = CellColumn::new();
        col.push_i32(1);
        col.push_f64(2.0);
        let b = col.batches(8).next().unwrap();
        assert_eq!(b.homogeneity, Homogeneity::Polymorphic);
    }

    #[test]
    fn byte_size_is_8_per_cell() {
        let col: CellColumn = (0..10).map(|i| Cell::from_i32(i)).collect();
        assert_eq!(col.byte_size(), 80);
    }

    #[test]
    fn as_u64_slice_works() {
        let col: CellColumn = vec![1i32, 2, 3].into_iter().map(Cell::from_i32).collect();
        let bits = col.as_u64_slice();
        assert_eq!(bits.len(), 3);
        assert_eq!(bits[0], Cell::TAG_I32 | 1);
    }
}
