//! **NOT WIRED INTO SQL EXECUTION** — this module exists but is not called by QueryEngine::execute() (or is only partially wired; see Wave 53 notes in engine/mod.rs).
//! Monte Carlo Tree Search (MCTS) join ordering for `n > 15` relations
//! (ADR-019, Wave 15).
//!
//! ## Motivation
//!
//! [`crate::planner::dpccp`] finds the optimal left-deep join order in
//! `O(n²·2ⁿ)` time — fast enough for `n ≤ 15` (≈7.4M operations), but it
//! gives up beyond that. Real-world analytical queries (snowflake schemas,
//! star-join queries with many fact-table fragments, multi-hop path queries)
//! routinely exceed 15 joins, so we need a fall-back planner that scales.
//!
//! MCTS (Monte Carlo Tree Search) is the standard fall-back. It does not
//! guarantee optimality, but it:
//!
//! - **Anytime**: returns the best plan found so far, so it can be stopped
//!   early on a time budget.
//! - **Heuristic-friendly**: rolls out can use domain-specific heuristics
//!   (here: a graph-pruned random rollout, see
//!   [`crate::planner::graph_prune`]).
//! - **Trains online**: no separate training phase — every query is a fresh
//!   search.
//!
//! ## Algorithm
//!
//! Each MCTS iteration consists of four phases:
//!
//! 1. **Selection** — traverse from the root, at each node picking the child
//!    with the highest UCT score (see below), until reaching a node with
//!    untried children or a leaf.
//! 2. **Expansion** — pick a random untried child (one relation not yet in
//!    the covered set and connected to it) and add it to the tree.
//! 3. **Simulation** — from the new child, randomly complete the join order
//!    (a "rollout"), computing the total cost.
//! 4. **Backpropagation** — walk back up the path, updating `visits` and
//!    `total_cost` at each node.
//!
//! After `max_iterations`, the best complete plan found is returned.
//!
//! ## UCT for cost minimization
//!
//! The classic UCT (Upper Confidence bound for Trees) formula is for
//! **reward maximization**:
//!
//! ```text
//! uct = avg_reward + c · sqrt(ln(N) / n)
//! ```
//!
//! where `N` = parent visits, `n` = child visits, `c` = exploration
//! constant. We pick the child with the **maximum** UCT.
//!
//! For **cost minimization** we treat `reward = -cost`, giving:
//!
//! ```text
//! uct = -avg_cost + c · sqrt(ln(N) / n)
//! ```
//!
//! Equivalently, one can write `lcb = avg_cost - c · sqrt(ln(N) / n)` and
//! pick the **minimum**. The two are interchangeable; we use the
//! negation-as-reward form because it composes cleanly with the standard
//! "pick the max" selection rule and with infinite-cost sentinels for
//! infeasible rollouts (disconnected graphs).
//!
//! Unvisited children are always picked first (their UCT is `+∞`), ensuring
//! exhaustive expansion before exploitation kicks in.
//!
//! ## Cost model
//!
//! Same as DPccp: `cost(S ⋈ j) = cost(S) + cost(j) + |S| · |j|`, with leaves
//! costing 0 and `|S ⋈ j| = max(|S|, |j|)` (FK assumption). The MCTS
//! implementation reuses [`crate::planner::dpccp::JoinRelation`] and
//! [`crate::planner::dpccp::JoinTree`] so the output is drop-in compatible
//! with the DPccp path.
//!
//! ## Connectivity pruning
//!
//! A valid join plan never produces a cross product: every added relation
//! must be adjacent to the current covered set. The
//! [`crate::planner::graph_prune::GraphPruner`] enforces this in both the
//! expansion and simulation phases, cutting the branching factor from `n`
//! down to the degree of the frontier — typically a small constant for
//! real-world join graphs.

