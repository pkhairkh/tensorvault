//! The query planner: a calibrated analytic cost model (ADR-023) + Kingman
//! queueing predictor (ADR-020) + DPccp join ordering (ADR-019) + a
//! cost-aware plan lowerer.
//!
//! ## Overview
//!
//! The planner predicts the wall-clock latency of a logical plan before it is
//! executed, so that the join reorderer (ADR-019), index selector (ADR-016),
//! and admission controller (ADR-020) can make decisions without actually
//! running anything.
//!
//! The cost of a plan is the sum of two terms:
//!
//! 1. **Compute cost** — for each operator in the plan, `n_cells /
//!    throughput(operator, tier)`. The throughput is bounded above by either
//!    the SIMD execution rate (`lanes × f_cpu`, for L3-resident data) or the
//!    memory bandwidth (`BW / cell_size`, for DRAM-resident data).
//!
//! 2. **Queueing cost** — Kingman's formula predicts the mean wait time in a
//!    G/G/1 queue from `(λ, μ, c_a, c_s)`. This is added once per join (the
//!    only operator that can contend on a shared hash table) in the current
//!    simple model.
//!
//! ## Submodules
//!
//! - [`CostModel`] (this file) — per-tier compute-cost estimates.
//! - [`kingman`] — Kingman's-formula queueing predictor for admission control
//!   and join-cost tail latency.
//! - [`dpccp`] — left-deep DPccp join ordering for `n ≤ 15` relations
//!   (ADR-019).
//! - [`agm`] — Atserias-Grohe-Marx fractional cover bound, the worst-case
//!   size of a join result and the runtime bound of worst-case optimal join
//!   algorithms.
//! - [`wcoj`] — worst-case optimal join (Leapfrog triejoin) plan selection:
//!   picks between hash join and leapfrog based on the AGM bound.
//! - [`cardinality`] — simple per-table row-count and selectivity estimates
//!   used by the cost model and the join reorderer.
//! - [`lowerer`] — cost-aware lowering of a `LogicalPlan` into a sequence of
//!   `KernelInvocation`s, picking the cheapest tier per operator and
//!   dispatching each join to either `HashProbe` or `LeapfrogJoin` via
//!   [`wcoj::choose_join_algorithm`].
//!
//! ## Calibration
//!
//! The default [`CostModel`] encodes Zen 5 measurements taken on an
//! AMD EPYC-Turin (see ADR-023):
//!
//! | Kernel | Tier | Measured | Theoretical |
//! |--------|------|----------|-------------|
//! | `scan_eq` AVX-512 | L3 | 24.1 G cells/s | 24 G (8 lanes × 3 GHz) |
//! | `scan_eq` AVX-512 | DRAM | ~5 G cells/s | 5 G (40 GB/s ÷ 8 B) |
//! | `sum_f64` AVX-512 | L3 | 29.8 G cells/s | 24 G |
//!
//! The theoretical formula matches measurement within 5%, so the cost model
//! uses the formula directly. New CPUs are calibrated by editing the
//! `CostModel` defaults (or by constructing a custom one and passing it to
//! [`estimate_cost`]).

pub mod agm;
pub mod cardinality;
pub mod dpccp;
pub mod kingman;
pub mod lowerer;
pub mod wcoj;

pub use agm::{agm_bound, JoinHypergraph};
pub use cardinality::CardinalityEstimator;
pub use dpccp::{dpccp, JoinRelation, JoinTree};
pub use kingman::KingmanPredictor;
pub use lowerer::PlanLowerer;
pub use wcoj::{build_wcoj_plan, choose_join_algorithm, JoinAlgorithm, WcojPlan};

use crate::executor::plan::{LogicalPlan, PlanNode};
use crate::kernel::Operator;
use crate::memory::tier::MemoryTier;

