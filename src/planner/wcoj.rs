//! Worst-case optimal join (WCOJ) plan selection.
//!
//! When the AGM bound ([`crate::planner::agm`]) for a multi-way join is much
//! smaller than the naive `∏ |Ri|` product, a worst-case optimal join
//! algorithm — turboGP's [`LeapfrogJoin`](crate::kernel::leapfrog::LeapfrogJoin)
//! — runs in `O(IN + OUT + AGM)` time, beating a cascade of binary hash joins
//! (which can blow up to `∏ |Ri|` on cyclic queries such as the triangle).
//!
//! ## Decision rule
//!
//! [`choose_join_algorithm`] computes the AGM bound and the naive product of
//! the input cardinalities, then picks:
//!
//! - **Leapfrog** when `agm_bound < product / 2` — the WCOJ bound is less
//!   than half the naive product, so the cyclic structure of the query
//!   gives leapfrog a clear asymptotic win.
//! - **HashJoin** otherwise — the query is acyclic (or the inputs are small
//!   enough that the constant-factor overhead of leapfrog's iterator
//!   dispatch eats the asymptotic gain), and the binary hash join's tight
//!   inner loop wins.
//!
//! The `1/2` factor is a deliberate safety margin: leapfrog's per-iteration
//! cost (a binary seek + a comparison across N iterators) is higher than a
//! hash probe's per-key cost, so we require a clear asymptotic win before
//! switching.
//!
//! ## WCOJ plan
//!
//! When leapfrog is selected, [`build_wcoj_plan`] produces a [`WcojPlan`]:
//!
//! - `relation_order` — relations sorted by ascending cardinality. Leapfrog
//!   processes the smallest iterator first because the algorithm's seek cost
//!   is `O(log |R|)` — driving from the smallest relation minimizes the
//!   number of seeks.
//! - `join_attributes` — the union of all attribute indices covered by the
//!   relations, sorted ascending. These are the columns the leapfrog trie
//!   must intersect on.
//! - `estimated_size` — the AGM bound, the worst-case output size.
//!
//! ## References
//!
//! - Ngo, Ré, Rudra, "Skew Strikes Back: New Developments in the Theory of
//!   Join Algorithms", SIGMOD Record 2014.
//! - Veldhuizen, "Leapfrog Triejoin: a Simple, Worst-Case Optimal Join
//!   Algorithm", ICDT 2014.

use crate::planner::agm::{agm_bound, JoinHypergraph};

/// The join algorithm chosen by [`choose_join_algorithm`] for a given query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinAlgorithm {
    /// Binary hash join (DPccp ordering, `Operator::HashProbe`).
    ///
    /// Picked when the query is acyclic or the AGM bound is not
    /// significantly smaller than the naive `∏ |Ri|` product. Hash joins
    /// have a tight inner loop (`VPCMPEQB` on SwissTable metadata) that
    /// beats leapfrog's per-key iterator dispatch when the asymptotic
    /// advantage is small.
    HashJoin,
    /// Worst-case optimal leapfrog triejoin
    /// ([`crate::kernel::leapfrog::LeapfrogJoin`]).
    ///
    /// Picked when the AGM bound is less than half the naive product — the
    /// cyclic structure of the query gives leapfrog a clear asymptotic win
    /// (`O(AGM)` vs `O(∏ |Ri|)`).
    Leapfrog,
}

/// A WCOJ execution plan: relations in processing order, the join attributes,
/// and an estimate of the output size.
///
/// Produced by [`build_wcoj_plan`] when [`choose_join_algorithm`] picks
/// [`JoinAlgorithm::Leapfrog`]. The executor feeds `relation_order` to the
/// [`LeapfrogJoin`](crate::kernel::leapfrog::LeapfrogJoin) kernel as a
/// sequence of sorted iterators.
#[derive(Debug, Clone)]
pub struct WcojPlan {
    /// Relations (indices into the hypergraph's `relations` vector) in the
    /// order they should be processed by the leapfrog kernel.
    ///
    /// Sorted by ascending cardinality: the smallest relation goes first.
    /// Leapfrog's seek cost is `O(log |R|)` per seek, so driving from the
    /// smallest relation minimizes the number of expensive seeks. Ties are
    /// broken by relation index (stable order).
    pub relation_order: Vec<usize>,
    /// The join attribute indices (into the hypergraph's `attributes`
    /// vector), sorted ascending.
    ///
    /// For a single-attribute intersection (e.g., `R(A) ⋈ S(A) ⋈ T(A)`),
    /// this is a single-element vector `[0]`. For a triangle query
    /// `R(A,B) ⋈ S(B,C) ⋈ T(A,C)`, it is `[0, 1, 2]` (all attributes).
    pub join_attributes: Vec<usize>,
    /// Estimated result size (the AGM bound).
    ///
    /// This is the worst-case output size of the join — leapfrog runs in
    /// `O(IN + OUT + AGM)` time, so this is also the worst-case runtime
    /// bound (modulo the input and output sizes).
    pub estimated_size: f64,
}

