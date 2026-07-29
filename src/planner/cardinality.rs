//! Cardinality estimation for the planner (ADR-019, ADR-023).
//!
//! The cardinality estimator provides per-table row counts and per-predicate
//! selectivity estimates that feed into the cost model
//! ([`crate::planner::CostModel`]) and the join reorderer ([`crate::planner::dpccp`]).
//!
//! ## Scope
//!
//! This is the **simple** estimator: it tracks only whole-table row counts
//! (`add_table`), not per-column distinct-value histograms. As a result:
//!
//! - `estimate_join` uses the FK-join heuristic `distinct ≈ |R|` for both
//!   sides, which gives `|R ⋈ S| = min(|R|, |S|)`.
//! - `estimate_selectivity` returns fixed defaults: `0.1` for equality
//!   predicates and `0.33` for range predicates.
//!
//! A future extension (deferred to ADR-016's submodular index selector) will
//! add per-column statistics via `ANALYZE` and replace the defaults with
//! real distinct-value counts.

use std::collections::HashMap;

/// A simple cardinality estimator that tracks per-table row counts.
///
/// Used by the cost model to estimate the size of intermediate join results,
/// which in turn drives the join reorderer's cost comparisons.
///
/// # Example
///
/// ```
/// use turbogp::planner::CardinalityEstimator;
///
/// let mut est = CardinalityEstimator::new();
/// est.add_table("orders", 1_000_000);
/// est.add_table("customers", 10_000);
///
/// // FK join: |orders ⋈ customers| ≈ min(1M, 10K) = 10K.
/// let join_card = est.estimate_join("orders", "customers", "cust_id", "id");
/// assert_eq!(join_card, 10_000);
/// ```
pub struct CardinalityEstimator {
    /// Table name → row count.
    table_stats: HashMap<String, usize>,
}

impl CardinalityEstimator {
    /// Create an empty estimator with no table statistics.
    #[must_use]
    pub fn new() -> Self {
        Self { table_stats: HashMap::new() }
    }

    /// Register the row count for a table.
    ///
    /// If `name` was already registered, the new count replaces the old one.
    pub fn add_table(&mut self, name: &str, row_count: usize) {
        self.table_stats.insert(name.to_string(), row_count);
    }

    /// Estimate the result size of an equi-join `R.k = S.k`.
    ///
    /// Uses the formula `|R| · |S| / max(distinct(R.k), distinct(S.k))`.
    ///
    /// Since this estimator does not track per-column distinct counts, we
    /// assume `distinct(R.k) = |R|` and `distinct(S.k) = |S|` (i.e., both
    /// keys are unique — the foreign-key join assumption). This gives:
    ///
    /// ```text
    /// |R ⋈ S| = |R| · |S| / max(|R|, |S|) = min(|R|, |S|)
    /// ```
    ///
    /// If either table is unknown (returns 0), the result is 0.
    ///
    /// The `left_key` and `right_key` parameters are accepted for API
    /// stability but are unused in this simple version — a future
    /// column-stats extension will index them.
    #[must_use]
    pub fn estimate_join(
        &self,
        left: &str,
        right: &str,
        _left_key: &str,
        _right_key: &str,
    ) -> usize {
        let left_card = self.table_stats.get(left).copied().unwrap_or(0);
        let right_card = self.table_stats.get(right).copied().unwrap_or(0);
        if left_card == 0 || right_card == 0 {
            return 0;
        }
        // distinct(R.k) ≈ |R|, distinct(S.k) ≈ |S| ⇒ max = max(|R|, |S|).
        let max_distinct = left_card.max(right_card);
        (left_card * right_card) / max_distinct
    }

    /// Estimate the selectivity of a predicate (the fraction of rows that
    /// pass).
    ///
    /// - `equality` / `eq` / `=` → `0.1` (default: 1/distinct, assuming
    ///   ~10 distinct values when no stats are available).
    /// - `range` / `>` / `<` / `>=` / `<=` / `between` → `0.33` (the
    ///   classical Selinger default for range predicates).
    /// - Anything else → `0.1` (default to equality).
    ///
    /// The `table` parameter is accepted for API stability but unused in
    /// this simple version.
    #[must_use]
    pub fn estimate_selectivity(&self, _table: &str, predicate_type: &str) -> f64 {
        match predicate_type {
            "equality" | "eq" | "=" => 0.1,
            "range" | ">" | "<" | ">=" | "<=" | "between" => 0.33,
            _ => 0.1,
        }
    }

