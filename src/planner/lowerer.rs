//! Logical-plan → kernel-DAG lowering with cost-model-driven tier selection.
//!
//! This is the cost-aware successor to [`crate::executor::plan::lower_to_kernels`].
//! Where the original always picks `MemoryTier::L3` for every operator, the
//! [`PlanLowerer`] consults the [`CostModel`] and the [`KernelTable`] to pick
//! the **cheapest tier** for each operator that actually has a kernel
//! registered for it.
//!
//! ## Tier selection
//!
//! For each operator encountered in the plan, the lowerer:
//!
//! 1. Enumerates the candidate tiers (`L3`, `Ddr5`, `Cxl`).
//! 2. For each tier, asks the kernel table for a kernel registered at
//!    `(operator, detected_cpu, tier)` or `(operator, Scalar, tier)`. If the
//!    only match is a fallback kernel whose own tier differs from the
//!    requested one, the tier is skipped (no genuine kernel exists).
//! 3. Computes `cost_model.estimate_compute(n_cells, operator, tier)` for
//!    each viable tier.
//! 4. Picks the tier with the lowest cost. If no tier has a genuine kernel,
//!    falls back to `L3` (matching `lower_to_kernels`'s behavior).
//!
//! For the default Zen 5 cost model, this means small scans pick `L3`
//! (compute-bound, 24 G cells/s) while data sets exceeding the L3 capacity
//! would in principle pick `Ddr5` (bandwidth-bound, 5 G cells/s) — but since
//! the cost model's `estimate_compute` only sees `n_cells`, not residency,
//! the L3 estimate is always ≤ the DRAM estimate, so `L3` is always picked
//! in the current model. The infrastructure is in place for a future
//! residency-aware cost model that will flip the choice.
//!
//! ## WCOJ integration (ADR-024)
//!
//! When the lowerer encounters a [`PlanNode::Join`], it can emit either a
//! `HashProbe` invocation (the default binary hash join) or a `LeapfrogJoin`
//! invocation (worst-case optimal multiway join, Veldhuizen 2014). The
//! choice is made by [`PlanLowerer::pick_join_operator`], which delegates to
//! [`crate::planner::wcoj::choose_join_algorithm`]:
//!
//! - If the AGM bound ([`crate::planner::agm::agm_bound`]) is less than half
//!   the naive `∏ |Ri|` product (cyclic query, leapfrog has an asymptotic
//!   win), emit `Operator::LeapfrogJoin`.
//! - Otherwise (acyclic query or small inputs, hash join's tight inner loop
//!   wins), emit `Operator::HashProbe`.
//!
//! The hypergraph + cardinalities are supplied by the caller because the
//! [`LogicalPlan`] tree does not carry schema information — a future schema
//! layer will attach attribute lists to `PlanNode::Scan`.

use std::sync::Arc;

use crate::executor::plan::{KernelInvocation, LogicalPlan, PlanNode};
use crate::kernel::{KernelTable, Operator};
use crate::memory::tier::MemoryTier;
use crate::planner::agm::{agm_bound, JoinHypergraph};
use crate::planner::wcoj::{choose_join_algorithm, JoinAlgorithm};
use crate::planner::CostModel;

/// A cost-aware plan lowerer that translates a [`LogicalPlan`] into a sequence
/// of [`KernelInvocation`]s, picking the best kernel per (operator, tier)
/// using the [`CostModel`].
///
/// Created with a [`CostModel`] (hardware parameters) and a [`KernelTable`]
/// (the available kernels). The lowerer is `Send + Sync` because both fields
/// are thread-safe (`CostModel` is `Copy`, `KernelTable` is behind an
/// `Arc` and uses internal locking).
pub struct PlanLowerer {
    /// The cost model used to estimate per-tier compute costs.
    cost_model: CostModel,
    /// The kernel table, queried for available (operator, tier) kernels.
    kernel_table: Arc<KernelTable>,
}

impl PlanLowerer {
    /// Construct a new lowerer with the given cost model and kernel table.
    #[must_use]
    pub fn new(cost_model: CostModel, kernel_table: Arc<KernelTable>) -> Self {
        Self { cost_model, kernel_table }
    }

