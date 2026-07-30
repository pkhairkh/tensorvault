//! Graph-based pruning for MCTS join ordering (ADR-019, Wave 15).
//!
//! When the number of relations `n` exceeds 15, the [`crate::planner::dpccp`]
//! algorithm gives up (its `O(n²·2ⁿ)` complexity explodes). The MCTS-based
//! [`crate::planner::mcts::MctsJoinOrderer`] takes over, but a naive MCTS over
//! `n!` orderings would be hopelessly lost. The [`GraphPruner`] cuts the
//! branching factor down to size by exploiting the structure of the join
//! graph.
//!
//! ## Connectivity constraint
//!
//! A valid join plan never produces a cross product: every relation added to
//! the plan must share a join predicate with at least one relation already in
//! the plan. Equivalently, the partial join order forms a *connected subgraph*
//! of the join graph at every step.
//!
//! This is the same constraint enforced by DPccp (the "connected complement
//! pairs" in its name) and by Selinger's original connectivity pruning. The
//! [`GraphPruner::valid_children`] method exposes this constraint to MCTS:
//! at each step, only relations adjacent to the current covered set are
//! candidates for expansion.
//!
//! ## Lower bound
//!
//! [`GraphPruner::lower_bound`] provides a cheap admissible lower bound on
//! the cost of completing a partial join plan, suitable for alpha-beta-style
//! pruning. The bound is:
//!
//! ```text
//! lb(covered) = max_card(covered) · Σ_{r ∉ covered} card(r)
//! ```
//!
//! where `max_card(covered)` is the largest cardinality among the relations
//! in `covered` (the FK-join cardinality of the current partial result). The
//! bound is admissible because:
//!
//! 1. Every remaining relation `r` must eventually be joined.
//! 2. The cost of joining `r` is `current_card · card(r)` (or more, since
//!    `current_card` only grows as we add relations).
//! 3. So the cheapest possible addition of `r` costs at least
//!    `max_card(covered) · card(r)`.
//!
//! The bound is tight when the join graph is a star centered on the largest
//! relation, and loose when there are many small relations chained off a
//! large one. It is always non-negative.

use crate::planner::dpccp::JoinRelation;

/// Pruning rules for MCTS based on the join graph.
///
/// Pre-computes the undirected adjacency list of the join graph so that
/// [`Self::valid_children`] and [`Self::lower_bound`] are cheap to call from
/// inside the MCTS loop (which may run for 10 000+ iterations).
pub struct GraphPruner {
    /// For each relation `i`, the list of relations it shares a join
    /// predicate with. Symmetric: if `j ∈ adjacency[i]` then
    /// `i ∈ adjacency[j]`.
    adjacency: Vec<Vec<usize>>,
}

impl GraphPruner {
    /// Build a pruner from the join graph encoded in `relations`.
    ///
    /// Each relation's `joins_with` list is treated as undirected: if `j`
    /// appears in `relations[i].joins_with`, then `i` and `j` are connected
    /// in both directions, regardless of whether `i` appears in
    /// `relations[j].joins_with` (mirroring [`crate::planner::dpccp`]'s
    /// undirected treatment).
    ///
    /// Self-loops and out-of-range indices are silently dropped (defensive
    /// against malformed inputs).
    #[must_use]
    pub fn new(relations: &[JoinRelation]) -> Self {
        let n = relations.len();
        let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, r) in relations.iter().enumerate() {
            for &j in &r.joins_with {
                if j == i || j >= n {
                    continue; // self-loop or out of range: ignore
                }
                if !adjacency[i].contains(&j) {
                    adjacency[i].push(j);
                }
                if !adjacency[j].contains(&i) {
                    adjacency[j].push(i);
                }
            }
        }
        // Sort each adjacency list for deterministic iteration order — MCTS
        // relies on a stable enumeration of children so that UCT can compare
        // visit counts meaningfully across iterations.
        for nbrs in &mut adjacency {
            nbrs.sort_unstable();
        }
        Self { adjacency }
    }

    /// Returns the valid next relations to add given the current `covered`
    /// set (as a bitmask).
    ///
    /// A relation `j` is valid if:
    ///
    /// 1. It is **not** already in `covered`.
    /// 2. It connects to at least one relation already in `covered`
    ///    (connectivity), **or** `covered` is empty (the first relation can
    ///    be anything — every relation is a valid starting point).
    ///
    /// Returns the valid relation indices in ascending order.
    ///
    /// # Examples
    ///
    /// For a 3-relation chain `A - B - C`:
    ///
    /// - `valid_children(0b000)` → `[0, 1, 2]` (any starting point).
    /// - `valid_children(0b001)` → `[1]` (only B connects to A).
    /// - `valid_children(0b101)` → `[1]` (B connects to both A and C).
    /// - `valid_children(0b111)` → `[]` (all covered — terminal).
    #[must_use]
    pub fn valid_children(&self, covered: u64) -> Vec<usize> {
        let n = self.adjacency.len();
        if covered == 0 {
            // Empty set: any relation is a valid first pick.
            return (0..n).collect();
        }
        let mut result = Vec::new();
        for j in 0..n {
            if covered & (1u64 << j) != 0 {
                continue; // already in covered
            }
            // Check if j is connected to any relation in covered.
            if self.adjacency[j].iter().any(|&i| covered & (1u64 << i) != 0) {
                result.push(j);
            }
        }
        result
    }

    /// Lower bound on the cost of completing the join from the `covered`
    /// state, using the FK-join cardinality assumption.
    ///
    /// Returns 0.0 for the empty set (no work done yet, no lower bound
    /// available without more sophisticated analysis). Otherwise, the bound
    /// is `max_card(covered) · Σ_{r ∉ covered} card(r)`.
    ///
    /// This bound is **admissible** (never overestimates the true optimal
    /// cost) under the cost model `cost(S ⋈ j) = cost(S) + cost(j) + |S| · |j|`
    /// with `|S ⋈ j| = max(|S|, |j|)`:
    ///
    /// - `|S|` (the cardinality of the current partial result) only grows as
    ///   we add more relations, so each future join costs at least
    ///   `max_card(covered) · card(r)`.
    /// - Each remaining relation `r` must be joined exactly once, so we sum
    ///   over all remaining relations.
    ///
    /// Use this for alpha-beta-style pruning: if
    /// `cost_so_far + lower_bound(covered) >= incumbent_cost`, prune.
    #[must_use]
    pub fn lower_bound(&self, covered: u64, relations: &[JoinRelation]) -> f64 {
        if covered == 0 {
            return 0.0;
        }
        let max_card = max_covered_cardinality(covered, relations);
        if max_card == 0 {
            return 0.0;
        }
        let mut lb = 0.0;
        for (j, r) in relations.iter().enumerate() {
            if covered & (1u64 << j) == 0 {
                lb += max_card as f64 * r.cardinality as f64;
            }
        }
        lb
    }

    /// Borrow the adjacency list (for testing and debugging).
    #[cfg(test)]
    pub(crate) fn adjacency(&self) -> &[Vec<usize>] {
        &self.adjacency
    }
}

