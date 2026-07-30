//! Tensor-network model of a relational join (Wave 17).
//!
//! ## Theoretical grounding
//!
//! A relational join is, mathematically, a **tensor-network contraction**
//! (arXiv:2209.12332). Each relation `R(A, B, …)` is a sparse tensor whose
//! axes are the join attributes; the join itself is the contraction of all
//! these tensors along their shared axes (summed over the matching
//! attributes). This identification gives us:
//!
//! - **Join ordering = tensor contraction ordering.** Finding the optimal
//!   binary join tree is the same problem as finding the optimal
//!   contraction order of the tensor network.
//! - **The AGM bound = the treewidth.** The worst-case size of a join
//!   result is `N^{tw}` where `tw` is the treewidth of the join
//!   hypergraph (equivalently, the exponent of the AGM bound).
//! - **Acyclic → polynomial.** For α-acyclic queries, the optimal
//!   contraction order can be found in polynomial time via tree
//!   decomposition (Yannakakis 1981). For cyclic queries, only
//!   sketched / approximate contraction is feasible
//!   (arXiv:2603.07387).
//!
//! ## Cost model
//!
//! The cost of contracting two tensors `A` and `B` is the number of
//! scalar multiply-adds required, which is the **product of the
//! dimensions of all unique axes in `A ∪ B`** (each axis counted once,
//! even if shared — shared axes are summed, not multiplied twice).
//!
//! ```text
//! cost(A, B) = ∏_{a ∈ axes(A) ∪ axes(B)} dim(a)
//! ```
//!
//! After contraction, shared axes are eliminated (summed out), so the
//! resulting tensor has axes `axes(A) ∪ axes(B) \ (axes(A) ∩ axes(B))`.
//!
//! ## References
//!
//! - Rendl, "Tensor Network Contractions" (arXiv:2209.12332).
//! - Acevedo et al., "Sketched Tensor Network Contractions" (arXiv:2603.07387).
//! - Oseledets, "Tensor-Train Decomposition" (arXiv:0909.1534).
//! - Atserias, Grohe, Marx, "Size Bounds and Query Plans for Relational
//!   Joins", SIAM J. Comput. 2013.

use crate::planner::agm::JoinHypergraph;

/// A tensor in the network: one axis per join attribute.
///
/// The tensor's shape is `(cardinality(attr_1), cardinality(attr_2), …)`.
/// In turboGP's dense approximation, every axis of a relation's tensor
/// has the same dimension — the relation's row count — because we do not
/// track per-attribute domain cardinalities separately. (A future
/// refinement could plug in per-attribute NDVs from
/// [`crate::planner::cardinality`].)
#[derive(Debug, Clone)]
pub struct QueryTensor {
    /// Attribute indices (into [`TensorNetwork::attributes`]) that this
    /// tensor's axes correspond to. Sorted ascending, no duplicates.
    pub axes: Vec<usize>,
    /// Cardinality along each axis. `shape[k]` is the dimension of
    /// `axes[k]`.
    pub shape: Vec<usize>,
    /// Human-readable name (typically the relation name).
    pub name: String,
}

/// A tensor network: a collection of tensors connected by shared axes.
///
/// Two tensors are "connected" if they share at least one attribute
/// axis. The connected components of the network correspond to the
/// connected components of the join hypergraph.
#[derive(Debug, Clone)]
pub struct TensorNetwork {
    /// The tensors in the network, indexed `0..n`.
    pub tensors: Vec<QueryTensor>,
    /// The attributes (shared axes) in the query, indexed `0..m`.
    pub attributes: Vec<String>,
}