/// Decide whether to use WCOJ (leapfrog) or hash join for a given multi-way
/// join.
///
/// # Algorithm
///
/// 1. Compute the AGM bound via [`agm_bound`].
/// 2. Compute the naive product of the input cardinalities
///    (`∏ |Ri|`). This is the worst-case size of a *binary* join cascade.
/// 3. If `agm_bound < product / 2`, return [`JoinAlgorithm::Leapfrog`];
///    otherwise return [`JoinAlgorithm::HashJoin`].
///
/// The `1/2` factor is a safety margin: leapfrog's per-key cost is higher
/// than a hash probe's, so we require a clear asymptotic win before
/// switching.
///
/// # Edge cases
///
/// - Empty hypergraph (no relations) → [`JoinAlgorithm::HashJoin`] (there
///   is nothing to join; the default is harmless).
/// - Single relation → [`JoinAlgorithm::HashJoin`] (no join needed; the
///   product equals the cardinality, and the AGM bound equals it too, so
///   the `agm < product/2` test fails).
/// - Any zero-cardinality relation → AGM bound collapses to 0, so
///   [`JoinAlgorithm::Leapfrog`] is returned (the join is empty either way;
///   leapfrog will terminate immediately on the empty input).
///
/// # Example
///
/// Triangle query `R(A,B) ⋈ S(B,C) ⋈ T(A,C)` with `|R|=|S|=|T|=100`:
///
/// - Product = `100³ = 1 000 000`.
/// - AGM bound = `100^1.5 = 1000` (each attribute is covered by 2 of 3
///   relations, so the optimal fractional cover is `(0.5, 0.5, 0.5)`,
///   giving `100^(0.5·3) = 1000`).
/// - `1000 < 500 000` → Leapfrog.
///
/// ```
/// use turbogp::planner::agm::JoinHypergraph;
/// use turbogp::planner::wcoj::{choose_join_algorithm, JoinAlgorithm};
///
/// let graph = JoinHypergraph::from_named(
///     &["A", "B", "C"],
///     &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
/// );
/// // Triangle query, |R|=|S|=|T|=100 → AGM = 1000, product = 1M → Leapfrog.
/// assert_eq!(choose_join_algorithm(&graph, &[100, 100, 100]), JoinAlgorithm::Leapfrog);
/// ```
#[must_use]
pub fn choose_join_algorithm(graph: &JoinHypergraph, cardinalities: &[usize]) -> JoinAlgorithm {
    // Edge case: no relations or no cardinalities → nothing to join. Pick
    // the default (HashJoin); the caller will emit no join invocation
    // anyway because the plan has no Join node.
    if graph.relations.is_empty() || cardinalities.is_empty() {
        return JoinAlgorithm::HashJoin;
    }

    // Compute the naive product of cardinalities. Use saturating
    // multiplication to avoid panic on overflow (very large synthetic
    // cardinalities in tests).
    let n_rel = graph.relations.len().min(cardinalities.len());
    let mut product: f64 = 1.0;
    for &c in cardinalities.iter().take(n_rel) {
        product *= c as f64;
    }

    // If the product is 0 (some relation is empty), the join is empty;
    // leapfrog terminates immediately on the first exhausted iterator, so
    // it is at least as fast as a hash join (which would still build the
    // hash table). Prefer Leapfrog.
    if product == 0.0 {
        return JoinAlgorithm::Leapfrog;
    }

    // Compute the AGM bound.
    let agm = agm_bound(graph, cardinalities);

    // Decision rule: Leapfrog iff the AGM bound is < product / 2.
    if agm < product / 2.0 {
        JoinAlgorithm::Leapfrog
    } else {
        JoinAlgorithm::HashJoin
    }
}