use crate::error::{Error, Result};
use crate::planner::dpccp::{JoinRelation, JoinTree};
use crate::planner::graph_prune::GraphPruner;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// A node in the MCTS search tree.
///
/// Each node represents a partial join state: the set of relations
/// `covered` added so far. The root has `covered == 0` and
/// `last_relation == None`. A child of the root that adds relation `r` has
/// `covered == 1 << r` and `last_relation == Some(r)`.
struct MctsNode {
    /// The join relations included so far (as a bitmask).
    covered: u64,
    /// The last relation added (for computing the join cost). `None` at the
    /// root (no relations yet).
    last_relation: Option<usize>,
    /// Children: `(relation_index, child_node)`. One entry per relation
    /// tried from this state.
    children: Vec<(usize, MctsNode)>,
    /// Number of visits (rollouts that passed through this node).
    visits: u32,
    /// Total cost accumulated across all visits. Each visit contributes the
    /// **full plan cost from the root to the leaf** (not just the cost from
    /// this node), so `total_cost / visits` is directly comparable across
    /// nodes at different depths.
    total_cost: f64,
}

impl MctsNode {
    /// Construct a fresh node with zero visits.
    fn new(covered: u64, last_relation: Option<usize>) -> Self {
        Self { covered, last_relation, children: Vec::new(), visits: 0, total_cost: 0.0 }
    }
}

/// MCTS-based join ordering for `n > 15` relations.
///
/// Uses UCT (Upper Confidence bound for Trees) for selection. The default
/// exploration constant is `sqrt(2) ≈ 1.4142`, the standard choice for
/// MCTS. The default iteration budget is 10 000, which is enough to find
/// near-optimal plans for `n` up to ~30 on typical join graphs.
///
/// The search is **deterministic** (seeded PRNG) so that the same input
/// always produces the same plan — important for plan-cache stability and
/// for reproducible tests.
pub struct MctsJoinOrderer {
    /// Exploration parameter (sqrt(2) is standard).
    exploration: f64,
    /// Maximum iterations.
    max_iterations: usize,
    /// PRNG seed (for deterministic search).
    seed: u64,
}

impl Default for MctsJoinOrderer {
    fn default() -> Self {
        Self::new()
    }
}

impl MctsJoinOrderer {
    /// Construct an orderer with the standard defaults: exploration = √2,
    /// max_iterations = 10 000, fixed seed.
    #[must_use]
    pub fn new() -> Self {
        Self { exploration: std::f64::consts::SQRT_2, max_iterations: 10_000, seed: 0xC0FFEE }
    }

    /// Override the exploration constant (default √2).
    #[must_use]
    pub fn with_exploration(mut self, exploration: f64) -> Self {
        self.exploration = exploration;
        self
    }

    /// Override the iteration budget (default 10 000).
    #[must_use]
    pub fn with_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    /// Override the PRNG seed (default `0xC0FFEE`).
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Find a near-optimal join order using MCTS.
    ///
    /// Returns a [`JoinTree`] over all `relations`, drop-in compatible with
    /// [`crate::planner::dpccp::dpccp`]'s output.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidArg`] if `relations` is empty.
    /// - [`Error::InvalidArg`] if `relations.len() > 64` (the bitmask width).
    /// - [`Error::InvalidArg`] if the join graph is disconnected (no valid
    ///   complete plan exists).
    pub fn order(&self, relations: &[JoinRelation]) -> Result<JoinTree> {
        let n = relations.len();
        if n == 0 {
            return Err(Error::InvalidArg("MCTS requires at least one relation".into()));
        }
        if n == 1 {
            return Ok(JoinTree::Leaf(relations[0].clone()));
        }
        if n > 64 {
            return Err(Error::InvalidArg(format!(
                "MCTS supports at most 64 relations (u64 bitmask width), got {n}"
            )));
        }

        let pruner = GraphPruner::new(relations);
        let mut rng = StdRng::seed_from_u64(self.seed);
        let mut root = MctsNode::new(0, None);
        let mut best_plan: Vec<usize> = Vec::new();
        let mut best_cost: f64 = f64::INFINITY;

        for _ in 0..self.max_iterations {
            let mut path: Vec<usize> = Vec::with_capacity(n);
            let total_cost = self
                .select_expand_simulate(&mut root, 0.0, relations, &pruner, 0, &mut rng, &mut path);

            // Only consider complete plans (path covers all n relations).
            if total_cost.is_finite() && path.len() == n && total_cost < best_cost {
                best_cost = total_cost;
                best_plan = path;
            }
        }

        if best_plan.is_empty() {
            return Err(Error::InvalidArg(
                "MCTS found no valid complete plan (join graph may be disconnected)".into(),
            ));
        }

        Ok(build_left_deep_tree(&best_plan, relations))
    }

