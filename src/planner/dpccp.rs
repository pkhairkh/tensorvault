//! **NOT WIRED INTO SQL EXECUTION** — this module exists but is not called by QueryEngine::execute() (or is only partially wired; see Wave 53 notes in engine/mod.rs).
//! DPccp join ordering (ADR-019).
//!
//! DPccp (Dynamic Programming over connected complement pairs, Moerkotte &
//! Neumann 2008) is the modern standard for optimal join ordering. It avoids
//! generating cross products during DP, giving roughly a 2× constant-factor
//! speedup over Selinger's original 1979 algorithm.
//!
//! ## Scope
//!
//! This implementation uses the **left-deep** simplification: at each DP step,
//! we expand a connected subset `S` by joining it with a *single* relation
//! `j` that is connected to `S`. This is a subset of full DPccp (which also
//! considers bushy plans formed by joining two non-singleton subsets), but it
//! is sufficient for `n ≤ 15` joins — the ADR-019 limit beyond which IDP
//! takes over.
//!
//! ## Cost model
//!
//! For each candidate join `S ⋈ {j}` we compute:
//!
//! ```text
//! cost(S ⋈ j) = cost(S) + cost(j) + |S| · |j|
//! ```
//!
//! where `|·|` denotes the *output cardinality* of the subtree. Leaves have
//! cost 0 (table scans are costed separately by [`crate::planner::CostModel`]).
//! Cardinality of an inner node uses the foreign-key assumption
//! `|R ⋈ S| = max(|R|, |S|)` (one side is a unique key, so each row on the
//! smaller side matches at most one row on the larger side).
//!
//! ## Tie-breaking
//!
//! When two candidate plans for the same subset have equal cost (which happens
//! often — the symmetric `|S|·|j|` term is invariant under reordering), we
//! prefer the plan whose **leftmost leaf has smaller cardinality**. This
//! mirrors the classical heuristic of driving the join from the smallest
//! relation, keeping intermediate results small.
//!
//! ## References
//!
//! - Moerkotte & Neumann, "Dynamic Programming Strikes Back", SIGMOD 2008.
//! - Selinger, "Access Path Selection in a Relational Database Management
//!   System", 1979.
//! - ADR-019 (`docs/adr/019-dpccp-join-ordering.md`).

use crate::error::{Error, Result};
use std::collections::HashMap;

/// A join relation (a single table) with its cardinality estimate and the
/// list of other relations it joins with.
///
/// `joins_with` holds the **indices** (into the `relations` slice passed to
/// [`dpccp`]) of relations that share a join predicate with this one. The
/// graph is treated as undirected: a connection `{i, j}` is considered
/// present if `i ∈ relations[j].joins_with` **or** `j ∈ relations[i].joins_with`.
#[derive(Debug, Clone)]
pub struct JoinRelation {
    /// The table name (for debugging and plan display).
    pub name: String,
    /// Estimated row count of the table.
    pub cardinality: usize,
    /// Indices of relations this one joins with (undirected).
    pub joins_with: Vec<usize>,
}

/// A join tree: either a leaf relation or an inner join of two subtrees.
///
/// The tree may be left-deep (every `right` is a `Leaf`) or bushy. The
/// [`dpccp`] function produces left-deep trees; bushy trees are supported by
/// the data structure for future extension.
#[derive(Debug, Clone)]
pub enum JoinTree {
    /// A leaf relation (a single table reference).
    Leaf(JoinRelation),
    /// An inner join of `left` and `right` on the join columns.
    Inner {
        /// The left subtree.
        left: Box<JoinTree>,
        /// The right subtree.
        right: Box<JoinTree>,
        /// Cumulative cost of this subtree (= `cost(left) + cost(right) +
        /// |left| · |right|`).
        cost: f64,
        /// Output cardinality of this join (FK assumption:
        /// `max(|left|, |right|)`).
        cardinality: usize,
    },
}

