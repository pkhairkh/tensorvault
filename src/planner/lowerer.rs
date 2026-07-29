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

use std::sync::Arc;

use crate::executor::plan::{KernelInvocation, LogicalPlan, PlanNode};
use crate::kernel::{KernelTable, Operator};
use crate::memory::tier::MemoryTier;
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
}