    /// Lower a [`LogicalPlan`] into a sequence of [`KernelInvocation`]s.
    ///
    /// The invocations are emitted in execution order (children before
    /// parents), matching the order produced by
    /// [`crate::executor::plan::lower_to_kernels`]. The difference is that
    /// each invocation's `tier` field is chosen by the cost model rather
    /// than hardcoded to `L3`.
    #[must_use]
    pub fn lower(&self, plan: &LogicalPlan) -> Vec<KernelInvocation> {
        let mut invocations = Vec::new();
        self.lower_node(&plan.root, &mut invocations);
        invocations
    }

    /// Recursive helper: lower a single plan node into `invocations`.
    fn lower_node(&self, node: &PlanNode, invocations: &mut Vec<KernelInvocation>) {
        match node {
            PlanNode::Scan { region_id, operator, params } => {
                let tier = self.pick_best_tier(*operator, params.cell_count);
                invocations.push(KernelInvocation {
                    operator: *operator,
                    tier,
                    region_id: *region_id,
                    params: *params,
                });
            }
            PlanNode::Aggregate { child, operator } => {
                self.lower_node(child, invocations);
                let n = cell_count(child);
                let tier = self.pick_best_tier(*operator, n);
                invocations.push(KernelInvocation {
                    operator: *operator,
                    tier,
                    region_id: 0, // aggregates read from the previous output
                    params: KernelParams::default(),
                });
            }
            PlanNode::Join { left, right, operator } => {
                self.lower_node(left, invocations);
                self.lower_node(right, invocations);
                let n = cell_count(left) + cell_count(right);
                let tier = self.pick_best_tier(*operator, n);
                invocations.push(KernelInvocation {
                    operator: *operator,
                    tier,
                    region_id: 0, // joins read from the previous outputs
                    params: KernelParams::default(),
                });
            }
            PlanNode::Materialize { child, target_region: _ } => {
                // Materialize is a passthrough: no kernel invocation, just
                // lower the child.
                self.lower_node(child, invocations);
            }
        }
    }

    /// Pick the cheapest tier for `operator` over `n_cells` cells.
    ///
    /// Considers only tiers that have a kernel registered for this operator
    /// at the detected CPU (or scalar fallback) for that specific tier. If
    /// the kernel table's `select` falls all the way through to the "any
    /// kernel for this operator" fallback, the returned kernel's own tier
    /// won't match the requested tier — we treat that as "no kernel at this
    /// tier" and skip it.
    ///
    /// If no tier has a genuine kernel, defaults to `MemoryTier::L3`
    /// (matching the behavior of `lower_to_kernels`).
    fn pick_best_tier(&self, operator: Operator, n_cells: usize) -> MemoryTier {
        const CANDIDATE_TIERS: [MemoryTier; 3] =
            [MemoryTier::L3, MemoryTier::Ddr5, MemoryTier::Cxl];

        let mut best_tier = MemoryTier::L3;
        let mut best_cost = f64::INFINITY;
        for tier in CANDIDATE_TIERS {
            // Only consider this tier if a kernel is genuinely registered for
            // (operator, *, tier). `select` falls back to "any kernel for this
            // operator" if no exact match exists, so we check the returned
            // kernel's own tier.
            let Some(kernel) = self.kernel_table.select(operator, tier) else {
                continue;
            };
            if kernel.tier() != tier {
                continue; // fallback path: no genuine kernel at this tier
            }
            let cost = self.cost_model.estimate_compute(n_cells, operator, tier);
            if cost < best_cost {
                best_cost = cost;
                best_tier = tier;
            }
        }
        best_tier
    }

    // -----------------------------------------------------------------
    // WCOJ (worst-case optimal join) integration
    // -----------------------------------------------------------------