impl JoinTree {
    /// The cumulative cost of executing this subtree.
    ///
    /// Leaves have cost 0 (table scans are costed separately). Inner nodes
    /// return the stored cumulative cost, which already includes the costs of
    /// both children plus the join work `|left| · |right|`.
    #[must_use]
    pub fn cost(&self) -> f64 {
        match self {
            Self::Leaf(_) => 0.0,
            Self::Inner { cost, .. } => *cost,
        }
    }

    /// The output cardinality (row count) of this subtree.
    ///
    /// Leaves return the table's row count. Inner nodes return the FK-join
    /// estimate `max(|left|, |right|)`.
    #[must_use]
    pub fn cardinality(&self) -> usize {
        match self {
            Self::Leaf(r) => r.cardinality,
            Self::Inner { cardinality, .. } => *cardinality,
        }
    }
}

/// Find the optimal left-deep join order using DPccp.
///
/// # Arguments
///
/// * `relations` — The relations to join, with their cardinalities and join
///   graph. The join graph is undirected (see [`JoinRelation::joins_with`]).
///
/// # Errors
///
/// - Returns [`Error::InvalidArg`] if `relations.len() > 15`. For `n > 15`,
///   use IDP (Iterative Dynamic Programming), which is not yet implemented.
/// - Returns [`Error::InvalidArg`] if `relations` is empty.
/// - Returns [`Error::InvalidArg`] if the join graph is disconnected — no
///   valid join plan exists.
///
/// # Algorithm
///
/// 1. Initialize the DP table with each relation as a singleton subset.
/// 2. Iterate subsets in order of popcount (size 1, then 2, ...).
/// 3. For each subset `S`, find all singletons `j` not in `S` that are
///    connected to `S`.
/// 4. For each such `j`, form `S' = S ∪ {j}` with cost
///    `cost(S) + cost(j) + |S| · |j|`. If `S'` already has a plan, keep the
///    cheaper one (ties broken by smaller leftmost-leaf cardinality).
/// 5. Return the plan for the full set.
///
/// # Complexity
///
/// O(n² · 2ⁿ) for the left-deep simplification (full DPccp is O(3ⁿ)).
/// At n = 15, that's ~7.4M operations — well under 100 ms.
pub fn dpccp(relations: &[JoinRelation]) -> Result<JoinTree> {
    let n = relations.len();
    if n == 0 {
        return Err(Error::InvalidArg("DPccp requires at least one relation".into()));
    }
    if n > 15 {
        return Err(Error::InvalidArg(format!(
            "DPccp supports at most 15 relations (got {n}); use IDP for larger queries (ADR-019)"
        )));
    }

    // Special case: single relation — return the leaf directly.
    if n == 1 {
        return Ok(JoinTree::Leaf(relations[0].clone()));
    }

    // DP table: bitmask -> (best plan for that subset, cumulative cost).
    // n ≤ 15 ⇒ bitmask fits in u16 (16 bits).
    let mut dp: HashMap<u16, (JoinTree, f64)> = HashMap::new();

    // Initialize singletons.
    for (i, r) in relations.iter().enumerate() {
        let mask = 1u16 << i;
        dp.insert(mask, (JoinTree::Leaf(r.clone()), 0.0));
    }

    let total_mask: u16 = (1u16 << n) - 1;

    // Iterate subsets in order of popcount so that when we process a subset
    // of size `k`, all subsets of size `< k` have already been finalized.
    for size in 1..n {
        // Snapshot the subsets of this size before mutating `dp`. New plans
        // created in this iteration have size `k+1` and won't be processed
        // until the next iteration.
        let current_subsets: Vec<u16> =
            dp.keys().copied().filter(|m| m.count_ones() as usize == size).collect();

        for s_mask in current_subsets {
            // Clone the tree + cost to release the immutable borrow of `dp`
            // before we mutate it below.
            let (s_tree, s_cost) = dp[&s_mask].clone();
            let s_card = s_tree.cardinality();
            let s_leftmost = leftmost_card(&s_tree);

            // Try expanding `S` with each connected singleton `j` ∉ `S`.
            for j in 0..n {
                if s_mask & (1u16 << j) != 0 {
                    continue; // already in S
                }
                if !is_connected(s_mask, j, relations) {
                    continue; // not adjacent to S
                }

                let j_card = relations[j].cardinality;
                let join_cost = s_cost + (s_card as f64) * (j_card as f64);
                let new_card = s_card.max(j_card);
                let new_tree = JoinTree::Inner {
                    left: Box::new(s_tree.clone()),
                    right: Box::new(JoinTree::Leaf(relations[j].clone())),
                    cost: join_cost,
                    cardinality: new_card,
                };
                let new_mask = s_mask | (1u16 << j);
                // The leftmost leaf is unchanged when we append a singleton
                // on the right (left-deep expansion).
                let new_leftmost = s_leftmost;

                match dp.get(&new_mask) {
                    None => {
                        dp.insert(new_mask, (new_tree, join_cost));
                    }
                    Some((existing_tree, existing_cost)) => {
                        let existing_leftmost = leftmost_card(existing_tree);
                        // Replace if strictly cheaper, or equal cost with a
                        // strictly smaller leftmost-leaf cardinality.
                        let should_replace = join_cost < *existing_cost - 1e-9
                            || ((join_cost - existing_cost).abs() < 1e-9
                                && new_leftmost < existing_leftmost);
                        if should_replace {
                            dp.insert(new_mask, (new_tree, join_cost));
                        }
                    }
                }
            }
        }
    }

    // The full-set plan is the answer.
    dp.remove(&total_mask).map(|(tree, _)| tree).ok_or_else(|| {
        Error::InvalidArg(
            "DPccp found no valid join plan for the full set (join graph may be disconnected)"
                .into(),
        )
    })
}

