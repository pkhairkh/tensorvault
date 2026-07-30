//! Conversion of a tensor contraction ordering into a [`JoinTree`] (Wave 17).
//!
//! A tensor contraction ordering is a sequence of pairs `(i, j)` —
//! "contract the subtree containing original tensor `i` with the subtree
//! containing original tensor `j`". This module turns that abstraction
//! into the concrete [`JoinTree`] used by the rest of the planner
//! ([`crate::planner::dpccp`], [`crate::planner::mcts`]).
//!
//! ## Algorithm
//!
//! We use a **union-find over slots**: each original tensor starts in
//! its own slot holding a [`JoinTree::Leaf`]. For each `(i, j)` in the
//! ordering:
//!
//! 1. Look up the slots holding `i` and `j` (via an `owner` array).
//! 2. Wrap their contents in a [`JoinTree::Inner`] with the standard
//!    DPccp cost formula `cost(left) + cost(right) + |left| · |right|`
//!    and FK-join cardinality `max(|left|, |right|)`.
//! 3. Place the merged tree back in the lower-numbered slot; the
//!    higher-numbered slot is retired, and all entries in `owner`
//!    pointing to it are repointed to the surviving slot.
//!
//! After all steps, exactly one slot should remain — that slot holds
//! the final [`JoinTree`]. If more than one slot remains (the ordering
//! was incomplete), the function returns
//! [`Error::InvalidArg`](crate::error::Error::InvalidArg).
//!
//! ## Cost compatibility
//!
//! The cost formula matches [`crate::planner::dpccp`] exactly, so the
//! resulting tree's [`JoinTree::cost`] is directly comparable to a
//! DPccp-produced tree's cost. This lets the planner pick the cheaper
//! of DPccp and the tensor-network plan without scaling.

use crate::error::{Error, Result};
use crate::planner::dpccp::{JoinRelation, JoinTree};
use crate::planner::tensor::TensorNetwork;