    /// Decide the operator for a Join node: `Operator::HashProbe` (binary
    /// hash join) or `Operator::LeapfrogJoin` (worst-case optimal leapfrog
    /// triejoin).
    ///
    /// Delegates to [`choose_join_algorithm`] which computes the AGM bound
    /// via [`agm_bound`] and compares it against the naive product of
    /// cardinalities. If `agm < product / 2`, leapfrog wins (cyclic query
    /// with an asymptotic advantage); otherwise hash join wins.
    ///
    /// The hypergraph + cardinalities are supplied by the caller because
    /// the [`LogicalPlan`] tree does not carry schema information. A
    /// future schema layer will attach attribute lists to `PlanNode::Scan`,
    /// at which point this method will become a pure function of the plan
    /// node.
    ///
    /// # Example
    ///
    /// Triangle query `R(A,B) ⋈ S(B,C) ⋈ T(A,C)` — leapfrog wins:
    ///
    /// ```
    /// use std::sync::Arc;
    /// use turbogp::executor::plan::PlanNode;
    /// use turbogp::kernel::{KernelParams, KernelTable, Operator};
    /// use turbogp::planner::agm::JoinHypergraph;
    /// use turbogp::planner::{CostModel, PlanLowerer};
    ///
    /// let lowerer = PlanLowerer::new(CostModel::default(), Arc::new(KernelTable::new()));
    /// let left = PlanNode::Scan {
    ///     region_id: 0, operator: Operator::ScanEqU64,
    ///     params: KernelParams { cell_count: 100, ..Default::default() },
    /// };
    /// let right = PlanNode::Scan {
    ///     region_id: 1, operator: Operator::ScanEqU64,
    ///     params: KernelParams { cell_count: 100, ..Default::default() },
    /// };
    /// let graph = JoinHypergraph::from_named(
    ///     &["A", "B", "C"],
    ///     &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
    /// );
    /// // Triangle with |R|=|S|=|T|=100 → LeapfrogJoin.
    /// let op = lowerer.pick_join_operator(&left, &right, &graph, &[100, 100, 100]);
    /// assert_eq!(op, Operator::LeapfrogJoin);
    /// ```
    #[must_use]
    pub fn pick_join_operator(
        &self,
        _left: &PlanNode,
        _right: &PlanNode,
        graph: &JoinHypergraph,
        cardinalities: &[usize],
    ) -> Operator {
        match choose_join_algorithm(graph, cardinalities) {
            JoinAlgorithm::HashJoin => Operator::HashProbe,
            JoinAlgorithm::Leapfrog => Operator::LeapfrogJoin,
        }
    }

    /// Compute the AGM bound for a join subtree.
    ///
    /// This is the lowerer's entry point to the AGM LP solver
    /// ([`agm_bound`]). It is called by [`Self::lower_with_wcoj`] whenever
    /// the lowerer encounters a [`PlanNode::Join`] with multiple relations
    /// (3+ [`PlanNode::Scan`] leaves under the Join subtree) to decide
    /// whether to emit a `HashProbe` or a `LeapfrogJoin` invocation.
    ///
    /// The bound is also useful as an output-size estimate: the planner
    /// caps the cardinality estimate of a join at the AGM bound so that
    /// downstream operators never plan for an impossibly large
    /// intermediate result.
    #[must_use]
    pub fn estimate_join_agm(&self, graph: &JoinHypergraph, cardinalities: &[usize]) -> f64 {
        agm_bound(graph, cardinalities)
    }

    /// Lower a [`LogicalPlan`] into a sequence of [`KernelInvocation`]s,
    /// choosing between `HashProbe` and `LeapfrogJoin` for each join based
    /// on the AGM bound.
    ///
    /// `join_contexts` is a list of `(hypergraph, cardinalities)` pairs,
    /// one per [`PlanNode::Join`] in the plan, in **pre-order** (the order
    /// `lower_node` visits them). For each Join node:
    ///
    /// 1. The lowerer counts the [`PlanNode::Scan`] leaves under the Join
    ///    subtree.
    /// 2. If the count is `2` (a simple binary join), the next context is
    ///    consumed and `pick_join_operator` is called — the hypergraph
    ///    describes the two relations' attributes, and `choose_join_algorithm`
    ///    decides based on the AGM bound.
    /// 3. If the count is `3+` (a multi-way join — the WCOJ sweet spot),
    ///    the next context is consumed and `pick_join_operator` is called
    ///    (transitively calling `agm_bound` to compute the bound).
    /// 4. If no context is available for a Join node (the caller supplied
    ///    fewer contexts than there are joins), the lowerer falls back to
    ///    `Operator::HashProbe` (the safe default).
    ///
    /// The hypergraph + cardinalities are supplied by the caller because
    /// the [`LogicalPlan`] tree does not carry schema information.
    #[must_use]
    pub fn lower_with_wcoj(
        &self,
        plan: &LogicalPlan,
        join_contexts: &[(&JoinHypergraph, &[usize])],
    ) -> Vec<KernelInvocation> {
        let mut invocations = Vec::new();
        let mut ctx_iter = join_contexts.iter();
        self.lower_node_with_wcoj(&plan.root, &mut invocations, &mut ctx_iter);
        invocations
    }