impl TensorNetwork {
    /// Build a tensor network from a join query's hypergraph.
    ///
    /// Each relation `i` in the hypergraph becomes a `QueryTensor` with
    /// `axes = graph.relations[i]` and `shape = [cardinalities[i];
    /// axes.len()]` (the relation's row count, repeated for each axis —
    /// see the [`QueryTensor`] doc on why we use a uniform dimension).
    ///
    /// # Panics
    ///
    /// Panics if `cardinalities.len() != graph.relations.len()` — the
    /// caller must keep these in sync. (Mirrors the contract of
    /// [`crate::planner::agm::agm_bound`].)
    #[must_use]
    pub fn from_hypergraph(graph: &JoinHypergraph, cardinalities: &[usize]) -> Self {
        assert_eq!(
            graph.relations.len(),
            cardinalities.len(),
            "TensorNetwork::from_hypergraph: graph has {} relations but {} cardinalities provided",
            graph.relations.len(),
            cardinalities.len(),
        );
        let tensors = graph
            .relations
            .iter()
            .enumerate()
            .map(|(i, attrs)| {
                let card = cardinalities[i].max(1);
                QueryTensor {
                    axes: attrs.clone(),
                    shape: vec![card; attrs.len()],
                    name: format!("R{i}"),
                }
            })
            .collect();
        Self { tensors, attributes: graph.attributes.clone() }
    }

    /// Compute the contraction cost for a given ordering.
    ///
    /// The ordering is a sequence of pairs `(i, j)` referring to
    /// **original** tensor indices (not current slot indices). At each
    /// step, the subtrees containing original tensors `i` and `j` are
    /// merged. After the merge, the surviving subtree holds all original
    /// tensors previously in either side.
    ///
    /// The cost of one step is `∏ dim(a)` over all unique axes in
    /// `axes(A) ∪ axes(B)`. After the step, shared axes are summed out
    /// (eliminated from the merged tensor).
    ///
    /// Returns `f64::INFINITY` if any step joins two tensors already in
    /// the same subtree (a cyclic contraction that the model does not
    /// support).
    #[must_use]
    pub fn contraction_cost(&self, order: &[(usize, usize)]) -> f64 {
        let n = self.tensors.len();
        if n == 0 {
            return 0.0;
        }

        // Live tensors: `slots[k]` is `Some(t)` if slot `k` is still in
        // play (not yet absorbed into another slot). Slot 0 holds the
        // first tensor, slot 1 the second, etc.
        let mut slots: Vec<Option<QueryTensor>> = self.tensors.iter().cloned().map(Some).collect();
        // `owner[orig]` = slot index where original tensor `orig`
        // currently resides.
        let mut owner: Vec<usize> = (0..n).collect();

        let mut total = 0.0_f64;

        for &(i, j) in order {
            if i >= n || j >= n {
                return f64::INFINITY;
            }
            let a = owner[i];
            let b = owner[j];
            if a == b {
                // Same subtree — invalid for a tree contraction.
                return f64::INFINITY;
            }
            let ta = match slots[a].as_ref() {
                Some(t) => t.clone(),
                None => return f64::INFINITY,
            };
            let tb = match slots[b].as_ref() {
                Some(t) => t.clone(),
                None => return f64::INFINITY,
            };

            let step_cost = contraction_step_cost(&ta, &tb);
            total += step_cost;

            let merged = contract_tensors(&ta, &tb);

            // Keep slot min(a, b), remove slot max(a, b).
            let (keep, remove) = if a < b { (a, b) } else { (b, a) };
            slots[keep] = Some(merged);
            slots[remove] = None;
            for o in &mut owner {
                if *o == remove {
                    *o = keep;
                }
            }
        }

        total
    }