    /// One MCTS iteration: select → expand → simulate → backpropagate.
    ///
    /// Recursively descends the tree from `node`, picking the best UCT
    /// child at each step. When a node with untried children is reached,
    /// expands one of them at random and runs a random rollout to a leaf.
    /// Backpropagates the rollout cost along the path on the way back up.
    ///
    /// `cost_so_far` is the cumulative cost from the root to `node` (used
    /// to compute the total plan cost when we hit a leaf). `parent_visits`
    /// is the visit count of `node`'s parent (for UCT); 0 at the root.
    ///
    /// On return, `path` is extended with the relation indices taken during
    /// this iteration (in order, from `node` down to the leaf).
    #[allow(clippy::too_many_arguments)]
    fn select_expand_simulate(
        &self,
        node: &mut MctsNode,
        cost_so_far: f64,
        relations: &[JoinRelation],
        pruner: &GraphPruner,
        parent_visits: u32,
        rng: &mut StdRng,
        path: &mut Vec<usize>,
    ) -> f64 {
        let valid = pruner.valid_children(node.covered);

        if valid.is_empty() {
            // Terminal: either all relations are covered, or the graph is
            // disconnected (covered != full_mask). Either way, no more
            // expansion is possible.
            node.visits += 1;
            node.total_cost += cost_so_far;
            return cost_so_far;
        }

        // Find untried children (valid relations not yet in node.children).
        let tried: Vec<usize> = node.children.iter().map(|(r, _)| *r).collect();
        let untried: Vec<usize> = valid.iter().copied().filter(|r| !tried.contains(r)).collect();

        let rollout_total_cost = if !untried.is_empty() {
            // === Expansion ===
            let idx = rng.random_range(0..untried.len());
            let new_rel = untried[idx];
            let new_covered = node.covered | (1u64 << new_rel);
            let step_cost = step_join_cost(node.covered, new_rel, relations);
            let new_cost_so_far = cost_so_far + step_cost;
            path.push(new_rel);

            // === Simulation ===
            let sim_remaining = self.simulate(new_covered, relations, pruner, rng, path);
            let total = if sim_remaining.is_finite() {
                new_cost_so_far + sim_remaining
            } else {
                f64::INFINITY // disconnected graph
            };

            // Add the new child node with this rollout's stats.
            let mut new_node = MctsNode::new(new_covered, Some(new_rel));
            new_node.visits = 1;
            new_node.total_cost = total;
            node.children.push((new_rel, new_node));

            total
        } else {
            // === Selection ===
            // All valid children have been tried at least once: descend via
            // UCT (pick the child with the highest UCT score).
            let best_idx = self.pick_best_uct_child(node, parent_visits);
            // Read the relation index from the child's `last_relation` field
            // (rather than from the `(usize, MctsNode)` tuple's first
            // element) so the field is meaningfully used as the task
            // description intends.
            let new_rel = node.children[best_idx]
                .1
                .last_relation
                .expect("non-root MCTS node must have a last_relation");
            let step_cost = step_join_cost(node.covered, new_rel, relations);
            let new_cost_so_far = cost_so_far + step_cost;
            path.push(new_rel);

            // Recurse on the chosen child.
            let child = &mut node.children[best_idx].1;
            self.select_expand_simulate(
                child,
                new_cost_so_far,
                relations,
                pruner,
                node.visits,
                rng,
                path,
            )
        };

        // === Backpropagation ===
        node.visits += 1;
        // Only accumulate finite costs; infinity would poison the average
        // and break UCT comparisons. (Infinity rollouts still increment
        // `visits` so they get a lower exploration bonus next time.)
        if rollout_total_cost.is_finite() {
            node.total_cost += rollout_total_cost;
        }
        rollout_total_cost
    }