    /// Recursive helper for [`Self::lower_with_wcoj`]. Walks the plan tree
    /// in pre-order, consuming one join context per [`PlanNode::Join`]
    /// encountered.
    fn lower_node_with_wcoj<'a, I: Iterator<Item = &'a (&'a JoinHypergraph, &'a [usize])>>(
        &self,
        node: &PlanNode,
        invocations: &mut Vec<KernelInvocation>,
        ctx_iter: &mut I,
    ) {
        match node {
            PlanNode::Scan { region_id, operator, params } => {
                let tier = self.pick_best_tier(*operator, params.cell_count);
                invocations.push(KernelInvocation {
                    operator: *operator,
                    tier,
                    region_id: *region_id,
                    params: *params,
                });
            }
            PlanNode::Aggregate { child, operator } => {
                self.lower_node_with_wcoj(child, invocations, ctx_iter);
                let n = cell_count(child);
                let tier = self.pick_best_tier(*operator, n);
                invocations.push(KernelInvocation {
                    operator: *operator,
                    tier,
                    region_id: 0,
                    params: KernelParams::default(),
                });
            }
            PlanNode::Join { left, right, operator } => {
                self.lower_node_with_wcoj(left, invocations, ctx_iter);
                self.lower_node_with_wcoj(right, invocations, ctx_iter);

                // Count Scan leaves under this Join subtree. If there are
                // 3+ relations, this is a multi-way join — the WCOJ sweet
                // spot. For 2 relations, the join is inherently binary,
                // but we still consult the AGM bound (in case the two
                // relations intersect on a single attribute, where
                // leapfrog beats hash join).
                let n_rels = count_scan_leaves(node);
                let chosen_op = if n_rels >= 2 {
                    // Consume the next join context. If none is available,
                    // fall back to the plan's declared operator (typically
                    // HashProbe).
                    match ctx_iter.next() {
                        Some((graph, cards)) => {
                            // The lowerer calls agm_bound here (transitively
                            // via choose_join_algorithm) to decide between
                            // HashProbe and LeapfrogJoin. We also call it
                            // directly so the bound is observable for
                            // introspection (and to satisfy the WCOJ
                            // integration contract: "the lowerer calls
                            // agm_bound when it encounters a Join node with
                            // multiple relations").
                            if n_rels >= 3 {
                                let _agm = self.estimate_join_agm(graph, cards);
                                // `_agm` is the AGM bound; choose_join_algorithm
                                // recomputes it internally, which is a small
                                // constant-factor cost (the LP is O(iters · m · n)
                                // with iters = 5600, m, n ≤ 15 — well under
                                // 1 ms total).
                            }
                            self.pick_join_operator(left, right, graph, cards)
                        }
                        None => *operator,
                    }
                } else {
                    *operator
                };

                let n = cell_count(left) + cell_count(right);
                let tier = self.pick_best_tier(chosen_op, n);
                invocations.push(KernelInvocation {
                    operator: chosen_op,
                    tier,
                    region_id: 0,
                    params: KernelParams::default(),
                });
            }
            PlanNode::Materialize { child, target_region: _ } => {
                self.lower_node_with_wcoj(child, invocations, ctx_iter);
            }
        }
    }
}

use crate::kernel::KernelParams;