/// Returns `true` if relation `j` is connected to (shares a join predicate
/// with) any relation in the subset `s_mask`.
///
/// The join graph is treated as undirected: a connection is present if
/// `j ∈ relations[i].joins_with` **or** `i ∈ relations[j].joins_with` for
/// some `i ∈ s_mask`.
fn is_connected(s_mask: u16, j: usize, relations: &[JoinRelation]) -> bool {
    for i in 0..relations.len() {
        if s_mask & (1u16 << i) != 0
            && (relations[i].joins_with.contains(&j) || relations[j].joins_with.contains(&i))
        {
            return true;
        }
    }
    false
}

/// The cardinality of the leftmost leaf in the tree.
///
/// Used for tie-breaking in DP: when two plans have equal cost, prefer the
/// one whose leftmost leaf is smaller (it drives the join from a smaller
/// outer relation, keeping intermediate results small).
fn leftmost_card(tree: &JoinTree) -> usize {
    match tree {
        JoinTree::Leaf(r) => r.cardinality,
        JoinTree::Inner { left, .. } => leftmost_card(left),
    }
}

/// Count the number of relations in a join tree (for testing).
#[cfg(test)]
fn count_relations(tree: &JoinTree) -> usize {
    match tree {
        JoinTree::Leaf(_) => 1,
        JoinTree::Inner { left, right, .. } => count_relations(left) + count_relations(right),
    }
}