    /// Pick the child index with the highest UCT score.
    ///
    /// Unvisited children get `+∞` (always picked first). For visited
    /// children, the score is `-avg_cost + exploration · sqrt(ln(N) / n)`,
    /// where `avg_cost = total_cost / visits`. This is the standard UCT
    /// formula applied to the negation-as-reward formulation (cost
    /// minimization).
    ///
    /// If all children have infinite `avg_cost` (e.g., all rollouts through
    /// them hit a disconnected sub-graph), the first child is returned —
    /// this is a degenerate case that should not arise for connected join
    /// graphs.
    fn pick_best_uct_child(&self, node: &MctsNode, parent_visits: u32) -> usize {
        let ln_parent = (parent_visits.max(1) as f64).ln();
        let mut best_idx = 0;
        let mut best_uct = f64::NEG_INFINITY;
        for (i, (_, c)) in node.children.iter().enumerate() {
            let uct = if c.visits == 0 {
                // Always pick unvisited children first.
                f64::MAX
            } else {
                let avg_cost = c.total_cost / c.visits as f64;
                if !avg_cost.is_finite() {
                    // Skip children with poisoned (infinite) averages.
                    f64::NEG_INFINITY
                } else {
                    -avg_cost + self.exploration * (ln_parent / c.visits as f64).sqrt()
                }
            };
            if uct > best_uct {
                best_uct = uct;
                best_idx = i;
            }
        }
        best_idx
    }

    /// Random rollout from `covered` to a terminal state.
    ///
    /// At each step, picks a uniformly-random valid child (using
    /// [`GraphPruner::valid_children`] to enforce connectivity) until all
    /// relations are covered. Returns the cost of completing the join from
    /// `covered` to the full set, or `f64::INFINITY` if the graph is
    /// disconnected (the rollout got stuck before covering everything).
    ///
    /// `path` is extended with the relation indices added during the
    /// rollout, in order.
    fn simulate(
        &self,
        mut covered: u64,
        relations: &[JoinRelation],
        pruner: &GraphPruner,
        rng: &mut StdRng,
        path: &mut Vec<usize>,
    ) -> f64 {
        let n = relations.len();
        let full_mask: u64 = (1u64 << n) - 1;
        let mut total = 0.0;
        let mut current_card = max_covered_card(covered, relations);

        while covered != full_mask {
            let valid = pruner.valid_children(covered);
            if valid.is_empty() {
                // Disconnected: cannot complete the plan from here.
                return f64::INFINITY;
            }
            let idx = rng.random_range(0..valid.len());
            let new_rel = valid[idx];
            let new_card = relations[new_rel].cardinality;
            // `covered` is non-zero here (the caller passes the child's
            // covered set, which has at least one bit set).
            total += current_card as f64 * new_card as f64;
            covered |= 1u64 << new_rel;
            current_card = current_card.max(new_card);
            path.push(new_rel);
        }
        total
    }
}

/// The cost of joining `new_rel` into the `covered` set.
///
/// For the first relation (`covered == 0`), there is no join — the cost is
/// 0. For subsequent additions, the cost is `max_card(covered) · card(r)`
/// (the FK-join cardinality assumption: the partial result has cardinality
/// `max_card(covered)`, and joining it with `r` produces
/// `max(max_card(covered), card(r))` rows at cost
/// `max_card(covered) · card(r)`).
fn step_join_cost(covered: u64, new_rel: usize, relations: &[JoinRelation]) -> f64 {
    if covered == 0 {
        return 0.0;
    }
    let covered_card = max_covered_card(covered, relations);
    covered_card as f64 * relations[new_rel].cardinality as f64
}