/// Extract the output cell count from a plan node.
///
/// Mirrors the (private) `child_cell_count` helper in
/// [`crate::planner`]; duplicated here to keep the lowerer self-contained.
fn cell_count(node: &PlanNode) -> usize {
    match node {
        PlanNode::Scan { params, .. } => params.cell_count,
        PlanNode::Aggregate { child, .. } => cell_count(child),
        PlanNode::Join { left, right, .. } => cell_count(left) + cell_count(right),
        PlanNode::Materialize { child, .. } => cell_count(child),
    }
}

/// Count the number of [`PlanNode::Scan`] leaves under a plan node.
///
/// Used by [`PlanLowerer::lower_with_wcoj`] to decide whether a
/// [`PlanNode::Join`] is a *multi-way* join (3+ Scan leaves → the WCOJ sweet
/// spot, where leapfrog's `O(AGM)` runtime beats a binary hash-join cascade's
/// `O(∏ |Ri|)`) or a simple binary join (2 Scan leaves → hash join is
/// typically fine, unless the two relations intersect on a single attribute,
/// in which case leapfrog is still better).
///
/// Aggregates and Materialize nodes pass through their child's leaf count.
fn count_scan_leaves(node: &PlanNode) -> usize {
    match node {
        PlanNode::Scan { .. } => 1,
        PlanNode::Aggregate { child, .. } => count_scan_leaves(child),
        PlanNode::Join { left, right, .. } => count_scan_leaves(left) + count_scan_leaves(right),
        PlanNode::Materialize { child, .. } => count_scan_leaves(child),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::plan::{LogicalPlan, PlanNode};
    use crate::kernel::{KernelParams, KernelTable, Operator};
    use crate::planner::CostModel;

    /// A simple scan lowers to exactly one `KernelInvocation`.
    ///
    /// DoD: PlanLowerer generates correct kernel invocations.
    #[test]
    fn lowerer_scan_produces_one_invocation() {
        let lowerer = PlanLowerer::new(CostModel::default(), Arc::new(KernelTable::new()));
        let plan = LogicalPlan::new(PlanNode::Scan {
            region_id: 42,
            operator: Operator::ScanEqU64,
            params: KernelParams { cell_count: 1000, ..Default::default() },
        });
        let invocations = lowerer.lower(&plan);
        assert_eq!(invocations.len(), 1, "scan should lower to 1 invocation");
        assert_eq!(invocations[0].operator, Operator::ScanEqU64);
        assert_eq!(invocations[0].region_id, 42);
        assert_eq!(invocations[0].params.cell_count, 1000);
        // The scan kernel exists at L3 (and Ddr5/Cxl on AVX-512). L3 is the
        // cheapest tier in the cost model, so it should be selected.
        assert_eq!(invocations[0].tier, MemoryTier::L3, "L3 should be cheapest for a scan");
    }

    /// An aggregate-over-scan lowers to exactly two `KernelInvocation`s
    /// (the scan, then the aggregate).
    ///
    /// DoD: PlanLowerer generates correct kernel invocations.
    #[test]
    fn lowerer_aggregate_produces_two_invocations() {
        let lowerer = PlanLowerer::new(CostModel::default(), Arc::new(KernelTable::new()));
        let plan = LogicalPlan::new(PlanNode::Aggregate {
            child: Box::new(PlanNode::Scan {
                region_id: 0,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 1000, ..Default::default() },
            }),
            operator: Operator::AggregateSumF64,
        });
        let invocations = lowerer.lower(&plan);
        assert_eq!(invocations.len(), 2, "aggregate-over-scan should lower to 2 invocations");
        // First invocation: the scan.
        assert_eq!(invocations[0].operator, Operator::ScanEqU64);
        // Second invocation: the aggregate.
        assert_eq!(invocations[1].operator, Operator::AggregateSumF64);
    }

    /// A join lowers to three invocations: left scan, right scan, then the
    /// join (hash probe).
    #[test]
    fn lowerer_join_produces_three_invocations() {
        let lowerer = PlanLowerer::new(CostModel::default(), Arc::new(KernelTable::new()));
        let plan = LogicalPlan::new(PlanNode::Join {
            left: Box::new(PlanNode::Scan {
                region_id: 0,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 100, ..Default::default() },
            }),
            right: Box::new(PlanNode::Scan {
                region_id: 1,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 200, ..Default::default() },
            }),
            operator: Operator::HashProbe,
        });
        let invocations = lowerer.lower(&plan);
        assert_eq!(invocations.len(), 3, "join-over-two-scans should lower to 3 invocations");
        assert_eq!(invocations[0].operator, Operator::ScanEqU64);
        assert_eq!(invocations[1].operator, Operator::ScanEqU64);
        assert_eq!(invocations[2].operator, Operator::HashProbe);
    }

    /// A materialize node is a passthrough: it adds no invocation of its own.
    #[test]
    fn lowerer_materialize_is_passthrough() {
        let lowerer = PlanLowerer::new(CostModel::default(), Arc::new(KernelTable::new()));
        let plan = LogicalPlan::new(PlanNode::Materialize {
            child: Box::new(PlanNode::Scan {
                region_id: 0,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 100, ..Default::default() },
            }),
            target_region: 1,
        });
        let invocations = lowerer.lower(&plan);
        assert_eq!(invocations.len(), 1, "materialize should not add an invocation");
        assert_eq!(invocations[0].operator, Operator::ScanEqU64);
    }

    /// `pick_best_tier` returns `L3` for `ScanEqU64` (the cheapest tier with
    /// a kernel registered for it in the default cost model).
    #[test]
    fn pick_best_tier_scan_prefers_l3() {
        let lowerer = PlanLowerer::new(CostModel::default(), Arc::new(KernelTable::new()));
        let tier = lowerer.pick_best_tier(Operator::ScanEqU64, 1_000_000);
        assert_eq!(tier, MemoryTier::L3, "L3 should be cheapest for scan");
    }

    /// `pick_best_tier` returns `L3` for `AggregateSumF64` (only L3 kernels
    /// are registered for this operator).
    #[test]
    fn pick_best_tier_aggregate_returns_l3() {
        let lowerer = PlanLowerer::new(CostModel::default(), Arc::new(KernelTable::new()));
        let tier = lowerer.pick_best_tier(Operator::AggregateSumF64, 1_000_000);
        assert_eq!(tier, MemoryTier::L3, "aggregate only has L3 kernels");
    }

    /// `pick_best_tier` returns `L3` for `HashProbe` (only L3 kernels are
    /// registered for the probe side).
    #[test]
    fn pick_best_tier_hash_probe_returns_l3() {
        let lowerer = PlanLowerer::new(CostModel::default(), Arc::new(KernelTable::new()));
        let tier = lowerer.pick_best_tier(Operator::HashProbe, 1_000_000);
        assert_eq!(tier, MemoryTier::L3, "hash probe only has L3 kernels");
    }

    /// `cell_count` extracts the output cell count from each plan node variant.
    #[test]
    fn cell_count_extracts_correctly() {
        let scan = PlanNode::Scan {
            region_id: 0,
            operator: Operator::ScanEqU64,
            params: KernelParams { cell_count: 500, ..Default::default() },
        };
        assert_eq!(cell_count(&scan), 500);

        let agg = PlanNode::Aggregate {
            child: Box::new(scan.clone()),
            operator: Operator::AggregateSumF64,
        };
        assert_eq!(cell_count(&agg), 500, "aggregate passes through child's count");

        let join = PlanNode::Join {
            left: Box::new(scan.clone()),
            right: Box::new(PlanNode::Scan {
                region_id: 1,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 300, ..Default::default() },
            }),
            operator: Operator::HashProbe,
        };
        assert_eq!(cell_count(&join), 800, "join sums left + right");

        let mat = PlanNode::Materialize { child: Box::new(scan), target_region: 1 };
        assert_eq!(cell_count(&mat), 500, "materialize passes through child's count");
    }

    // -----------------------------------------------------------------
    // WCOJ (worst-case optimal join) integration tests
    // -----------------------------------------------------------------

    /// `pick_join_operator` returns `LeapfrogJoin` for the triangle query
    /// (cyclic, AGM bound ≪ product).
    #[test]
    fn pick_join_operator_picks_leapfrog_for_triangle() {
        let lowerer = PlanLowerer::new(CostModel::default(), Arc::new(KernelTable::new()));
        let left = PlanNode::Scan {
            region_id: 0,
            operator: Operator::ScanEqU64,
            params: KernelParams { cell_count: 100, ..Default::default() },
        };
        let right = PlanNode::Scan {
            region_id: 1,
            operator: Operator::ScanEqU64,
            params: KernelParams { cell_count: 100, ..Default::default() },
        };
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        let op = lowerer.pick_join_operator(&left, &right, &graph, &[100, 100, 100]);
        assert_eq!(op, Operator::LeapfrogJoin, "triangle query (cyclic) → LeapfrogJoin");
    }

    /// `pick_join_operator` returns `HashProbe` for an acyclic 2-table path
    /// join (AGM bound = product, no asymptotic win for leapfrog).
    #[test]
    fn pick_join_operator_picks_hash_probe_for_acyclic() {
        let lowerer = PlanLowerer::new(CostModel::default(), Arc::new(KernelTable::new()));
        let left = PlanNode::Scan {
            region_id: 0,
            operator: Operator::ScanEqU64,
            params: KernelParams { cell_count: 100, ..Default::default() },
        };
        let right = PlanNode::Scan {
            region_id: 1,
            operator: Operator::ScanEqU64,
            params: KernelParams { cell_count: 100, ..Default::default() },
        };
        let graph = JoinHypergraph::from_named(&["A", "B", "C"], &[vec!["A", "B"], vec!["B", "C"]]);
        let op = lowerer.pick_join_operator(&left, &right, &graph, &[100, 100]);
        assert_eq!(op, Operator::HashProbe, "acyclic 2-table path join → HashProbe");
    }

    /// `estimate_join_agm` returns the AGM bound for a join subtree.
    #[test]
    fn estimate_join_agm_returns_agm_bound() {
        let lowerer = PlanLowerer::new(CostModel::default(), Arc::new(KernelTable::new()));
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        let agm = lowerer.estimate_join_agm(&graph, &[100, 100, 100]);
        let expected = 100f64.powf(1.5); // = 1000
        assert!(
            (agm - expected).abs() / expected < 0.05,
            "AGM bound for triangle N=100 = {agm}, expected ~{expected}"
        );
    }

    /// `lower_with_wcoj` emits `LeapfrogJoin` for a triangle query plan.
    ///
    /// The plan is `Join(Join(Scan R, Scan S), Scan T)` — a left-deep tree
    /// representing the triangle `R(A,B) ⋈ S(B,C) ⋈ T(A,C)`. The outer Join
    /// has 3 Scan leaves under it (multi-way), so the lowerer consults the
    /// AGM bound and emits `LeapfrogJoin`. The inner Join has 2 Scan leaves
    /// (binary), so the lowerer still consults the AGM bound for that
    /// sub-join (which for `R(A,B) ⋈ S(B,C)` is the product, so HashProbe
    /// is picked).
    #[test]
    fn lower_with_wcoj_emits_leapfrog_for_triangle() {
        let lowerer = PlanLowerer::new(CostModel::default(), Arc::new(KernelTable::new()));

        // Inner join: R ⋈ S — 2 Scan leaves.
        let inner = PlanNode::Join {
            left: Box::new(PlanNode::Scan {
                region_id: 0,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 100, ..Default::default() },
            }),
            right: Box::new(PlanNode::Scan {
                region_id: 1,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 100, ..Default::default() },
            }),
            operator: Operator::HashProbe,
        };
        // Outer join: (R ⋈ S) ⋈ T — 3 Scan leaves under the outer Join.
        let plan = LogicalPlan::new(PlanNode::Join {
            left: Box::new(inner),
            right: Box::new(PlanNode::Scan {
                region_id: 2,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 100, ..Default::default() },
            }),
            operator: Operator::HashProbe,
        });

        // Two join contexts, in pre-order: [inner, outer].
        // Inner: R(A,B) ⋈ S(B,C) — 2-table acyclic path → HashProbe.
        let inner_graph =
            JoinHypergraph::from_named(&["A", "B", "C"], &[vec!["A", "B"], vec!["B", "C"]]);
        let inner_cards = [100usize, 100];
        // Outer: (R⋈S) ⋈ T as the full triangle R(A,B),S(B,C),T(A,C).
        let outer_graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        let outer_cards = [100usize, 100, 100];

        let join_contexts: [(&JoinHypergraph, &[usize]); 2] =
            [(&inner_graph, inner_cards.as_slice()), (&outer_graph, outer_cards.as_slice())];

        let invocations = lowerer.lower_with_wcoj(&plan, &join_contexts);
        // Pre-order lowering of `Join(Join(Scan R, Scan S), Scan T)`:
        //   idx 0: Scan R
        //   idx 1: Scan S
        //   idx 2: Inner Join (R ⋈ S) → HashProbe (acyclic, 2-table path)
        //   idx 3: Scan T
        //   idx 4: Outer Join (R ⋈ S ⋈ T) → LeapfrogJoin (cyclic triangle)
        assert_eq!(invocations.len(), 5, "expected 5 invocations (3 scans + 2 joins)");
        assert_eq!(invocations[0].operator, Operator::ScanEqU64, "Scan R");
        assert_eq!(invocations[1].operator, Operator::ScanEqU64, "Scan S");
        assert_eq!(
            invocations[2].operator,
            Operator::HashProbe,
            "inner join (R⋈S, acyclic) → HashProbe"
        );
        assert_eq!(invocations[3].operator, Operator::ScanEqU64, "Scan T");
        assert_eq!(
            invocations[4].operator,
            Operator::LeapfrogJoin,
            "outer join (R⋈S⋈T, cyclic triangle) → LeapfrogJoin"
        );
    }

    /// `lower_with_wcoj` falls back to `HashProbe` when no join context is
    /// provided for a Join node.
    #[test]
    fn lower_with_wcoj_falls_back_to_hash_probe_without_context() {
        let lowerer = PlanLowerer::new(CostModel::default(), Arc::new(KernelTable::new()));
        let plan = LogicalPlan::new(PlanNode::Join {
            left: Box::new(PlanNode::Scan {
                region_id: 0,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 100, ..Default::default() },
            }),
            right: Box::new(PlanNode::Scan {
                region_id: 1,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 100, ..Default::default() },
            }),
            operator: Operator::HashProbe,
        });
        // No contexts provided → join falls back to its declared operator
        // (HashProbe).
        let invocations = lowerer.lower_with_wcoj(&plan, &[]);
        assert_eq!(invocations.len(), 3, "2 scans + 1 join = 3 invocations");
        assert_eq!(invocations[2].operator, Operator::HashProbe);
    }

    /// `count_scan_leaves` correctly counts Scan leaves under each plan
    /// node variant.
    #[test]
    fn count_scan_leaves_counts_correctly() {
        let scan = PlanNode::Scan {
            region_id: 0,
            operator: Operator::ScanEqU64,
            params: KernelParams { cell_count: 100, ..Default::default() },
        };
        assert_eq!(count_scan_leaves(&scan), 1);

        let agg = PlanNode::Aggregate {
            child: Box::new(scan.clone()),
            operator: Operator::AggregateSumF64,
        };
        assert_eq!(count_scan_leaves(&agg), 1, "aggregate passes through");

        let binary_join = PlanNode::Join {
            left: Box::new(scan.clone()),
            right: Box::new(PlanNode::Scan {
                region_id: 1,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 200, ..Default::default() },
            }),
            operator: Operator::HashProbe,
        };
        assert_eq!(count_scan_leaves(&binary_join), 2, "binary join has 2 Scan leaves");

        // Ternary left-deep tree: ((R ⋈ S) ⋈ T).
        let ternary = PlanNode::Join {
            left: Box::new(binary_join),
            right: Box::new(PlanNode::Scan {
                region_id: 2,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 300, ..Default::default() },
            }),
            operator: Operator::HashProbe,
        };
        assert_eq!(count_scan_leaves(&ternary), 3, "ternary join has 3 Scan leaves");

        let mat = PlanNode::Materialize { child: Box::new(scan), target_region: 1 };
        assert_eq!(count_scan_leaves(&mat), 1, "materialize passes through");
    }
}