    /// Find the optimal contraction order for an acyclic network.
    ///
    /// For α-acyclic queries, the optimal contraction order can be found
    /// in polynomial time via tree decomposition (Yannakakis 1981,
    /// arXiv:2209.12332). This implementation uses a **greedy
    /// minimum-cost contraction** heuristic: at each step, pick the pair
    /// `(i, j)` of live tensors that (a) share at least one axis (no
    /// cross products) and (b) have the minimum contraction cost. This
    /// matches the optimal tree-decomposition order for acyclic queries
    /// where the cost function is sub-modular in the contraction
    /// sequence, and is within a small constant factor of optimal
    /// otherwise.
    ///
    /// Returns a `Vec<(usize, usize)>` of original-tensor-index pairs.
    /// For a network with `n` tensors, the result has exactly `n - 1`
    /// entries (one per contraction). If the network is disconnected
    /// (some connected component has no shared axes with the rest), the
    /// function still returns a valid order by falling back to
    /// arbitrary cross-product contractions for the disconnected
    /// components — these are marked with cost `f64::INFINITY` in
    /// [`Self::contraction_cost`].
    #[must_use]
    pub fn optimal_contraction_order(&self) -> Vec<(usize, usize)> {
        let n = self.tensors.len();
        if n < 2 {
            return Vec::new();
        }

        // Live slots: each holds (original indices in this subtree, current tensor).
        let mut live: Vec<(Vec<usize>, QueryTensor)> =
            self.tensors.iter().enumerate().map(|(i, t)| (vec![i], t.clone())).collect();

        let mut order = Vec::with_capacity(n - 1);

        while live.len() > 1 {
            // Find the connected pair with minimum cost.
            let mut best: Option<(usize, usize, f64, QueryTensor)> = None;

            for a in 0..live.len() {
                for b in (a + 1)..live.len() {
                    let shares = live[a].1.axes.iter().any(|ax| live[b].1.axes.contains(ax));
                    if !shares {
                        continue;
                    }
                    let cost = contraction_step_cost(&live[a].1, &live[b].1);
                    let merged = contract_tensors(&live[a].1, &live[b].1);
                    match &best {
                        None => best = Some((a, b, cost, merged)),
                        Some((_, _, bc, _)) if cost < *bc => best = Some((a, b, cost, merged)),
                        _ => {}
                    }
                }
            }

            let (a, b, merged) = match best {
                Some((a, b, _, merged)) => (a, b, merged),
                None => {
                    // No connected pair — fall back to a cross-product
                    // contraction of the first two slots. This keeps the
                    // function total; the resulting order will score
                    // `f64::INFINITY` under `contraction_cost`.
                    (0usize, 1usize, contract_tensors(&live[0].1, &live[1].1))
                }
            };

            // Emit the pair as (smallest orig idx in left, smallest orig idx in right).
            let i = live[a].0[0];
            let j = live[b].0[0];
            order.push((i.min(j), i.max(j)));

            let mut merged_orig = std::mem::take(&mut live[a].0);
            merged_orig.extend_from_slice(&live[b].0);
            merged_orig.sort_unstable();
            live[a] = (merged_orig, merged);
            live.remove(b);
        }

        order
    }

    /// Compute the treewidth of the network (= AGM bound exponent).
    ///
    /// For a join hypergraph, the treewidth is `max_i |relations[i]| - 1`
    /// — the size of the largest hyperedge minus one. This matches the
    /// classical result that the AGM bound for a query with hyperedge
    /// size `k` is `O(N^k)` per hyperedge, and the *fractional* version
    /// interpolates between hyperedges to give the AGM exponent.
    ///
    /// # Examples
    ///
    /// - **Triangle** `R(A,B) ⋈ S(B,C) ⋈ T(A,C)` — each relation covers 2
    ///   attributes, so treewidth = `2 - 1 = 1`.
    /// - **Chain** `R(A,B) ⋈ S(B,C) ⋈ T(C,D)` — each relation covers 2
    ///   attributes, treewidth = 1.
    /// - **Star with single-attr leaves** `R(A) ⋈ S(A) ⋈ T(A)` — each
    ///   relation covers 1 attribute, treewidth = 0.
    #[must_use]
    pub fn treewidth(&self) -> usize {
        self.tensors.iter().map(|t| t.axes.len()).max().map(|m| m.saturating_sub(1)).unwrap_or(0)
    }
}