    /// Look up the row count for a table (0 if unknown).
    #[must_use]
    pub fn table_row_count(&self, name: &str) -> usize {
        self.table_stats.get(name).copied().unwrap_or(0)
    }
}

impl Default for CardinalityEstimator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `estimate_join` with known table sizes returns a reasonable value
    /// (FK-join assumption: `min(|R|, |S|)`).
    ///
    /// DoD: CardinalityEstimator returns reasonable values.
    #[test]
    fn estimate_join_with_known_sizes_returns_min() {
        let mut est = CardinalityEstimator::new();
        est.add_table("orders", 1_000_000);
        est.add_table("customers", 10_000);

        // FK join: 1M * 10K / max(1M, 10K) = 1M * 10K / 1M = 10K = min.
        let card = est.estimate_join("orders", "customers", "cust_id", "id");
        assert_eq!(card, 10_000, "FK-join cardinality should be min(|R|, |S|) = 10K");
    }

    /// `estimate_join` is symmetric: `R ⋈ S` and `S ⋈ R` give the same size.
    #[test]
    fn estimate_join_is_symmetric() {
        let mut est = CardinalityEstimator::new();
        est.add_table("R", 1000);
        est.add_table("S", 100);
        let left_right = est.estimate_join("R", "S", "k", "k");
        let right_left = est.estimate_join("S", "R", "k", "k");
        assert_eq!(left_right, right_left, "join cardinality should be symmetric");
        assert_eq!(left_right, 100, "min(1000, 100) = 100");
    }

    /// `estimate_join` returns 0 if either table is unknown.
    #[test]
    fn estimate_join_unknown_table_returns_zero() {
        let mut est = CardinalityEstimator::new();
        est.add_table("R", 1000);
        // "S" is not registered.
        let card = est.estimate_join("R", "S", "k", "k");
        assert_eq!(card, 0, "unknown table should give 0 cardinality");
    }

    /// `estimate_selectivity` for equality returns 0.1 (default).
    ///
    /// DoD: CardinalityEstimator returns 0.1 for equality by default.
    #[test]
    fn estimate_selectivity_equality_returns_default() {
        let est = CardinalityEstimator::new();
        let sel = est.estimate_selectivity("R", "equality");
        assert!((sel - 0.1).abs() < 1e-9, "equality selectivity should be 0.1, got {sel}");
        // Alternate spellings.
        let sel = est.estimate_selectivity("R", "eq");
        assert!((sel - 0.1).abs() < 1e-9);
        let sel = est.estimate_selectivity("R", "=");
        assert!((sel - 0.1).abs() < 1e-9);
    }

    /// `estimate_selectivity` for range returns 0.33.
    #[test]
    fn estimate_selectivity_range_returns_033() {
        let est = CardinalityEstimator::new();
        for op in &["range", ">", "<", ">=", "<=", "between"] {
            let sel = est.estimate_selectivity("R", op);
            assert!(
                (sel - 0.33).abs() < 1e-9,
                "range selectivity for {op} should be 0.33, got {sel}"
            );
        }
    }

    /// `estimate_selectivity` for unknown types defaults to 0.1.
    #[test]
    fn estimate_selectivity_unknown_defaults_to_01() {
        let est = CardinalityEstimator::new();
        let sel = est.estimate_selectivity("R", "unknown_predicate");
        assert!((sel - 0.1).abs() < 1e-9, "unknown predicate should default to 0.1, got {sel}");
    }

    /// `add_table` overwrites the previous row count.
    #[test]
    fn add_table_overwrites() {
        let mut est = CardinalityEstimator::new();
        est.add_table("R", 100);
        assert_eq!(est.table_row_count("R"), 100);
        est.add_table("R", 200);
        assert_eq!(est.table_row_count("R"), 200);
    }

    /// `table_row_count` returns 0 for unknown tables.
    #[test]
    fn table_row_count_unknown_returns_zero() {
        let est = CardinalityEstimator::new();
        assert_eq!(est.table_row_count("nonexistent"), 0);
    }

    /// Default estimator has no stats.
    #[test]
    fn default_estimator_is_empty() {
        let est = CardinalityEstimator::default();
        assert_eq!(est.table_row_count("R"), 0);
    }
}