/// The maximum cardinality among relations in `covered` (the FK-join
/// cardinality of the partial result).
///
/// Returns 0 if `covered` is empty or out of range.
fn max_covered_cardinality(covered: u64, relations: &[JoinRelation]) -> usize {
    let mut max_card = 0usize;
    for (i, r) in relations.iter().enumerate() {
        if covered & (1u64 << i) != 0 && r.cardinality > max_card {
            max_card = r.cardinality;
        }
    }
    max_card
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3-relation chain `A - B - C`: adjacency is symmetric and self-loop-free.
    #[test]
    fn graph_pruner_builds_symmetric_adjacency_for_chain() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0, 2] },
            JoinRelation { name: "C".into(), cardinality: 150, joins_with: vec![1] },
        ];
        let p = GraphPruner::new(&relations);
        let adj = p.adjacency();
        assert_eq!(adj[0], vec![1], "A is adjacent to B");
        assert_eq!(adj[1], vec![0, 2], "B is adjacent to A and C");
        assert_eq!(adj[2], vec![1], "C is adjacent to B");
    }

    /// Asymmetric `joins_with` lists are normalized: if A lists B, then B's
    /// adjacency includes A even if B does not list A.
    #[test]
    fn graph_pruner_normalizes_asymmetric_joins_with() {
        let relations = vec![
            // A lists B; B does NOT list A.
            JoinRelation { name: "A".into(), cardinality: 10, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 20, joins_with: vec![] },
        ];
        let p = GraphPruner::new(&relations);
        let adj = p.adjacency();
        assert_eq!(adj[0], vec![1], "A is adjacent to B");
        assert_eq!(adj[1], vec![0], "B's adjacency includes A (undirected)");
    }

    /// `valid_children` for the empty set returns all relations.
    /// DoD: GraphPruner::valid_children for empty set → all relations.
    #[test]
    fn valid_children_for_empty_set_returns_all_relations() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0, 2] },
            JoinRelation { name: "C".into(), cardinality: 150, joins_with: vec![1] },
        ];
        let p = GraphPruner::new(&relations);
        let valid = p.valid_children(0);
        assert_eq!(valid, vec![0, 1, 2], "empty set should admit all relations");
    }

    /// After adding relation 0, `valid_children` returns only relations
    /// connected to 0.
    /// DoD: GraphPruner::valid_children after adding relation 0 → only
    /// relations connected to 0.
    #[test]
    fn valid_children_after_adding_relation_zero_returns_only_neighbors() {
        let relations = vec![
            // A (0) connects only to B (1).
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0, 2] },
            JoinRelation { name: "C".into(), cardinality: 150, joins_with: vec![1] },
        ];
        let p = GraphPruner::new(&relations);
        let valid = p.valid_children(0b001);
        // A is covered; the only relation adjacent to A is B.
        assert_eq!(valid, vec![1], "after adding A, only B is a valid next pick");
    }

    /// For a chain `A-B-C`, after covering A and C (0b101), only B is valid
    /// (B is adjacent to both A and C, and is the only remaining relation).
    #[test]
    fn valid_children_for_chain_middle_after_covering_ends() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0, 2] },
            JoinRelation { name: "C".into(), cardinality: 150, joins_with: vec![1] },
        ];
        let p = GraphPruner::new(&relations);
        // Cover A (bit 0) and C (bit 2): 0b101.
        let valid = p.valid_children(0b101);
        assert_eq!(valid, vec![1], "only B remains and it is adjacent to both A and C");
    }

    /// `valid_children` for the full set returns an empty vector (terminal).
    #[test]
    fn valid_children_for_full_set_is_empty() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0] },
        ];
        let p = GraphPruner::new(&relations);
        let valid = p.valid_children(0b11);
        assert!(valid.is_empty(), "full set should have no valid children");
    }

    /// `valid_children` for a disconnected state (covered set + an isolated
    /// relation not adjacent to it) excludes the isolated relation.
    #[test]
    fn valid_children_excludes_disconnected_relations() {
        // A-B chain, plus isolated C.
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 10, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 20, joins_with: vec![0] },
            JoinRelation { name: "C".into(), cardinality: 30, joins_with: vec![] },
        ];
        let p = GraphPruner::new(&relations);
        // Cover A (bit 0): 0b001. Only B is adjacent to A; C is isolated.
        let valid = p.valid_children(0b001);
        assert_eq!(valid, vec![1], "C is disconnected from A and should be excluded");
    }

    /// `lower_bound` is non-negative for any covered set.
    /// DoD: GraphPruner::lower_bound is non-negative.
    #[test]
    fn lower_bound_is_non_negative() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0, 2] },
            JoinRelation { name: "C".into(), cardinality: 150, joins_with: vec![1] },
        ];
        let p = GraphPruner::new(&relations);
        // Empty set: returns 0.0.
        assert!(p.lower_bound(0, &relations) >= 0.0, "empty set lower_bound >= 0");
        // Single relation covered.
        assert!(p.lower_bound(0b001, &relations) >= 0.0, "partial set lower_bound >= 0");
        // Two relations covered.
        assert!(p.lower_bound(0b011, &relations) >= 0.0, "partial set lower_bound >= 0");
        // Full set: no remaining relations, lb = 0.
        assert!(p.lower_bound(0b111, &relations) >= 0.0, "full set lower_bound >= 0");
    }

    /// `lower_bound` for a partial set equals `max_card(covered) · Σ remaining cards`.
    ///
    /// For A(100), B(200), C(150): covering A gives max_card = 100, remaining
    /// cards = 200 + 150 = 350, so lb = 100 * 350 = 35 000.
    #[test]
    fn lower_bound_matches_formula_for_partial_set() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0, 2] },
            JoinRelation { name: "C".into(), cardinality: 150, joins_with: vec![1] },
        ];
        let p = GraphPruner::new(&relations);
        let lb = p.lower_bound(0b001, &relations);
        // max_card(A) = 100; remaining = B(200) + C(150) = 350.
        let expected = 100.0 * (200.0 + 150.0);
        assert!((lb - expected).abs() < 1e-6, "lb = {lb}, expected {expected}");
    }

    /// `lower_bound` for the empty set returns 0 (no information).
    #[test]
    fn lower_bound_for_empty_set_is_zero() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0] },
        ];
        let p = GraphPruner::new(&relations);
        assert_eq!(p.lower_bound(0, &relations), 0.0);
    }

    /// `lower_bound` for the full set returns 0 (nothing left to do).
    #[test]
    fn lower_bound_for_full_set_is_zero() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0] },
        ];
        let p = GraphPruner::new(&relations);
        assert_eq!(p.lower_bound(0b11, &relations), 0.0);
    }

    /// `lower_bound` is tight for a star query: center C(1000) + satellites
    /// S1(10), S2(10). Starting from C, every satellite joins at cost
    /// 1000 * 10 = 10 000, and lb = 1000 * (10 + 10) = 20 000 = actual cost.
    #[test]
    fn lower_bound_is_tight_for_star_query_from_center() {
        let relations = vec![
            JoinRelation { name: "C".into(), cardinality: 1000, joins_with: vec![1, 2] },
            JoinRelation { name: "S1".into(), cardinality: 10, joins_with: vec![0] },
            JoinRelation { name: "S2".into(), cardinality: 10, joins_with: vec![0] },
        ];
        let p = GraphPruner::new(&relations);
        // Cover C (bit 0): 0b001. max_card = 1000; remaining = 10 + 10 = 20.
        let lb = p.lower_bound(0b001, &relations);
        let actual = 1000.0 * 10.0 + // C ⋈ S1
            1000.0 * 10.0; // (C ⋈ S1) ⋈ S2 — card stays at max(1000, 10) = 1000
        assert!((lb - actual).abs() < 1e-6, "lb = {lb}, expected {actual} (tight for star)");
    }
}