/// Convert a tensor contraction ordering into a join tree.
///
/// Each contraction step `(i, j)` becomes a join between the sub-trees
/// containing tensors `i` and `j` (see the module docs for the
/// algorithm). The resulting [`JoinTree`] is structurally equivalent to
/// one produced by [`crate::planner::dpccp::dpccp`] and uses the same
/// cost formula.
///
/// # Arguments
///
/// * `network` — The tensor network (used only to validate that the
///   number of tensors matches `relations.len()`). The network's
///   `attributes` are not consulted; the join tree is built purely
///   from `relations`.
/// * `order` — The contraction ordering, as produced by
///   [`TensorNetwork::optimal_contraction_order`]. Each pair refers to
///   **original** tensor indices (not current slot indices).
/// * `relations` — The relations to use as leaves in the join tree.
///   `relations[i]` is the leaf for tensor `i`.
///
/// # Errors
///
/// - [`Error::InvalidArg`] if `relations` is empty.
/// - [`Error::InvalidArg`] if `network.tensors.len() != relations.len()`.
/// - [`Error::InvalidArg`] if any `(i, j)` in `order` has `i` or `j`
///   out of range, or if `i` and `j` are already in the same subtree.
/// - [`Error::InvalidArg`] if, after applying all steps, more than one
///   subtree remains (the ordering was incomplete — expected
///   `relations.len() - 1` steps for a fully connected contraction).
///
/// # Examples
///
/// ```
/// use turbogp::planner::agm::JoinHypergraph;
/// use turbogp::planner::contraction::contraction_to_join_tree;
/// use turbogp::planner::dpccp::JoinRelation;
/// use turbogp::planner::tensor::TensorNetwork;
///
/// let relations = vec![
///     JoinRelation { name: "R".into(), cardinality: 100, joins_with: vec![1] },
///     JoinRelation { name: "S".into(), cardinality: 200, joins_with: vec![0] },
/// ];
/// let graph = JoinHypergraph::from_named(&["A", "B"], &[vec!["A", "B"], vec!["A", "B"]]);
/// let net = TensorNetwork::from_hypergraph(&graph, &[100, 200]);
/// let order = vec![(0, 1)];
/// let tree = contraction_to_join_tree(&net, &order, &relations).expect("join succeeds");
/// assert!(tree.cost() > 0.0);
/// ```
pub fn contraction_to_join_tree(
    network: &TensorNetwork,
    order: &[(usize, usize)],
    relations: &[JoinRelation],
) -> Result<JoinTree> {
    if relations.is_empty() {
        return Err(Error::InvalidArg("contraction_to_join_tree: no relations provided".into()));
    }
    if network.tensors.len() != relations.len() {
        return Err(Error::InvalidArg(format!(
            "contraction_to_join_tree: network has {} tensors but {} relations provided",
            network.tensors.len(),
            relations.len(),
        )));
    }

    // Single relation, empty order → return the leaf directly.
    if relations.len() == 1 {
        if !order.is_empty() {
            return Err(Error::InvalidArg(format!(
                "contraction_to_join_tree: 1 relation but {} contraction steps provided",
                order.len(),
            )));
        }
        return Ok(JoinTree::Leaf(relations[0].clone()));
    }

    let n = relations.len();

    // Each slot starts as a Leaf; `None` after retirement.
    let mut slots: Vec<Option<JoinTree>> =
        (0..n).map(|i| Some(JoinTree::Leaf(relations[i].clone()))).collect();
    // `owner[orig]` = slot index where original tensor `orig` currently resides.
    let mut owner: Vec<usize> = (0..n).collect();

    for &(i, j) in order {
        if i >= n || j >= n {
            return Err(Error::InvalidArg(format!(
                "contraction_to_join_tree: index out of range in step ({i}, {j}) (n = {n})"
            )));
        }
        let a = owner[i];
        let b = owner[j];
        if a == b {
            return Err(Error::InvalidArg(format!(
                "contraction_to_join_tree: step ({i}, {j}) joins a tensor with itself (already in slot {a})"
            )));
        }
        let tree_a = slots[a].take().ok_or_else(|| {
            Error::InvalidArg(format!(
                "contraction_to_join_tree: slot {a} (for tensor {i}) is empty"
            ))
        })?;
        let tree_b = slots[b].take().ok_or_else(|| {
            Error::InvalidArg(format!(
                "contraction_to_join_tree: slot {b} (for tensor {j}) is empty"
            ))
        })?;

        let card_a = tree_a.cardinality();
        let card_b = tree_b.cardinality();
        let cost_a = tree_a.cost();
        let cost_b = tree_b.cost();
        let join_cost = cost_a + cost_b + (card_a as f64) * (card_b as f64);
        let new_card = card_a.max(card_b);

        let merged = JoinTree::Inner {
            left: Box::new(tree_a),
            right: Box::new(tree_b),
            cost: join_cost,
            cardinality: new_card,
        };

        // Keep slot min(a, b), retire slot max(a, b).
        let (keep, remove) = if a < b { (a, b) } else { (b, a) };
        slots[keep] = Some(merged);
        slots[remove] = None;
        for o in &mut owner {
            if *o == remove {
                *o = keep;
            }
        }
    }

    // Exactly one slot should remain; otherwise the order was incomplete.
    let remaining: Vec<JoinTree> = slots.into_iter().flatten().collect();
    match remaining.len() {
        0 => Err(Error::InvalidArg(
            "contraction_to_join_tree: no slots remain after applying the order".into(),
        )),
        1 => Ok(remaining.into_iter().next().expect("exactly one tree")),
        n => Err(Error::InvalidArg(format!(
            "contraction_to_join_tree: {n} subtrees remain after {} steps (expected 1); the order is incomplete",
            order.len(),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::agm::JoinHypergraph;

    /// Build a 3-relation triangle network + relations for testing.
    fn triangle() -> (TensorNetwork, Vec<JoinRelation>) {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        let relations = vec![
            JoinRelation { name: "R".into(), cardinality: 100, joins_with: vec![1, 2] },
            JoinRelation { name: "S".into(), cardinality: 100, joins_with: vec![0, 2] },
            JoinRelation { name: "T".into(), cardinality: 100, joins_with: vec![0, 1] },
        ];
        let net = TensorNetwork::from_hypergraph(&graph, &[100, 100, 100]);
        (net, relations)
    }

    /// Count relations in a tree (helper).
    fn count_relations(tree: &JoinTree) -> usize {
        match tree {
            JoinTree::Leaf(_) => 1,
            JoinTree::Inner { left, right, .. } => count_relations(left) + count_relations(right),
        }
    }

    /// Test 17-5: contraction_to_join_tree produces a valid JoinTree.
    #[test]
    fn contraction_to_join_tree_produces_valid_tree() {
        let (net, relations) = triangle();
        let order = vec![(0, 1), (0, 2)];
        let tree = contraction_to_join_tree(&net, &order, &relations).expect("should succeed");
        assert_eq!(count_relations(&tree), 3, "tree should contain 3 relations");
        // Cost: step (0,1) = 0 + 0 + 100*100 = 10000, card = 100.
        //       step (0,2) = 10000 + 0 + 100*100 = 20000, card = 100.
        assert!((tree.cost() - 20_000.0).abs() < 1e-9, "expected cost 20000, got {}", tree.cost());
        assert_eq!(tree.cardinality(), 100);
    }

    /// Contraction order from `optimal_contraction_order` produces a
    /// valid tree directly.
    #[test]
    fn optimal_order_to_join_tree() {
        let (net, relations) = triangle();
        let order = net.optimal_contraction_order();
        assert_eq!(order.len(), 2);
        let tree = contraction_to_join_tree(&net, &order, &relations).expect("should succeed");
        assert_eq!(count_relations(&tree), 3);
        assert!(tree.cost() > 0.0);
    }

    /// Empty relations list → error.
    #[test]
    fn empty_relations_errors() {
        let net = TensorNetwork { tensors: Vec::new(), attributes: Vec::new() };
        let result = contraction_to_join_tree(&net, &[], &[]);
        assert!(result.is_err());
    }

    /// Mismatched tensors / relations length → error.
    #[test]
    fn mismatched_lengths_error() {
        let (net, _relations) = triangle();
        let wrong_relations = vec![
            JoinRelation { name: "X".into(), cardinality: 10, joins_with: vec![] },
            JoinRelation { name: "Y".into(), cardinality: 20, joins_with: vec![] },
        ];
        let result = contraction_to_join_tree(&net, &[(0, 1)], &wrong_relations);
        assert!(result.is_err());
    }

    /// Out-of-range index in order → error.
    #[test]
    fn out_of_range_index_errors() {
        let (net, relations) = triangle();
        let result = contraction_to_join_tree(&net, &[(0, 99)], &relations);
        assert!(result.is_err());
    }

    /// Self-contraction (i == j already in same subtree) → error.
    #[test]
    fn self_contraction_errors() {
        let (net, relations) = triangle();
        // (0,1) merges them; (0,1) again tries to merge a slot with itself.
        let result = contraction_to_join_tree(&net, &[(0, 1), (0, 1)], &relations);
        assert!(result.is_err());
    }

    /// Incomplete order (fewer than n-1 steps) → error.
    #[test]
    fn incomplete_order_errors() {
        let (net, relations) = triangle();
        // Only 1 step for 3 tensors → 2 subtrees remain.
        let result = contraction_to_join_tree(&net, &[(0, 1)], &relations);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("incomplete"), "error should mention incomplete order, got: {err}");
    }

    /// Single relation, no steps → returns the leaf directly.
    #[test]
    fn single_relation_no_steps_returns_leaf() {
        let graph = JoinHypergraph::from_named(&["A"], &[vec!["A"]]);
        let net = TensorNetwork::from_hypergraph(&graph, &[42]);
        let relations =
            vec![JoinRelation { name: "R".into(), cardinality: 42, joins_with: vec![] }];
        let tree = contraction_to_join_tree(&net, &[], &relations).expect("should succeed");
        match tree {
            JoinTree::Leaf(r) => {
                assert_eq!(r.name, "R");
                assert_eq!(r.cardinality, 42);
            }
            other => panic!("expected Leaf, got {other:?}"),
        }
    }

    /// Cost formula matches DPccp for a 2-table join: cost = |A|·|B|.
    #[test]
    fn cost_matches_dpccp_formula() {
        let graph = JoinHypergraph::from_named(&["A", "B"], &[vec!["A", "B"], vec!["A", "B"]]);
        let net = TensorNetwork::from_hypergraph(&graph, &[100, 200]);
        let relations = vec![
            JoinRelation { name: "R".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "S".into(), cardinality: 200, joins_with: vec![0] },
        ];
        let tree = contraction_to_join_tree(&net, &[(0, 1)], &relations).expect("should succeed");
        // 100 * 200 = 20000.
        assert!((tree.cost() - 20_000.0).abs() < 1e-9, "cost = {}, expected 20000", tree.cost());
        assert_eq!(tree.cardinality(), 200); // max(100, 200)
    }
}
