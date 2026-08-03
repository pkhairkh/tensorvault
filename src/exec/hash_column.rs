//! # Materialized hash columns (Wave 27).
//!
//! Pre-computes the xxh3 hash for string columns at load time and stores
//! it alongside the StringSearchColumn. This eliminates the per-query
//! re-hashing cost that causes the 6-7x performance gap on ClickBench
//! Q14-Q42 (GROUP BY URL).
//!
//! The materialized hash is a `Vec<u64>` parallel to the string column.
//! When a query does `GROUP BY url`, the executor can use the pre-computed
//! hash directly instead of calling `xxh3_64()` on every string per query.

use std::collections::HashMap;

/// A materialized hash column: pre-computed xxh3 hashes for a string column.
#[derive(Debug, Clone)]
pub struct HashColumn {
    /// The pre-computed xxh3_64 hashes, one per row.
    pub hashes: Vec<u64>,
}

impl HashColumn {
    /// Build a hash column from a list of strings.
    pub fn from_strings(strings: &[String]) -> Self {
        use xxhash_rust::xxh3;
        let hashes: Vec<u64> = strings.iter().map(|s| xxh3::xxh3_64(s.as_bytes())).collect();
        Self { hashes }
    }

    /// Build a hash column from string slices.
    pub fn from_strs(strings: &[&str]) -> Self {
        use xxhash_rust::xxh3;
        let hashes: Vec<u64> = strings.iter().map(|s| xxh3::xxh3_64(s.as_bytes())).collect();
        Self { hashes }
    }

    /// Get the hash at a specific row index.
    pub fn get(&self, idx: usize) -> u64 {
        self.hashes.get(idx).copied().unwrap_or(0)
    }

    /// Number of hashes.
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    /// Returns true if empty.
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Build a GROUP BY map from the hash column. Returns a map from
    /// hash value → list of row indices with that hash.
    pub fn group_by_hash(&self) -> HashMap<u64, Vec<usize>> {
        let mut groups: HashMap<u64, Vec<usize>> = HashMap::new();
        for (idx, &h) in self.hashes.iter().enumerate() {
            groups.entry(h).or_default().push(idx);
        }
        groups
    }

    /// Count distinct hash values.
    pub fn count_distinct(&self) -> usize {
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for &h in &self.hashes {
            seen.insert(h);
        }
        seen.len()
    }
}

/// A registry of materialized hash columns per (table, column) pair.
#[derive(Debug, Clone, Default)]
pub struct HashColumnRegistry {
    columns: HashMap<(String, String), HashColumn>,
}

impl HashColumnRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self { columns: HashMap::new() }
    }

    /// Register a materialized hash column.
    pub fn register(&mut self, table: &str, column: &str, hash_col: HashColumn) {
        self.columns.insert((table.to_string(), column.to_string()), hash_col);
    }

    /// Look up a materialized hash column.
    pub fn get(&self, table: &str, column: &str) -> Option<&HashColumn> {
        self.columns.get(&(table.to_string(), column.to_string()))
    }

    /// Check if a materialized hash column exists.
    pub fn has(&self, table: &str, column: &str) -> bool {
        self.columns.contains_key(&(table.to_string(), column.to_string()))
    }

    /// Remove a materialized hash column.
    pub fn remove(&mut self, table: &str, column: &str) -> bool {
        self.columns.remove(&(table.to_string(), column.to_string())).is_some()
    }

    /// Count of registered hash columns.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns true if empty.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_strings_basic() {
        let hc = HashColumn::from_strings(&["alice".into(), "bob".into(), "charlie".into()]);
        assert_eq!(hc.len(), 3);
        // The hashes should be non-zero (xxh3 of non-empty strings).
        assert_ne!(hc.get(0), 0);
        assert_ne!(hc.get(1), 0);
        assert_ne!(hc.get(2), 0);
        // Different strings should produce different hashes.
        assert_ne!(hc.get(0), hc.get(1));
    }

    #[test]
    fn from_strs_basic() {
        let hc = HashColumn::from_strs(&["a", "b", "c"]);
        assert_eq!(hc.len(), 3);
        assert_ne!(hc.get(0), hc.get(1));
    }

    #[test]
    fn group_by_hash() {
        let hc = HashColumn::from_strs(&["a", "b", "a", "c", "b"]);
        let groups = hc.group_by_hash();
        // 3 distinct values: "a", "b", "c"
        assert_eq!(groups.len(), 3);
        // "a" appears at indices 0, 2
        let a_hash = xxhash_rust::xxh3::xxh3_64(b"a");
        assert_eq!(groups.get(&a_hash).unwrap(), &vec![0, 2]);
    }

    #[test]
    fn count_distinct() {
        let hc = HashColumn::from_strs(&["a", "b", "a", "c", "b", "a"]);
        assert_eq!(hc.count_distinct(), 3);
    }

    #[test]
    fn empty_column() {
        let hc = HashColumn::from_strs(&[]);
        assert!(hc.is_empty());
        assert_eq!(hc.count_distinct(), 0);
    }

    #[test]
    fn registry_crud() {
        let mut reg = HashColumnRegistry::new();
        let hc = HashColumn::from_strs(&["a", "b", "c"]);
        reg.register("users", "name", hc);
        assert!(reg.has("users", "name"));
        assert_eq!(reg.len(), 1);

        let hc = reg.get("users", "name").unwrap();
        assert_eq!(hc.len(), 3);

        assert!(reg.remove("users", "name"));
        assert!(!reg.has("users", "name"));
    }

    #[test]
    fn registry_multiple_columns() {
        let mut reg = HashColumnRegistry::new();
        reg.register("users", "name", HashColumn::from_strs(&["a", "b"]));
        reg.register("users", "email", HashColumn::from_strs(&["x@y.com", "z@w.com"]));
        reg.register("orders", "id", HashColumn::from_strs(&["1", "2"]));
        assert_eq!(reg.len(), 3);
        assert!(reg.has("users", "name"));
        assert!(reg.has("users", "email"));
        assert!(reg.has("orders", "id"));
        assert!(!reg.has("orders", "name"));
    }

    #[test]
    fn hash_stability() {
        // The same string should always produce the same hash.
        let hc1 = HashColumn::from_strs(&["test"]);
        let hc2 = HashColumn::from_strs(&["test"]);
        assert_eq!(hc1.get(0), hc2.get(0));
    }

    #[test]
    fn large_column() {
        let strings: Vec<String> = (0..1000).map(|i| format!("string_{i}")).collect();
        let hc = HashColumn::from_strings(&strings);
        assert_eq!(hc.len(), 1000);
        assert_eq!(hc.count_distinct(), 1000); // all unique
    }
}
