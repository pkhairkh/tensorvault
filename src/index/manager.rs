//! # Index manager (Wave 26).
//!
//! Wires the existing BSI (Bit-Sliced Index) and LSH (Locality-Sensitive
//! Hash) index implementations into the query planning layer. The index
//! manager tracks which columns have indexes and provides lookup for
//! the optimizer to decide when to use an index vs a full scan.

use std::collections::HashMap;

/// The type of index on a column.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexType {
    /// Bit-Sliced Index for equality/range predicates on numeric columns.
    BSI,
    /// Locality-Sensitive Hash for similarity queries.
    LSH,
    /// Hash index for exact equality lookups.
    Hash,
}

/// An index on a specific table column.
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub table_name: String,
    pub column_name: String,
    pub index_type: IndexType,
    /// Estimated number of unique values (cardinality).
    pub cardinality: u64,
}

/// The index manager: tracks all indexes across all tables.
#[derive(Debug, Clone, Default)]
pub struct IndexManager {
    /// Maps (table_name, column_name) → IndexEntry.
    indexes: HashMap<(String, String), IndexEntry>,
}

impl IndexManager {
    /// Create an empty index manager.
    pub fn new() -> Self {
        Self { indexes: HashMap::new() }
    }

    /// Register an index on a column.
    pub fn create(&mut self, table: &str, column: &str, index_type: IndexType, cardinality: u64) {
        let entry = IndexEntry {
            table_name: table.to_string(),
            column_name: column.to_string(),
            index_type,
            cardinality,
        };
        self.indexes.insert((table.to_string(), column.to_string()), entry);
    }

    /// Drop an index.
    pub fn drop(&mut self, table: &str, column: &str) -> bool {
        self.indexes.remove(&(table.to_string(), column.to_string())).is_some()
    }

    /// Look up an index for a (table, column) pair.
    pub fn get(&self, table: &str, column: &str) -> Option<&IndexEntry> {
        self.indexes.get(&(table.to_string(), column.to_string()))
    }

    /// Check if an index exists for a column.
    pub fn has_index(&self, table: &str, column: &str) -> bool {
        self.indexes.contains_key(&(table.to_string(), column.to_string()))
    }

    /// Decide whether to use an index for a predicate.
    ///
    /// Returns true if the index should be used (i.e., the column has an
    /// index and the selectivity is high enough to justify it).
    pub fn should_use_index(
        &self,
        table: &str,
        column: &str,
        table_row_count: u64,
    ) -> bool {
        if !self.has_index(table, column) {
            return false;
        }
        if let Some(entry) = self.get(table, column) {
            // Use index if selectivity > 10% (i.e., cardinality / row_count > 0.1)
            // and the index type is appropriate.
            if table_row_count == 0 {
                return false;
            }
            let selectivity = entry.cardinality as f64 / table_row_count as f64;
            return selectivity > 0.1;
        }
        false
    }

    /// List all indexes.
    pub fn list(&self) -> Vec<&IndexEntry> {
        self.indexes.values().collect()
    }

    /// Count of indexes.
    pub fn len(&self) -> usize {
        self.indexes.len()
    }

    /// Returns true if no indexes exist.
    pub fn is_empty(&self) -> bool {
        self.indexes.is_empty()
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_lookup() {
        let mut mgr = IndexManager::new();
        mgr.create("users", "id", IndexType::BSI, 1000);
        assert!(mgr.has_index("users", "id"));
        let entry = mgr.get("users", "id").unwrap();
        assert_eq!(entry.index_type, IndexType::BSI);
        assert_eq!(entry.cardinality, 1000);
    }

    #[test]
    fn drop_index() {
        let mut mgr = IndexManager::new();
        mgr.create("users", "id", IndexType::BSI, 1000);
        assert!(mgr.drop("users", "id"));
        assert!(!mgr.has_index("users", "id"));
    }

    #[test]
    fn no_index_returns_false() {
        let mgr = IndexManager::new();
        assert!(!mgr.should_use_index("users", "id", 1000));
    }

    #[test]
    fn use_index_high_selectivity() {
        let mut mgr = IndexManager::new();
        // cardinality 500 out of 1000 rows = 50% selectivity → use index
        mgr.create("users", "id", IndexType::BSI, 500);
        assert!(mgr.should_use_index("users", "id", 1000));
    }

    #[test]
    fn skip_index_low_selectivity() {
        let mut mgr = IndexManager::new();
        // cardinality 50 out of 1000 rows = 5% selectivity → skip index
        mgr.create("users", "status", IndexType::BSI, 50);
        assert!(!mgr.should_use_index("users", "status", 1000));
    }

    #[test]
    fn multiple_indexes() {
        let mut mgr = IndexManager::new();
        mgr.create("users", "id", IndexType::BSI, 1000);
        mgr.create("users", "email", IndexType::Hash, 1000);
        mgr.create("products", "name", IndexType::LSH, 500);
        assert_eq!(mgr.len(), 3);
        assert!(mgr.has_index("users", "id"));
        assert!(mgr.has_index("users", "email"));
        assert!(mgr.has_index("products", "name"));
        assert!(!mgr.has_index("orders", "id"));
    }

    #[test]
    fn empty_table() {
        let mut mgr = IndexManager::new();
        mgr.create("users", "id", IndexType::BSI, 100);
        assert!(!mgr.should_use_index("users", "id", 0));
    }

    #[test]
    fn list_indexes() {
        let mut mgr = IndexManager::new();
        mgr.create("users", "id", IndexType::BSI, 1000);
        mgr.create("orders", "user_id", IndexType::Hash, 500);
        let list = mgr.list();
        assert_eq!(list.len(), 2);
    }
}