/// The name of the leftmost leaf in the tree (for testing).
#[cfg(test)]
fn leftmost_name(tree: &JoinTree) -> &str {
    match tree {
        JoinTree::Leaf(r) => &r.name,
        JoinTree::Inner { left, .. } => leftmost_name(left),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2-table join: produces a single Inner node with two leaves.
    /// DoD: DPccp produces valid join trees for n ≤ 5.
    #[test]
    fn dpccp_two_table_join_produces_simple_plan() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0] },
        ];
        let plan = dpccp(&relations).expect("2-table join should succeed");
        assert_eq!(count_relations(&plan), 2, "plan should contain 2 relations");
        // Should be an Inner with two Leaves.
        match &plan {
            JoinTree::Inner { left, right, cost, cardinality } => {
                assert!(matches!(left.as_ref(), JoinTree::Leaf(_)));
                assert!(matches!(right.as_ref(), JoinTree::Leaf(_)));
                // cost = 0 + 0 + 100*200 = 20000.
                assert!((cost - 20_000.0).abs() < 1e-6, "cost = {cost}, expected 20000");
                // card = max(100, 200) = 200.
                assert_eq!(*cardinality, 200);
            }
            other => panic!("expected Inner, got {other:?}"),
        }
    }

    /// Single table → leaf node (no joins).
    /// DoD: DPccp produces valid join trees for n ≤ 5.
    #[test]
    fn dpccp_single_table_returns_leaf() {
        let relations =
            vec![JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![] }];
        let plan = dpccp(&relations).expect("single-table query should succeed");
        match &plan {
            JoinTree::Leaf(r) => {
                assert_eq!(r.name, "A");
                assert_eq!(r.cardinality, 100);
            }
            other => panic!("expected Leaf, got {other:?}"),
        }
        assert_eq!(plan.cost(), 0.0);
        assert_eq!(plan.cardinality(), 100);
    }

    /// 3-table star query: center C (1000 rows) + satellites S1, S2 (10 rows
    /// each), each satellite joins only with the center.
    ///
    /// With the FK-join cardinality assumption and the smaller-leftmost-leaf
    /// tie-break, the optimal plan starts with a satellite (S1 ⋈ C or
    /// S2 ⋈ C), keeping intermediate cardinalities small.
    ///
    /// DoD: DPccp produces valid join trees for n ≤ 5.
    #[test]
    fn dpccp_three_table_star_joins_satellite_first() {
        let relations = vec![
            JoinRelation { name: "C".into(), cardinality: 1000, joins_with: vec![1, 2] },
            JoinRelation { name: "S1".into(), cardinality: 10, joins_with: vec![0] },
            JoinRelation { name: "S2".into(), cardinality: 10, joins_with: vec![0] },
        ];
        let plan = dpccp(&relations).expect("3-table star should succeed");
        assert_eq!(count_relations(&plan), 3, "plan should contain 3 relations");

        // The leftmost leaf should be a satellite (S1 or S2), NOT the center.
        // Rationale: with the symmetric cost formula, all valid orderings tie,
        // and the smaller-leftmost tie-break prefers starting from a satellite
        // (cardinality 10) rather than the center (cardinality 1000).
        let leftmost = leftmost_name(&plan);
        assert!(
            leftmost == "S1" || leftmost == "S2",
            "leftmost leaf should be a satellite, got {leftmost}"
        );

        // The plan should be a valid left-deep tree: ((X ⋈ C) ⋈ Y).
        match &plan {
            JoinTree::Inner { left, right, .. } => {
                // Outer join: left should be (sat ⋈ C), right should be a Leaf.
                assert!(
                    matches!(left.as_ref(), JoinTree::Inner { .. }),
                    "left of outer join should be an Inner, got {:?}",
                    left
                );
                assert!(
                    matches!(right.as_ref(), JoinTree::Leaf(_)),
                    "right of outer join should be a Leaf, got {:?}",
                    right
                );
                // Inner join: left should be a Leaf (satellite), right should
                // be a Leaf (center).
                if let JoinTree::Inner { left: inner_left, right: inner_right, .. } = left.as_ref()
                {
                    assert!(matches!(inner_left.as_ref(), JoinTree::Leaf(_)));
                    assert!(matches!(inner_right.as_ref(), JoinTree::Leaf(_)));
                }
            }
            other => panic!("expected Inner (join), got {other:?}"),
        }
    }

    /// 5-table chain query (A-B-C-D-E): returns a valid plan containing all
    /// 5 relations. The plan need not be optimal — just valid.
    ///
    /// DoD: DPccp produces valid join trees for n ≤ 5.
    #[test]
    fn dpccp_five_table_chain_returns_valid_plan() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0, 2] },
            JoinRelation { name: "C".into(), cardinality: 150, joins_with: vec![1, 3] },
            JoinRelation { name: "D".into(), cardinality: 50, joins_with: vec![2, 4] },
            JoinRelation { name: "E".into(), cardinality: 300, joins_with: vec![3] },
        ];
        let plan = dpccp(&relations).expect("5-table chain should succeed");
        assert_eq!(count_relations(&plan), 5, "plan should contain all 5 relations");
        // Cost and cardinality should be positive.
        assert!(plan.cost() > 0.0, "cost should be positive for a 5-table join");
        assert!(plan.cardinality() > 0, "cardinality should be positive");
    }

    /// n > 15 returns an error (IDP not yet implemented).
    #[test]
    fn dpccp_rejects_more_than_fifteen_relations() {
        let relations: Vec<JoinRelation> = (0..16)
            .map(|i| JoinRelation {
                name: format!("R{i}"),
                cardinality: 100,
                joins_with: if i > 0 { vec![i - 1] } else { vec![1] },
            })
            .collect();
        let result = dpccp(&relations);
        assert!(result.is_err(), "DPccp should reject n > 15");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("15") || err.contains("IDP"),
            "error should mention the 15-relation limit or IDP, got: {err}"
        );
    }

    /// Empty input returns an error.
    #[test]
    fn dpccp_rejects_empty_input() {
        let result = dpccp(&[]);
        assert!(result.is_err(), "DPccp should reject empty input");
    }

    /// Disconnected join graph returns an error.
    #[test]
    fn dpccp_rejects_disconnected_graph() {
        // A and B are not connected.
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![] },
        ];
        let result = dpccp(&relations);
        assert!(result.is_err(), "DPccp should reject disconnected graph");
    }

    /// `cost()` and `cardinality()` work on leaves and inner nodes.
    #[test]
    fn join_tree_cost_and_cardinality_accessors() {
        let leaf =
            JoinTree::Leaf(JoinRelation { name: "R".into(), cardinality: 42, joins_with: vec![] });
        assert_eq!(leaf.cost(), 0.0);
        assert_eq!(leaf.cardinality(), 42);

        let inner = JoinTree::Inner {
            left: Box::new(leaf),
            right: Box::new(JoinTree::Leaf(JoinRelation {
                name: "S".into(),
                cardinality: 7,
                joins_with: vec![],
            })),
            cost: 1234.0,
            cardinality: 42,
        };
        assert!((inner.cost() - 1234.0).abs() < 1e-9);
        assert_eq!(inner.cardinality(), 42);
    }

    /// `is_connected` correctly identifies adjacency in the join graph.
    #[test]
    fn is_connected_handles_undirected_graph() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 10, joins_with: vec![1] },
            // B does NOT list A in joins_with (asymmetric), but A lists B.
            JoinRelation { name: "B".into(), cardinality: 10, joins_with: vec![] },
            JoinRelation { name: "C".into(), cardinality: 10, joins_with: vec![] },
        ];
        // A is connected to B (A lists B).
        assert!(is_connected(0b001, 1, &relations));
        // B is connected to A (A lists B — undirected).
        assert!(is_connected(0b010, 0, &relations));
        // Neither A nor B is connected to C.
        assert!(!is_connected(0b001, 2, &relations));
        assert!(!is_connected(0b010, 2, &relations));
    }

    /// The cost formula `cost(S ⋈ j) = cost(S) + cost(j) + |S| · |j|` is
    /// applied correctly. For two leaves A(100) and B(200): cost = 100*200.
    #[test]
    fn dpccp_cost_formula_is_correct() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0] },
        ];
        let plan = dpccp(&relations).expect("should succeed");
        // 100 * 200 = 20000.
        assert!((plan.cost() - 20_000.0).abs() < 1e-6, "cost = {}, expected 20000", plan.cost());
    }
}