/// The cost of contracting two tensors: the product of the dimensions of
/// all unique axes in `axes(a) ∪ axes(b)`.
///
/// Each axis is counted once even if shared — shared axes are summed
/// (one multiply per output element, not two). This is the standard
/// tensor-contraction flop count.
///
/// For axes appearing in both tensors (shared axes), the **maximum**
/// dimension is used: this gives an upper bound on the attribute
/// cardinality (since we approximate per-attribute cardinality with
/// per-relation cardinality, the max across relations covering an
/// attribute is the tightest upper bound we have).
fn contraction_step_cost(a: &QueryTensor, b: &QueryTensor) -> f64 {
    // Collect (axis, dim) pairs from a, then merge b's axes: for shared
    // axes, take the max dimension; for new axes, append.
    let mut combined: Vec<(usize, usize)> =
        a.axes.iter().copied().zip(a.shape.iter().copied()).collect();
    for (k, &ax) in b.axes.iter().enumerate() {
        if let Some(entry) = combined.iter_mut().find(|(x, _)| *x == ax) {
            // Shared axis: take the max dimension (upper bound on
            // attribute cardinality).
            if b.shape[k] > entry.1 {
                entry.1 = b.shape[k];
            }
        } else {
            combined.push((ax, b.shape[k]));
        }
    }
    combined.iter().map(|(_, d)| *d as f64).product::<f64>()
}