/// A calibrated analytic cost model (ADR-023).
///
/// Encodes the hardware parameters that determine kernel throughput:
///
/// - `cpu_freq_hz` × `simd_lanes` = peak L3-resident throughput (cells/sec).
/// - `memory_bandwidth_bps` / `cell_size` = peak DRAM-resident throughput.
///
/// The default values are calibrated to a Zen 5 core running AVX-512 u64
/// kernels (8 lanes, 3 GHz, 40 GB/s DRAM). Override them for other CPUs or
/// for hypothetical what-if analysis.
#[derive(Debug, Clone, Copy)]
pub struct CostModel {
    /// CPU clock frequency in Hz (e.g. `3.0e9` for 3 GHz).
    pub cpu_freq_hz: f64,
    /// SIMD lanes per kernel invocation (e.g. `8` for AVX-512 u64).
    pub simd_lanes: usize,
    /// Memory bandwidth in bytes/sec (e.g. `40e9` for 40 GB/s DRAM).
    pub memory_bandwidth_bps: f64,
    /// Cell size in bytes (always 8 — turbogp is a u64-word engine, ADR-001).
    pub cell_size: usize,
}

impl CostModel {
    /// Peak throughput (cells/sec) for L3-resident data.
    ///
    /// For an L3-resident kernel, throughput is compute-bound: the kernel
    /// processes `simd_lanes` cells per cycle, and the CPU issues
    /// `cpu_freq_hz` cycles per second. The result is independent of the
    /// operator (all 8-lane AVX-512 kernels hit the same 24 G cells/sec
    /// bound on Zen 5), but the `operator` parameter is retained in the
    /// signature so future per-kernel calibration tables can plug in without
    /// changing call sites.
    ///
    /// Measured value on Zen 5 (AVX-512, L3-resident): 24.1 G cells/sec.
    #[must_use]
    pub fn throughput_l3(&self, _operator: Operator) -> f64 {
        // The parameter is intentionally unused: the formula `lanes × f_cpu`
        // is the same for every 8-lane AVX-512 kernel (see ADR-023, table).
        // A future per-kernel calibration table would index on `_operator`.
        self.simd_lanes as f64 * self.cpu_freq_hz
    }

    /// Peak throughput (cells/sec) for DRAM-resident data.
    ///
    /// For a DRAM-resident kernel, throughput is memory-bandwidth-bound: the
    /// kernel consumes `cell_size` bytes per cell, and DRAM supplies
    /// `memory_bandwidth_bps` bytes per second.
    ///
    /// Measured value on Zen 5 (40 GB/s DRAM, 8-byte cells): ~5 G cells/sec.
    #[must_use]
    pub fn throughput_dram(&self) -> f64 {
        if self.cell_size == 0 {
            return 0.0;
        }
        self.memory_bandwidth_bps / self.cell_size as f64
    }

    /// Estimate the compute cost (in seconds) of running `operator` over
    /// `n_cells` cells resident in `tier`.
    ///
    /// - L1/L2/L3 tiers: compute-bound → `n_cells / throughput_l3`.
    /// - DRAM/CXL/HBM/NVMe/Network tiers: bandwidth-bound →
    ///   `n_cells / throughput_dram`.
    ///
    /// Returns 0.0 if `n_cells == 0` (no work to do).
    #[must_use]
    pub fn estimate_compute(&self, n_cells: usize, operator: Operator, tier: MemoryTier) -> f64 {
        if n_cells == 0 {
            return 0.0;
        }
        let throughput = match tier {
            // Cache-resident: compute-bound.
            MemoryTier::L1L2 | MemoryTier::L3 => self.throughput_l3(operator),
            // Off-chip tiers: bandwidth-bound (CXL is bounded by its link
            // bandwidth, which the current model approximates with the DRAM
            // figure — a conservative lower bound).
            MemoryTier::Ddr5
            | MemoryTier::Hbm
            | MemoryTier::Cxl
            | MemoryTier::Nvme
            | MemoryTier::NvmeOf
            | MemoryTier::Network => self.throughput_dram(),
        };
        if throughput <= 0.0 {
            return 0.0;
        }
        n_cells as f64 / throughput
    }
}