/// Build a WCOJ execution plan for a multi-way join.
///
/// Returns the relations sorted by ascending cardinality (leapfrog processes
/// the smallest first because the algorithm's seek cost is `O(log |R|)` —
/// driving from the smallest relation minimizes the number of seeks), the
/// sorted union of all join attribute indices, and the AGM bound as the
/// estimated result size.
///
/// # Edge cases
///
/// - Empty hypergraph → returns a plan with empty `relation_order`,
///   empty `join_attributes`, and `estimated_size = 1.0` (the empty join
///   has one tuple).
/// - Single relation → `relation_order = [0]`, `join_attributes` = that
///   relation's attributes, `estimated_size = |R|`.
///
/// # Example
///
/// Triangle query with `|R|=50, |S|=100, |T|=200`:
///
/// ```
/// use turbogp::planner::agm::JoinHypergraph;
/// use turbogp::planner::wcoj::build_wcoj_plan;
///
/// let graph = JoinHypergraph::from_named(
///     &["A", "B", "C"],
///     &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
/// );
/// let plan = build_wcoj_plan(&graph, &[50, 100, 200]);
/// // Relations sorted by cardinality: [0 (50), 1 (100), 2 (200)].
/// assert_eq!(plan.relation_order, vec![0, 1, 2]);
/// // All attributes are join attributes.
/// assert_eq!(plan.join_attributes, vec![0, 1, 2]);
/// // AGM bound is roughly 50^0.5 · 100^0.5 · 200^0.5 = sqrt(50·100·200) ≈ 1000.
/// assert!(plan.estimated_size > 500.0 && plan.estimated_size < 2000.0,
///     "estimated_size = {}", plan.estimated_size);
/// ```
#[must_use]
pub fn build_wcoj_plan(graph: &JoinHypergraph, cardinalities: &[usize]) -> WcojPlan {
    // Edge case: empty hypergraph → trivial plan.
    if graph.relations.is_empty() || cardinalities.is_empty() {
        return WcojPlan {
            relation_order: Vec::new(),
            join_attributes: Vec::new(),
            estimated_size: 1.0,
        };
    }

    let n_rel = graph.relations.len().min(cardinalities.len());

    // Sort relation indices by ascending cardinality (stable on ties by
    // relation index, which `sort_by_key` provides for free).
    let mut relation_order: Vec<usize> = (0..n_rel).collect();
    relation_order.sort_by_key(|&i| cardinalities[i]);

    // Collect the union of all attribute indices covered by the relations.
    // Use a small sorted vec dedup since `n_attr` is tiny (≤ 15 in
    // practice, per ADR-019).
    let mut join_attributes: Vec<usize> = Vec::new();
    for i in 0..n_rel {
        for &a in &graph.relations[i] {
            if !join_attributes.contains(&a) {
                join_attributes.push(a);
            }
        }
    }
    join_attributes.sort_unstable();

    // Estimated size = AGM bound.
    let estimated_size = agm_bound(graph, cardinalities);

    WcojPlan { relation_order, join_attributes, estimated_size }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Triangle query (cyclic) → Leapfrog.
    ///
    /// `R(A,B) ⋈ S(B,C) ⋈ T(A,C)` with `|R|=|S|=|T|=100`:
    /// - Product = 1 000 000.
    /// - AGM = 1000.
    /// - `1000 < 500 000` → Leapfrog.
    #[test]
    fn choose_join_algorithm_picks_leapfrog_for_triangle() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        let algo = choose_join_algorithm(&graph, &[100, 100, 100]);
        assert_eq!(
            algo,
            JoinAlgorithm::Leapfrog,
            "cyclic triangle query should pick Leapfrog (AGM=1000, product=1M)"
        );
    }

    /// 2-table acyclic join on a shared attribute → HashJoin.
    ///
    /// `R(A,B) ⋈ S(B,C)` with `|R|=|S|=100`:
    /// - Product = 10 000.
    /// - AGM = 10 000 (the path query has `f_R = f_S = 1` since A and C are
    ///   only covered by one relation each).
    /// - `10 000 < 5 000` is false → HashJoin.
    #[test]
    fn choose_join_algorithm_picks_hashjoin_for_acyclic_two_table() {
        let graph = JoinHypergraph::from_named(&["A", "B", "C"], &[vec!["A", "B"], vec!["B", "C"]]);
        let algo = choose_join_algorithm(&graph, &[100, 100]);
        assert_eq!(
            algo,
            JoinAlgorithm::HashJoin,
            "acyclic 2-table path join should pick HashJoin (AGM=product=10K)"
        );
    }

    /// 2-table intersection on the *same* attribute → Leapfrog.
    ///
    /// `R(A) ⋈ S(A)` with `|R|=|S|=100`:
    /// - Product = 10 000.
    /// - AGM = 100 (the optimal cover is `f_R = f_S = 0.5`).
    /// - `100 < 5 000` → Leapfrog.
    ///
    /// This is the leapfrog's canonical case: multiway intersection on a
    /// single key.
    #[test]
    fn choose_join_algorithm_picks_leapfrog_for_intersection() {
        let graph = JoinHypergraph::from_named(&["A"], &[vec!["A"], vec!["A"]]);
        let algo = choose_join_algorithm(&graph, &[100, 100]);
        assert_eq!(
            algo,
            JoinAlgorithm::Leapfrog,
            "intersection on a single attribute should pick Leapfrog (AGM=100, product=10K)"
        );
    }

    /// Single relation → HashJoin (no join to do; product == AGM).
    #[test]
    fn choose_join_algorithm_single_relation_picks_hashjoin() {
        let graph = JoinHypergraph::from_named(&["A"], &[vec!["A"]]);
        let algo = choose_join_algorithm(&graph, &[100]);
        assert_eq!(
            algo,
            JoinAlgorithm::HashJoin,
            "single relation has AGM = product = |R|, so HashJoin is the default"
        );
    }

    /// Empty hypergraph → HashJoin (no join to do).
    #[test]
    fn choose_join_algorithm_empty_hypergraph_picks_hashjoin() {
        let graph = JoinHypergraph { attributes: vec![], relations: vec![] };
        let algo = choose_join_algorithm(&graph, &[]);
        assert_eq!(algo, JoinAlgorithm::HashJoin);
    }

    /// Any zero-cardinality relation → Leapfrog (the join is empty; leapfrog
    /// terminates immediately on the empty iterator).
    #[test]
    fn choose_join_algorithm_zero_cardinality_picks_leapfrog() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        // Triangle, but one relation is empty.
        let algo = choose_join_algorithm(&graph, &[100, 0, 100]);
        assert_eq!(
            algo,
            JoinAlgorithm::Leapfrog,
            "empty input → leapfrog terminates immediately, prefer it"
        );
    }

    /// `build_wcoj_plan` orders relations by ascending cardinality.
    #[test]
    fn build_wcoj_plan_orders_relations_by_cardinality() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        // Cardinalities: R=300, S=50, T=200 → order [1, 2, 0].
        let plan = build_wcoj_plan(&graph, &[300, 50, 200]);
        assert_eq!(
            plan.relation_order,
            vec![1, 2, 0],
            "expected relations ordered by ascending cardinality: [S(50), T(200), R(300)]"
        );
    }

    /// `build_wcoj_plan` returns the correct estimated size (AGM bound) for
    /// the triangle query.
    #[test]
    fn build_wcoj_plan_estimated_size_is_agm_bound() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        let n = 100usize;
        let plan = build_wcoj_plan(&graph, &[n, n, n]);
        let expected = (n as f64).powf(1.5); // = 1000
        assert!(
            (plan.estimated_size - expected).abs() / expected < 0.05,
            "estimated_size = {}, expected ~{} (AGM bound for triangle, N=100)",
            plan.estimated_size,
            expected
        );
    }

    /// `build_wcoj_plan` collects all join attributes (union, sorted).
    #[test]
    fn build_wcoj_plan_collects_all_join_attributes() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C", "D"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["C", "D"]],
        );
        let plan = build_wcoj_plan(&graph, &[100, 100, 100]);
        assert_eq!(plan.join_attributes, vec![0, 1, 2, 3], "all 4 attributes should be join attrs");
    }

    /// `build_wcoj_plan` for a single-attribute intersection returns just
    /// that one attribute.
    #[test]
    fn build_wcoj_plan_single_attribute_intersection() {
        let graph = JoinHypergraph::from_named(&["A"], &[vec!["A"], vec!["A"], vec!["A"]]);
        let plan = build_wcoj_plan(&graph, &[100, 200, 50]);
        assert_eq!(plan.join_attributes, vec![0]);
        // Order: smallest (idx 2, card 50) first.
        assert_eq!(plan.relation_order, vec![2, 0, 1]);
    }

    /// `build_wcoj_plan` handles the empty hypergraph.
    #[test]
    fn build_wcoj_plan_empty_hypergraph() {
        let graph = JoinHypergraph { attributes: vec![], relations: vec![] };
        let plan = build_wcoj_plan(&graph, &[]);
        assert!(plan.relation_order.is_empty());
        assert!(plan.join_attributes.is_empty());
        assert_eq!(plan.estimated_size, 1.0, "empty join has one tuple (the empty tuple)");
    }

    /// `build_wcoj_plan` for a single relation returns just that relation
    /// and its attributes.
    #[test]
    fn build_wcoj_plan_single_relation() {
        let graph = JoinHypergraph::from_named(&["A", "B"], &[vec!["A", "B"]]);
        let plan = build_wcoj_plan(&graph, &[42]);
        assert_eq!(plan.relation_order, vec![0]);
        assert_eq!(plan.join_attributes, vec![0, 1]);
        assert!(
            (plan.estimated_size - 42.0).abs() / 42.0 < 0.05,
            "estimated_size = {}, expected ~42 (single relation)",
            plan.estimated_size
        );
    }

    /// Tie-breaking: equal cardinalities → stable order by relation index.
    #[test]
    fn build_wcoj_plan_tie_breaks_by_relation_index() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        // All cardinalities equal → order is [0, 1, 2] (stable on index).
        let plan = build_wcoj_plan(&graph, &[100, 100, 100]);
        assert_eq!(plan.relation_order, vec![0, 1, 2]);
    }
}