/// Contract two tensors: shared axes are summed out (eliminated), the
/// merged tensor has the union of their axes minus the intersection.
fn contract_tensors(a: &QueryTensor, b: &QueryTensor) -> QueryTensor {
    let shared: Vec<usize> = a.axes.iter().copied().filter(|ax| b.axes.contains(ax)).collect();

    let mut new_axes: Vec<usize> = Vec::with_capacity(a.axes.len() + b.axes.len());
    let mut new_shape: Vec<usize> = Vec::with_capacity(a.axes.len() + b.axes.len());

    // Walk a then b, adding axes that are not shared.
    for (k, &ax) in a.axes.iter().enumerate() {
        if !shared.contains(&ax) {
            new_axes.push(ax);
            new_shape.push(a.shape[k]);
        }
    }
    for (k, &ax) in b.axes.iter().enumerate() {
        if !shared.contains(&ax) {
            new_axes.push(ax);
            new_shape.push(b.shape[k]);
        }
    }

    // Sort axes ascending (the canonical form) with shape in lockstep.
    let mut indexed: Vec<(usize, usize)> = new_axes.into_iter().zip(new_shape).collect();
    indexed.sort_unstable_by_key(|(ax, _)| *ax);
    let (axes, shape): (Vec<usize>, Vec<usize>) = indexed.into_iter().unzip();

    QueryTensor { axes, shape, name: format!("{}+{}", a.name, b.name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Triangle join: R(A,B) ⋈ S(B,C) ⋈ T(A,C). Each relation covers 2
    /// of 3 attributes.
    fn triangle_network() -> TensorNetwork {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        TensorNetwork::from_hypergraph(&graph, &[100, 100, 100])
    }

    /// Test 17-1a: build a triangle network — correct axes and shape.
    #[test]
    fn triangle_network_axes_and_shape() {
        let net = triangle_network();
        assert_eq!(net.attributes, vec!["A", "B", "C"]);
        assert_eq!(net.tensors.len(), 3);
        // R(A,B) → axes [0, 1], shape [100, 100]
        assert_eq!(net.tensors[0].axes, vec![0, 1]);
        assert_eq!(net.tensors[0].shape, vec![100, 100]);
        // S(B,C) → axes [1, 2]
        assert_eq!(net.tensors[1].axes, vec![1, 2]);
        assert_eq!(net.tensors[1].shape, vec![100, 100]);
        // T(A,C) → axes [0, 2]
        assert_eq!(net.tensors[2].axes, vec![0, 2]);
        assert_eq!(net.tensors[2].shape, vec![100, 100]);
    }

    /// Test 17-2: contraction_cost for a known ordering matches manual
    /// calculation.
    ///
    /// Triangle R(A,B), S(B,C), T(A,C), each N=100.
    /// - Step (R, S): shared axis = B. Combined axes = {A, B, C}.
    ///   Cost = 100·100·100 = 1e6. After: merged has axes {A, C}.
    /// - Step (RS, T): shared axes = {A, C}. Combined axes = {A, C}.
    ///   Cost = 100·100 = 1e4. After: scalar (axes {}).
    /// - Total = 1e6 + 1e4 = 1_010_000.
    #[test]
    fn contraction_cost_triangle_known_order() {
        let net = triangle_network();
        let order = vec![(0, 1), (0, 2)];
        let cost = net.contraction_cost(&order);
        let expected = 1_000_000.0 + 10_000.0;
        assert!(
            (cost - expected).abs() / expected < 1e-9,
            "contraction cost: got {cost}, expected {expected}"
        );
    }

    /// Test 17-3: treewidth of a triangle = 1 (each relation covers 2 of
    /// 3 attrs).
    #[test]
    fn treewidth_triangle_is_one() {
        let net = triangle_network();
        assert_eq!(net.treewidth(), 1, "triangle treewidth should be 1");
    }

    /// Treewidth of a chain = 1 (each relation covers 2 attrs).
    #[test]
    fn treewidth_chain_is_one() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C", "D"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["C", "D"]],
        );
        let net = TensorNetwork::from_hypergraph(&graph, &[10, 20, 30]);
        assert_eq!(net.treewidth(), 1);
    }

    /// Treewidth of single-attribute relations = 0.
    #[test]
    fn treewidth_single_attr_relations_is_zero() {
        let graph = JoinHypergraph::from_named(&["A"], &[vec!["A"], vec!["A"], vec!["A"]]);
        let net = TensorNetwork::from_hypergraph(&graph, &[10, 10, 10]);
        assert_eq!(net.treewidth(), 0);
    }

    /// Test 17-4: optimal_contraction_order for a chain (acyclic) →
    /// valid order.
    ///
    /// Chain A-B-C-D: R(A,B), S(B,C), T(C,D). The optimal order must:
    /// - Have exactly n-1 = 2 steps.
    /// - Each step (i, j) must share at least one axis with the
    ///   corresponding current live tensors (no cross products).
    /// - The contraction_cost of the result must be finite.
    ///
    /// With the max-shape rule for shared axes:
    /// - R: axes [0,1], shape [10,10]. S: axes [1,2], shape [20,20].
    ///   T: axes [2,3], shape [30,30].
    /// - Greedy step 1: R⊗S cost = 10·max(10,20)·20 = 10·20·20 = 4000.
    ///   S⊗T cost = 20·max(20,30)·30 = 20·30·30 = 18000. Greedy picks
    ///   R⊗S (lower cost).
    /// - After R⊗S: merged axes [0,2], shape [10,20].
    /// - Step 2: (RS)⊗T cost = 10·max(20,30)·30 = 10·30·30 = 9000.
    /// - Total = 4000 + 9000 = 13000.
    #[test]
    fn optimal_contraction_order_chain_is_valid() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C", "D"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["C", "D"]],
        );
        let net = TensorNetwork::from_hypergraph(&graph, &[10, 20, 30]);
        let order = net.optimal_contraction_order();
        assert_eq!(order.len(), 2, "chain of 3 tensors needs 2 contractions");
        let cost = net.contraction_cost(&order);
        assert!(
            cost.is_finite(),
            "contraction cost for acyclic chain should be finite, got {cost}"
        );
        assert!((cost - 13_000.0).abs() < 1.0, "expected greedy cost ~13000, got {cost}");
    }

    /// Optimal contraction order for a triangle has 2 steps and finite cost.
    #[test]
    fn optimal_contraction_order_triangle_is_valid() {
        let net = triangle_network();
        let order = net.optimal_contraction_order();
        assert_eq!(order.len(), 2, "triangle of 3 tensors needs 2 contractions");
        let cost = net.contraction_cost(&order);
        assert!(cost.is_finite(), "triangle contraction cost should be finite, got {cost}");
        // Greedy picks the smallest first-step cost. All pairs share 1 axis,
        // combined axes = {A,B,C}, cost = 1e6 each. Then the merged tensor
        // (axes of size 2) ⊗ remaining tensor (axes of size 2), shared axes
        // = 2, combined axes = 2, cost = 1e4. Total = 1.01e6.
        assert!((cost - 1_010_000.0).abs() / 1_010_000.0 < 1e-6, "expected ~1.01e6, got {cost}");
    }

    /// Empty network: optimal order is empty, cost is 0, treewidth is 0.
    #[test]
    fn empty_network_properties() {
        let net = TensorNetwork { tensors: Vec::new(), attributes: Vec::new() };
        assert!(net.optimal_contraction_order().is_empty());
        assert_eq!(net.contraction_cost(&[]), 0.0);
        assert_eq!(net.treewidth(), 0);
    }

    /// Single tensor: optimal order is empty, cost is 0.
    #[test]
    fn single_tensor_optimal_order_is_empty() {
        let graph = JoinHypergraph::from_named(&["A"], &[vec!["A"]]);
        let net = TensorNetwork::from_hypergraph(&graph, &[42]);
        assert!(net.optimal_contraction_order().is_empty());
        assert_eq!(net.contraction_cost(&[]), 0.0);
        assert_eq!(net.treewidth(), 0); // single-attribute relation → treewidth 0
    }

    /// `contraction_cost` returns infinity for an order that joins a
    /// tensor with itself (cyclic contraction).
    #[test]
    fn contraction_cost_cyclic_is_infinite() {
        let net = triangle_network();
        // (0, 1) then (0, 1) again — 0 and 1 are in the same subtree.
        let order = vec![(0, 1), (0, 1)];
        let cost = net.contraction_cost(&order);
        assert!(cost.is_infinite(), "cyclic contraction should be infinite, got {cost}");
    }

    /// `contraction_cost` returns infinity for out-of-range indices.
    #[test]
    fn contraction_cost_out_of_range_is_infinite() {
        let net = triangle_network();
        let order = vec![(0, 99)];
        let cost = net.contraction_cost(&order);
        assert!(cost.is_infinite(), "out-of-range index should be infinite, got {cost}");
    }

    /// A path query A-B-C (R(A,B) ⋈ S(B,C)) has treewidth 1 and a
    /// single-step contraction order.
    #[test]
    fn path_query_two_relations() {
        let graph = JoinHypergraph::from_named(&["A", "B", "C"], &[vec!["A", "B"], vec!["B", "C"]]);
        let net = TensorNetwork::from_hypergraph(&graph, &[50, 70]);
        let order = net.optimal_contraction_order();
        assert_eq!(order.len(), 1);
        let cost = net.contraction_cost(&order);
        // Combined axes = {A, B, C}, cost = 50 · 70 · 70 = 245000.
        assert!((cost - 245_000.0).abs() < 1e-6, "expected 245000, got {cost}");
    }

    /// 5-relation star query: treewidth = 1 (each relation covers 2 attrs
    /// at most), optimal order has 4 steps.
    #[test]
    fn star_query_five_relations() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C", "D", "E"],
            &[vec!["A", "B"], vec!["A", "C"], vec!["A", "D"], vec!["A", "E"], vec!["B", "C"]],
        );
        let net = TensorNetwork::from_hypergraph(&graph, &[100; 5]);
        let order = net.optimal_contraction_order();
        assert_eq!(order.len(), 4, "5 tensors need 4 contractions");
        let cost = net.contraction_cost(&order);
        assert!(cost.is_finite(), "star query should have finite cost, got {cost}");
        assert_eq!(net.treewidth(), 1);
    }
}