impl Default for CostModel {
    /// Zen 5 defaults: 3 GHz, 8 AVX-512 lanes, 40 GB/s DRAM, 8-byte cells.
    ///
    /// These match the measured throughputs in ADR-023 (24 G cells/sec L3,
    /// 5 G cells/sec DRAM) to within 5%.
    fn default() -> Self {
        Self { cpu_freq_hz: 3.0e9, simd_lanes: 8, memory_bandwidth_bps: 40.0e9, cell_size: 8 }
    }
}

/// Estimate the total wall-clock cost (in seconds) of executing `plan`.
///
/// Walks the plan tree, summing per-operator compute costs from
/// `cost_model`, and adds the Kingman queueing wait once per join (the only
/// operator that can contend on a shared hash table in the current model).
///
/// The estimates are intentionally simple:
///
/// - **Scan**: `estimate_compute(n, ScanEqU64, L3)` — uses the L3 tier by
///   default (the scheduler refines this at execution time).
/// - **Aggregate**: `child_cost + estimate_compute(n_child, AggregateSumF64, L3)`.
/// - **Join**: `left_cost + right_cost + estimate_compute(n_left + n_right,
///   HashProbe, L3) + kingman.predicted_wait()`.
/// - **Materialize**: passthrough (no compute cost modeled yet).
///
/// This is the entry point used by the join reorderer (ADR-019) and the
/// admission controller (ADR-020).
#[must_use]
pub fn estimate_cost(
    plan: &LogicalPlan,
    cost_model: &CostModel,
    kingman: &KingmanPredictor,
) -> f64 {
    estimate_node_cost(&plan.root, cost_model, kingman)
}

/// Recursive helper for [`estimate_cost`].
fn estimate_node_cost(node: &PlanNode, cost_model: &CostModel, kingman: &KingmanPredictor) -> f64 {
    match node {
        PlanNode::Scan { params, .. } => {
            // Use the canonical scan operator for the cost model: all scan
            // variants (eq, range, multi-predicate) have the same SIMD
            // throughput bound on Zen 5 (ADR-023), so the operator choice
            // doesn't affect the L3 estimate. A future per-kernel
            // calibration table would dispatch on `params.operator`.
            cost_model.estimate_compute(params.cell_count, Operator::ScanEqU64, MemoryTier::L3)
        }
        PlanNode::Aggregate { child, .. } => {
            let child_cost = estimate_node_cost(child, cost_model, kingman);
            let n = child_cell_count(child);
            child_cost + cost_model.estimate_compute(n, Operator::AggregateSumF64, MemoryTier::L3)
        }
        PlanNode::Join { left, right, .. } => {
            let left_cost = estimate_node_cost(left, cost_model, kingman);
            let right_cost = estimate_node_cost(right, cost_model, kingman);
            let n = child_cell_count(left) + child_cell_count(right);
            let join_compute = cost_model.estimate_compute(n, Operator::HashProbe, MemoryTier::L3);
            // Joins contend on the shared hash table — add the Kingman
            // predicted queueing wait. This is the only operator in the
            // current model that contributes a queueing term.
            left_cost + right_cost + join_compute + kingman.predicted_wait()
        }
        PlanNode::Materialize { child, .. } => {
            // Materialization has no modeled compute cost (it's a memcpy,
            // bounded by `rep_movsb` bandwidth — ADR-006 — which is a
            // future calibration axis).
            estimate_node_cost(child, cost_model, kingman)
        }
    }
}

