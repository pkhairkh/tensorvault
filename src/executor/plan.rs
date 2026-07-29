//! Logical plans and kernel invocations.

use crate::kernel::{KernelParams, KernelResult, Operator};
use crate::memory::region::RegionId;
use crate::memory::tier::MemoryTier;

/// A node in a logical plan.
#[derive(Debug, Clone)]
pub enum PlanNode {
    /// Scan a region with an operator.
    Scan {
        /// The region to scan.
        region_id: RegionId,
        /// The operator to apply.
        operator: Operator,
        /// Kernel parameters.
        params: KernelParams,
    },
    /// Aggregate the output of a child node.
    Aggregate {
        /// Child node.
        child: Box<PlanNode>,
        /// Aggregation operator.
        operator: Operator,
    },
    /// Join two child nodes.
    Join {
        /// Left child.
        left: Box<PlanNode>,
        /// Right child.
        right: Box<PlanNode>,
        /// Join operator (hash_build then hash_probe).
        operator: Operator,
    },
    /// Materialize the output to a region.
    Materialize {
        /// Child node.
        child: Box<PlanNode>,
        /// Target region.
        target_region: RegionId,
    },
}

/// A logical plan.
#[derive(Debug, Clone)]
pub struct LogicalPlan {
    /// The root node.
    pub root: PlanNode,
}

impl LogicalPlan {
    /// Create a new plan from a root node.
    pub fn new(root: PlanNode) -> Self {
        Self { root }
    }
}

/// A concrete kernel invocation: the operator, the tier, and the input region.
#[derive(Debug, Clone)]
pub struct KernelInvocation {
    /// The operator.
    pub operator: Operator,
    /// The memory tier (determines which kernel to select).
    pub tier: MemoryTier,
    /// The region to read from.
    pub region_id: RegionId,
    /// Parameters for the kernel.
    pub params: KernelParams,
}

/// The result of executing a plan.
#[derive(Debug, Clone, Default)]
pub struct PlanResult {
    /// Aggregated kernel results.
    pub kernel_results: Vec<KernelResult>,
}

/// Lower a logical plan to a list of kernel invocations.
pub fn lower_to_kernels(plan: &LogicalPlan) -> Vec<KernelInvocation> {
    let mut invocations = Vec::new();
    lower_node(&plan.root, &mut invocations);
    invocations
}

fn lower_node(node: &PlanNode, invocations: &mut Vec<KernelInvocation>) {
    match node {
        PlanNode::Scan { region_id, operator, params } => {
            invocations.push(KernelInvocation {
                operator: *operator,
                tier: MemoryTier::L3, // Default; the scheduler will refine.
                region_id: *region_id,
                params: *params,
            });
        }
        PlanNode::Aggregate { child, operator } => {
            lower_node(child, invocations);
            invocations.push(KernelInvocation {
                operator: *operator,
                tier: MemoryTier::L3,
                region_id: 0, // Aggregates read from the previous output.
                params: KernelParams::default(),
            });
        }
        PlanNode::Join { left, right, operator } => {
            lower_node(left, invocations);
            lower_node(right, invocations);
            invocations.push(KernelInvocation {
                operator: *operator,
                tier: MemoryTier::L3,
                region_id: 0,
                params: KernelParams::default(),
            });
        }
        PlanNode::Materialize { child, target_region: _ } => {
            lower_node(child, invocations);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_scan_lowers_to_one_invocation() {
        let plan = LogicalPlan::new(PlanNode::Scan {
            region_id: 42,
            operator: Operator::ScanEqU64,
            params: KernelParams { target_u64: 99, cell_count: 504, ..Default::default() },
        });
        let invocations = lower_to_kernels(&plan);
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].operator, Operator::ScanEqU64);
        assert_eq!(invocations[0].region_id, 42);
    }

    #[test]
    fn plan_aggregate_lowers_to_two_invocations() {
        let plan = LogicalPlan::new(PlanNode::Aggregate {
            child: Box::new(PlanNode::Scan {
                region_id: 0,
                operator: Operator::ScanEqU64,
                params: KernelParams::default(),
            }),
            operator: Operator::AggregateSumF64,
        });
        let invocations = lower_to_kernels(&plan);
        assert_eq!(invocations.len(), 2);
    }
}
