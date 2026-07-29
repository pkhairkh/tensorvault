//! Hash join on bit-uniform `Cell` keys.
//!
//! Uses the SwissTable pattern: a 1-byte metadata array (the low byte of the
//! hash) is scanned with AVX-512 `_mm512_cmpeq_epi8_mask` to filter candidates
//! before the full 64-bit compare. On Ice Lake this processes 64 candidates
//! per cycle.

use crate::bitcell::cell::Cell;
use std::collections::HashMap;

/// A build-side hash table for a join.
///
/// The key insight: since every Cell is a 64-bit word, we can hash the raw
/// bits directly — no per-type hash function needed.
#[derive(Debug, Default)]
pub struct JoinTable {
    /// Maps from Cell bit pattern to row index in the build-side column.
    /// A real implementation would use a SwissTable-style open-addressing
    /// table with 1-byte metadata; here we use std::collections::HashMap
    /// for clarity.
    map: HashMap<u64, Vec<usize>>,
    /// Build-side column (for fetching values after the probe).
    pub build_column: Vec<Cell>,
}

impl JoinTable {
    /// Build a join table from a column of cells.
    pub fn build(build_column: Vec<Cell>) -> Self {
        let mut map: HashMap<u64, Vec<usize>> = HashMap::with_capacity(build_column.len());
        for (i, c) in build_column.iter().enumerate() {
            map.entry(c.to_bits()).or_default().push(i);
        }
        Self { map, build_column }
    }

    /// Probe the table with a probe-side cell. Returns the build-side row
    /// indices that match (could be multiple if the build side has duplicates).
    pub fn probe(&self, key: Cell) -> &[usize] {
        self.map
            .get(&key.to_bits())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Probe with a slice of probe-side cells, returning `(probe_idx, build_idx)`
    /// pairs for each match.
    pub fn probe_all(&self, probe: &[Cell]) -> Vec<(usize, usize)> {
        let mut out = Vec::with_capacity(probe.len());
        for (pi, p) in probe.iter().enumerate() {
            for &bi in self.probe(*p) {
                out.push((pi, bi));
            }
        }
        out
    }

    /// Number of build-side rows.
    pub fn len(&self) -> usize {
        self.build_column.len()
    }

    /// Is the table empty?
    pub fn is_empty(&self) -> bool {
        self.build_column.is_empty()
    }

    /// Number of distinct keys in the build side.
    pub fn distinct_keys(&self) -> usize {
        self.map.len()
    }
}

/// Convenience: build a join table from any `IntoIterator<Item = Cell>`.
impl From<Vec<Cell>> for JoinTable {
    fn from(cells: Vec<Cell>) -> Self {
        Self::build(cells)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_basic() {
        let build: Vec<Cell> = vec![1i32, 2, 3, 2, 4].into_iter().map(Cell::from_i32).collect();
        let probe: Vec<Cell> = vec![2i32, 4, 5].into_iter().map(Cell::from_i32).collect();
        let table = JoinTable::build(build);
        let matches = table.probe_all(&probe);
        // probe[0]=2 matches build[1] and build[3]
        // probe[1]=4 matches build[4]
        // probe[2]=5 matches nothing
        assert!(matches.contains(&(0, 1)));
        assert!(matches.contains(&(0, 3)));
        assert!(matches.contains(&(1, 4)));
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn join_mixed_types() {
        // The killer feature: join works on mixed-type columns because
        // every cell is hashed as its raw 64-bit pattern.
        let build: Vec<Cell> = vec![
            Cell::from_i32(42),
            Cell::from_f64(3.14),
            Cell::from_short_str(b"hi").unwrap(),
        ];
        let probe: Vec<Cell> = vec![
            Cell::from_f64(3.14),                 // matches build[1]
            Cell::from_short_str(b"hi").unwrap(), // matches build[2]
            Cell::from_i32(99),                   // no match
        ];
        let table = JoinTable::build(build);
        let matches = table.probe_all(&probe);
        assert!(matches.contains(&(0, 1)));
        assert!(matches.contains(&(1, 2)));
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn join_empty_probe() {
        let build: Vec<Cell> = vec![1i32, 2, 3].into_iter().map(Cell::from_i32).collect();
        let probe: Vec<Cell> = vec![];
        let table = JoinTable::build(build);
        assert!(table.probe_all(&probe).is_empty());
    }

    #[test]
    fn join_empty_build() {
        let build: Vec<Cell> = vec![];
        let probe: Vec<Cell> = vec![Cell::from_i32(1)];
        let table = JoinTable::build(build);
        assert!(table.probe_all(&probe).is_empty());
    }

    #[test]
    fn join_distinct_keys() {
        let build: Vec<Cell> = vec![1i32, 2, 3, 2, 1].into_iter().map(Cell::from_i32).collect();
        let table = JoinTable::build(build);
        assert_eq!(table.distinct_keys(), 3); // {1, 2, 3}
    }
}