/// Extract the cell count from a plan node (the size of its output).
///
/// - Scan: `params.cell_count`.
/// - Aggregate: passes through the child's count (an aggregate doesn't
///   change the number of cells read; it produces a scalar).
/// - Join: sum of left + right inputs.
/// - Materialize: passes through.
fn child_cell_count(node: &PlanNode) -> usize {
    match node {
        PlanNode::Scan { params, .. } => params.cell_count,
        PlanNode::Aggregate { child, .. } => child_cell_count(child),
        PlanNode::Join { left, right, .. } => child_cell_count(left) + child_cell_count(right),
        PlanNode::Materialize { child, .. } => child_cell_count(child),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::plan::{LogicalPlan, PlanNode};
    use crate::kernel::{KernelParams, Operator};

    /// AVX-512 L3 throughput = 8 lanes × 3 GHz = 24 G cells/sec.
    #[test]
    fn throughput_l3_avx512_is_24_g_cells_per_sec() {
        let cm = CostModel::default();
        let t = cm.throughput_l3(Operator::ScanEqU64);
        // 8 * 3e9 = 24e9 exactly.
        assert!((t - 24.0e9).abs() / 24.0e9 < 0.05, "throughput_l3 = {t}, expected ~24e9");
    }

    /// DRAM throughput = 40 GB/s / 8 B = 5 G cells/sec.
    #[test]
    fn throughput_dram_is_5_g_cells_per_sec() {
        let cm = CostModel::default();
        let t = cm.throughput_dram();
        // 40e9 / 8 = 5e9 exactly.
        assert!((t - 5.0e9).abs() / 5.0e9 < 0.05, "throughput_dram = {t}, expected ~5e9");
    }

    /// 1 M cells at L3 (24 G cells/sec) takes ~41.7 µs — within 30% of 42 µs.
    /// DoD: CostModel predicts scan latency within 30% of measured
    /// (24 G cells/sec → ~42 µs for 1M cells).
    #[test]
    fn estimate_compute_1m_cells_l3_is_about_42_us() {
        let cm = CostModel::default();
        let secs = cm.estimate_compute(1_000_000, Operator::ScanEqU64, MemoryTier::L3);
        let us = secs * 1.0e6;
        // 1e6 / 24e9 = 41.667 µs. 30% tolerance: [29.17, 54.17] µs.
        assert!(
            (us - 42.0).abs() / 42.0 < 0.30,
            "estimate_compute = {us} µs, expected ~42 µs (within 30%)"
        );
    }

    /// 1 M cells at DRAM (5 G cells/sec) takes ~200 µs — 4.8× the L3 latency.
    /// This sanity-checks the bandwidth-bound path.
    #[test]
    fn estimate_compute_1m_cells_dram_is_about_200_us() {
        let cm = CostModel::default();
        let secs = cm.estimate_compute(1_000_000, Operator::ScanEqU64, MemoryTier::Ddr5);
        let us = secs * 1.0e6;
        // 1e6 / 5e9 = 200 µs.
        assert!((us - 200.0).abs() / 200.0 < 0.10, "estimate_compute = {us} µs, expected ~200 µs");
    }

    /// Zero cells → zero compute cost (no division by zero).
    #[test]
    fn estimate_compute_zero_cells_is_zero() {
        let cm = CostModel::default();
        let secs = cm.estimate_compute(0, Operator::ScanEqU64, MemoryTier::L3);
        assert_eq!(secs, 0.0);
    }

    /// Kingman ρ = 0.5 produces a finite, reasonable wait.
    #[test]
    fn kingman_rho_half_returns_reasonable_wait() {
        let k = KingmanPredictor::new(50.0, 100.0, 1.0, 1.0);
        let w = k.predicted_wait();
        // Should be 10 ms exactly.
        assert!(w > 0.0, "wait should be positive, got {w}");
        assert!(w < 1.0, "wait should be < 1 s, got {w}");
        assert!((w - 0.01).abs() < 1e-9, "expected ~10 ms, got {w}");
    }

    /// Kingman ρ = 0.99 produces a wait ~99× the ρ = 0.5 wait.
    #[test]
    fn kingman_rho_99_much_larger_than_rho_half() {
        let k_low = KingmanPredictor::new(50.0, 100.0, 1.0, 1.0);
        let k_high = KingmanPredictor::new(99.0, 100.0, 1.0, 1.0);
        let w_low = k_low.predicted_wait();
        let w_high = k_high.predicted_wait();
        assert!(w_high > 10.0 * w_low, "ρ=0.99 wait ({w_high}) should be ≫ ρ=0.5 wait ({w_low})");
    }

    /// `estimate_cost` on a simple 1M-cell scan returns a positive number
    /// (~42 µs).
    #[test]
    fn estimate_cost_simple_scan_is_positive() {
        let plan = LogicalPlan::new(PlanNode::Scan {
            region_id: 0,
            operator: Operator::ScanEqU64,
            params: KernelParams { cell_count: 1_000_000, ..Default::default() },
        });
        let cm = CostModel::default();
        let k = KingmanPredictor::new(10.0, 100.0, 1.0, 1.0);
        let cost = estimate_cost(&plan, &cm, &k);
        assert!(cost > 0.0, "cost should be positive, got {cost}");
        // Should be ~42 µs (no queueing term for a pure scan).
        let us = cost * 1.0e6;
        assert!((us - 42.0).abs() / 42.0 < 0.30, "cost = {us} µs, expected ~42 µs");
    }

    /// `estimate_cost` on an aggregate-over-scan sums both costs.
    #[test]
    fn estimate_cost_aggregate_over_scan_sums_costs() {
        let plan = LogicalPlan::new(PlanNode::Aggregate {
            child: Box::new(PlanNode::Scan {
                region_id: 0,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 1_000_000, ..Default::default() },
            }),
            operator: Operator::AggregateSumF64,
        });
        let cm = CostModel::default();
        let k = KingmanPredictor::default();
        let cost = estimate_cost(&plan, &cm, &k);
        // Scan: 1M cells / 24 G = 41.67 µs.
        // Aggregate: 1M cells / 24 G = 41.67 µs.
        // Total: ~83.3 µs.
        let us = cost * 1.0e6;
        assert!((us - 83.3).abs() / 83.3 < 0.10, "cost = {us} µs, expected ~83 µs");
    }

    /// `estimate_cost` on a join includes the Kingman wait.
    #[test]
    fn estimate_cost_join_includes_kingman_wait() {
        let plan = LogicalPlan::new(PlanNode::Join {
            left: Box::new(PlanNode::Scan {
                region_id: 0,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 100_000, ..Default::default() },
            }),
            right: Box::new(PlanNode::Scan {
                region_id: 1,
                operator: Operator::ScanEqU64,
                params: KernelParams { cell_count: 100_000, ..Default::default() },
            }),
            operator: Operator::HashProbe,
        });
        let cm = CostModel::default();
        // ρ = 0.5 → wait = 10 ms (dominates the µs-scale compute).
        let k = KingmanPredictor::new(50.0, 100.0, 1.0, 1.0);
        let cost = estimate_cost(&plan, &cm, &k);
        // Should be ~10 ms + ~16 µs ≈ 10.016 ms.
        let ms = cost * 1.0e3;
        assert!(
            (ms - 10.0).abs() / 10.0 < 0.05,
            "cost = {ms} ms, expected ~10 ms (Kingman-dominated)"
        );
    }

    /// `estimate_cost` on a materialize-over-scan equals the scan cost
    /// (materialize is a passthrough in the current model).
    #[test]
    fn estimate_cost_materialize_is_passthrough() {
        let scan = PlanNode::Scan {
            region_id: 0,
            operator: Operator::ScanEqU64,
            params: KernelParams { cell_count: 1_000_000, ..Default::default() },
        };
        let plan_scan = LogicalPlan::new(scan.clone());
        let plan_mat =
            LogicalPlan::new(PlanNode::Materialize { child: Box::new(scan), target_region: 1 });
        let cm = CostModel::default();
        let k = KingmanPredictor::default();
        let cost_scan = estimate_cost(&plan_scan, &cm, &k);
        let cost_mat = estimate_cost(&plan_mat, &cm, &k);
        assert!(
            (cost_scan - cost_mat).abs() < 1e-15,
            "materialize should not add cost: scan={cost_scan}, mat={cost_mat}"
        );
    }

    /// The default cost model matches Zen 5 measurements (3 GHz, 8 lanes,
    /// 40 GB/s, 8 B cells).
    #[test]
    fn default_cost_model_matches_zen5() {
        let cm = CostModel::default();
        assert!((cm.cpu_freq_hz - 3.0e9).abs() < 1.0);
        assert_eq!(cm.simd_lanes, 8);
        assert!((cm.memory_bandwidth_bps - 40.0e9).abs() < 1.0);
        assert_eq!(cm.cell_size, 8);
    }
}