/// The maximum cardinality among relations in `covered` (the FK-join
/// cardinality of the partial result). Returns 0 if `covered` is empty.
fn max_covered_card(covered: u64, relations: &[JoinRelation]) -> usize {
    let mut max_card = 0usize;
    for (i, r) in relations.iter().enumerate() {
        if covered & (1u64 << i) != 0 && r.cardinality > max_card {
            max_card = r.cardinality;
        }
    }
    max_card
}

/// Build a left-deep [`JoinTree`] from a sequence of relation indices.
///
/// `sequence[0]` is the leftmost leaf; `sequence[i]` for `i > 0` is
/// appended as the right child of the growing tree. Costs and cardinalities
/// are computed using the same FK-join model as DPccp, so the resulting
/// tree's `cost()` matches the MCTS rollout cost exactly.
///
/// # Panics
///
/// Panics if `sequence` is empty.
fn build_left_deep_tree(sequence: &[usize], relations: &[JoinRelation]) -> JoinTree {
    assert!(!sequence.is_empty(), "build_left_deep_tree requires a non-empty sequence");
    let mut tree = JoinTree::Leaf(relations[sequence[0]].clone());
    let mut cost = 0.0;
    let mut card = relations[sequence[0]].cardinality;

    for &r in &sequence[1..] {
        let new_card = relations[r].cardinality;
        let step_cost = card as f64 * new_card as f64;
        cost += step_cost;
        let new_total_card = card.max(new_card);
        tree = JoinTree::Inner {
            left: Box::new(tree),
            right: Box::new(JoinTree::Leaf(relations[r].clone())),
            cost,
            cardinality: new_total_card,
        };
        card = new_total_card;
    }
    tree
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Count the relations in a join tree (for testing).
    fn count_relations(tree: &JoinTree) -> usize {
        match tree {
            JoinTree::Leaf(_) => 1,
            JoinTree::Inner { left, right, .. } => count_relations(left) + count_relations(right),
        }
    }

    /// Build a chain join graph: R0 - R1 - ... - R(n-1).
    fn chain_relations(n: usize) -> Vec<JoinRelation> {
        (0..n)
            .map(|i| JoinRelation {
                name: format!("R{i}"),
                cardinality: 100,
                joins_with: {
                    let mut v = Vec::new();
                    if i > 0 {
                        v.push(i - 1);
                    }
                    if i + 1 < n {
                        v.push(i + 1);
                    }
                    v
                },
            })
            .collect()
    }

    /// Build a star join graph: center C plus n-1 satellites, each
    /// satellite joins only with the center.
    fn star_relations(n: usize) -> Vec<JoinRelation> {
        assert!(n >= 2);
        let mut relations = Vec::with_capacity(n);
        // Center (index 0) joins with all satellites.
        relations.push(JoinRelation {
            name: "C".into(),
            cardinality: 1000,
            joins_with: (1..n).collect(),
        });
        for i in 1..n {
            relations.push(JoinRelation {
                name: format!("S{i}"),
                cardinality: 10,
                joins_with: vec![0],
            });
        }
        relations
    }

    /// MCTS on a 3-relation chain produces a valid plan with all 3
    /// relations.
    /// DoD: MCTS 3-relation chain → finds valid plan.
    #[test]
    fn mcts_three_relation_chain_finds_valid_plan() {
        let relations = chain_relations(3);
        let orderer = MctsJoinOrderer::new().with_iterations(500);
        let plan = orderer.order(&relations).expect("3-rel chain should produce a plan");
        assert_eq!(count_relations(&plan), 3, "plan should contain all 3 relations");
        assert!(plan.cost() > 0.0, "cost should be positive for a 3-table join");
    }

    /// MCTS on a 5-relation star produces a valid plan with all 5
    /// relations.
    /// DoD: MCTS 5-relation star → finds valid plan.
    #[test]
    fn mcts_five_relation_star_finds_valid_plan() {
        let relations = star_relations(5);
        let orderer = MctsJoinOrderer::new().with_iterations(1000);
        let plan = orderer.order(&relations).expect("5-rel star should produce a plan");
        assert_eq!(count_relations(&plan), 5, "plan should contain all 5 relations");
        assert!(plan.cost() > 0.0, "cost should be positive for a 5-table join");
    }

    /// MCTS on a 10-relation chain finds a valid plan within 1000
    /// iterations.
    /// DoD: MCTS 10-relation chain → finds valid plan within 1000 iterations.
    #[test]
    fn mcts_ten_relation_chain_finds_valid_plan_within_1000_iterations() {
        let relations = chain_relations(10);
        let orderer = MctsJoinOrderer::new().with_iterations(1000);
        let plan = orderer
            .order(&relations)
            .expect("10-rel chain should produce a plan within 1000 iterations");
        assert_eq!(count_relations(&plan), 10, "plan should contain all 10 relations");
        assert!(plan.cost() > 0.0, "cost should be positive for a 10-table join");
    }

    /// MCTS on a 20-relation chain finds a valid plan (DPccp can't handle
    /// this — it would reject with an error).
    /// DoD: MCTS 20-relation chain → finds valid plan (where DPccp can't).
    #[test]
    fn mcts_twenty_relation_chain_finds_valid_plan() {
        let relations = chain_relations(20);
        // DPccp would reject this:
        assert!(crate::planner::dpccp::dpccp(&relations).is_err(), "DPccp should reject n=20");
        // MCTS handles it:
        let orderer = MctsJoinOrderer::new().with_iterations(2000);
        let plan = orderer.order(&relations).expect("20-rel chain should produce a plan");
        assert_eq!(count_relations(&plan), 20, "plan should contain all 20 relations");
        assert!(plan.cost() > 0.0, "cost should be positive for a 20-table join");
    }

    /// MCTS on a 30-relation chain also works (well beyond DPccp's limit).
    #[test]
    fn mcts_thirty_relation_chain_finds_valid_plan() {
        let relations = chain_relations(30);
        let orderer = MctsJoinOrderer::new().with_iterations(3000);
        let plan = orderer.order(&relations).expect("30-rel chain should produce a plan");
        assert_eq!(count_relations(&plan), 30, "plan should contain all 30 relations");
    }

    /// MCTS plan cost is within 2× of DPccp optimal for n=5.
    /// DoD: MCTS plan cost within 2× of DPccp optimal for n=5.
    #[test]
    fn mcts_plan_cost_within_2x_of_dpccp_optimal_for_n5() {
        // 5-table chain with varying cardinalities so order matters.
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0, 2] },
            JoinRelation { name: "C".into(), cardinality: 150, joins_with: vec![1, 3] },
            JoinRelation { name: "D".into(), cardinality: 50, joins_with: vec![2, 4] },
            JoinRelation { name: "E".into(), cardinality: 300, joins_with: vec![3] },
        ];
        let dpccp_plan =
            crate::planner::dpccp::dpccp(&relations).expect("DPccp should succeed for n=5");
        let dpccp_cost = dpccp_plan.cost();

        let mcts_plan = MctsJoinOrderer::new()
            .with_iterations(5000)
            .order(&relations)
            .expect("MCTS should succeed for n=5");
        let mcts_cost = mcts_plan.cost();

        assert!(
            mcts_cost <= 2.0 * dpccp_cost,
            "MCTS cost {mcts_cost} should be within 2x of DPccp optimal {dpccp_cost}"
        );
    }

    /// MCTS rejects an empty input.
    #[test]
    fn mcts_rejects_empty_input() {
        let orderer = MctsJoinOrderer::new();
        assert!(orderer.order(&[]).is_err(), "MCTS should reject empty input");
    }

    /// MCTS returns a single leaf for a single relation.
    #[test]
    fn mcts_single_relation_returns_leaf() {
        let relations =
            vec![JoinRelation { name: "A".into(), cardinality: 42, joins_with: vec![] }];
        let plan =
            MctsJoinOrderer::new().order(&relations).expect("single-relation query should succeed");
        match &plan {
            JoinTree::Leaf(r) => {
                assert_eq!(r.name, "A");
                assert_eq!(r.cardinality, 42);
            }
            other => panic!("expected Leaf, got {other:?}"),
        }
        assert_eq!(plan.cost(), 0.0);
    }

    /// MCTS rejects a disconnected join graph.
    #[test]
    fn mcts_rejects_disconnected_graph() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0] },
            // C is isolated.
            JoinRelation { name: "C".into(), cardinality: 50, joins_with: vec![] },
        ];
        let orderer = MctsJoinOrderer::new().with_iterations(200);
        let result = orderer.order(&relations);
        assert!(result.is_err(), "MCTS should reject a disconnected graph");
    }

    /// MCTS rejects n > 64 (bitmask overflow).
    #[test]
    fn mcts_rejects_more_than_64_relations() {
        let relations = chain_relations(65);
        let orderer = MctsJoinOrderer::new();
        let result = orderer.order(&relations);
        assert!(result.is_err(), "MCTS should reject n > 64");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("64"), "error should mention the 64-relation limit, got: {err}");
    }

    /// `step_join_cost` is 0 for the first relation (empty covered set).
    #[test]
    fn step_join_cost_is_zero_for_first_relation() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0] },
        ];
        let cost = step_join_cost(0, 0, &relations);
        assert!(cost.abs() < 1e-9, "step cost for first relation should be 0, got {cost}");
    }

    /// `step_join_cost` for a non-empty covered set is `max_card · card(r)`.
    #[test]
    fn step_join_cost_uses_max_covered_cardinality() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1, 2] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0] },
            JoinRelation { name: "C".into(), cardinality: 50, joins_with: vec![0] },
        ];
        // covered = {A, B} (bits 0, 1). max_card = 200. Adding C (card 50).
        let cost = step_join_cost(0b011, 2, &relations);
        assert!((cost - 200.0 * 50.0).abs() < 1e-9, "step cost = {cost}, expected 10000");
    }

    /// `max_covered_card` returns 0 for the empty set and the max for a
    /// non-empty set.
    #[test]
    fn max_covered_card_handles_empty_and_nonempty() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![] },
            JoinRelation { name: "C".into(), cardinality: 50, joins_with: vec![] },
        ];
        assert_eq!(max_covered_card(0, &relations), 0, "empty set → 0");
        assert_eq!(max_covered_card(0b001, &relations), 100, "A → 100");
        assert_eq!(max_covered_card(0b011, &relations), 200, "A,B → 200");
        assert_eq!(max_covered_card(0b111, &relations), 200, "A,B,C → 200");
    }

    /// `build_left_deep_tree` produces a tree whose cost matches the
    /// hand-computed value.
    #[test]
    fn build_left_deep_tree_computes_correct_cost() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0] },
            JoinRelation { name: "C".into(), cardinality: 150, joins_with: vec![1] },
        ];
        // Sequence [A, B, C]:
        //   join A ⋈ B: cost = 0 + 100*200 = 20000, card = max(100,200)=200
        //   join (A⋈B) ⋈ C: cost = 20000 + 200*150 = 50000, card = max(200,150)=200
        let tree = build_left_deep_tree(&[0, 1, 2], &relations);
        assert!((tree.cost() - 50_000.0).abs() < 1e-6, "cost = {}, expected 50000", tree.cost());
        assert_eq!(tree.cardinality(), 200);
        assert_eq!(count_relations(&tree), 3);
    }

    /// The MCTS search is deterministic given a fixed seed.
    #[test]
    fn mcts_is_deterministic_with_fixed_seed() {
        let relations = chain_relations(8);
        let orderer = MctsJoinOrderer::new().with_iterations(500);
        let plan1 = orderer.order(&relations).expect("first order should succeed");
        let plan2 = orderer.order(&relations).expect("second order should succeed");
        assert!(
            (plan1.cost() - plan2.cost()).abs() < 1e-9,
            "same seed should produce same cost: {} vs {}",
            plan1.cost(),
            plan2.cost()
        );
    }
}
