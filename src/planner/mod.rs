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
//! - [`mcts`] — Monte Carlo Tree Search join ordering for `n > 15`
//!   relations (ADR-019, Wave 15). Falls back from DPccp when the relation
//!   count exceeds DPccp's `O(n²·2ⁿ)` budget.
//! - [`graph_prune`] — connectivity-based pruning for MCTS: cuts the
//!   branching factor from `n` down to the join-graph frontier degree.
//! - [`agm`] — Atserias-Grohe-Marx fractional cover bound, the worst-case
//!   size of a join result and the runtime bound of worst-case optimal join
//!   algorithms.
//! - [`wcoj`] — worst-case optimal join (Leapfrog triejoin) plan selection:
//!   picks between hash join and leapfrog based on the AGM bound.
//! - [`cardinality`] — simple per-table row-count and selectivity estimates
//!   used by the cost model and the join reorderer.
//! - [`learned`] — learned cardinality estimator: per-(table, column)
//!   equi-width histograms + an exponentially-weighted correction factor
//!   that augments the simple [`CardinalityEstimator`] with data-driven
//!   selectivity estimates.
//! - [`calibration`] — online calibration loop for [`learned`]: records
//!   `(predicted, actual)` cardinality pairs, updates the correction factor,
//!   and tracks the running MAPE.
//! - [`lowerer`] — cost-aware lowering of a `LogicalPlan` into a sequence of
//!   `KernelInvocation`s, picking the cheapest tier per operator and
//!   dispatching each join to either `HashProbe` or `LeapfrogJoin` via
//!   [`wcoj::choose_join_algorithm`].
//! - [`tensor`] — tensor-network model of a relational join (Wave 17).
//!   Models the join as a tensor-network contraction (arXiv:2209.12332),
//!   giving polynomial-time optimal ordering for acyclic queries via tree
//!   decomposition. The treewidth of the network equals the AGM bound
//!   exponent.
//! - [`contraction`] — converts a tensor contraction order into a
//!   [`JoinTree`] compatible with DPccp and MCTS (Wave 17).
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
pub mod calibration;
pub mod cardinality;
pub mod contraction;
pub mod dpccp;
pub mod graph_prune;
pub mod kingman;
pub mod learned;
pub mod lowerer;
pub mod mcts;
pub mod tensor;
pub mod wcoj;

pub use agm::{agm_bound, JoinHypergraph};
pub use calibration::CalibrationLoop;
pub use cardinality::CardinalityEstimator;
pub use contraction::contraction_to_join_tree;
pub use dpccp::{dpccp, JoinRelation, JoinTree};
pub use graph_prune::GraphPruner;
pub use kingman::KingmanPredictor;
pub use learned::{Histogram, LearnedCardinality};
pub use lowerer::PlanLowerer;
pub use mcts::MctsJoinOrderer;
pub use tensor::TensorNetwork;
pub use wcoj::{build_wcoj_plan, choose_join_algorithm, JoinAlgorithm, WcojPlan};

use crate::error::{Error, Result};
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
///
/// ## Learned cardinality
///
/// Since Wave 14, a [`LearnedCardinality`] estimator may optionally be
/// attached via [`Self::with_learned`]. When present, the cost model
/// delegates equality and range selectivity lookups to the learned
/// estimator (per-(table, column) histograms + global correction factor)
/// instead of falling back to the fixed `0.1` / `0.33` analytic defaults
/// from [`CardinalityEstimator`].
#[derive(Debug, Clone)]
pub struct CostModel {
    /// CPU clock frequency in Hz (e.g. `3.0e9` for 3 GHz).
    pub cpu_freq_hz: f64,
    /// SIMD lanes per kernel invocation (e.g. `8` for AVX-512 u64).
    pub simd_lanes: usize,
    /// Memory bandwidth in bytes/sec (e.g. `40e9` for 40 GB/s DRAM).
    pub memory_bandwidth_bps: f64,
    /// Cell size in bytes (always 8 — turbogp is a u64-word engine, ADR-001).
    pub cell_size: usize,
    /// Optional learned cardinality estimator (Wave 14). When `Some`,
    /// selectivity lookups are served from per-(table, column) histograms
    /// with a globally-corrected prior; when `None`, the cost model falls
    /// back to the analytic `0.1` / `0.33` defaults.
    pub learned: Option<LearnedCardinality>,
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

    /// Attach a [`LearnedCardinality`] estimator to this cost model.
    ///
    /// When the learned estimator is present, [`Self::estimate_selectivity`]
    /// and [`Self::estimate_range`] delegate to it (per-(table, column)
    /// histograms + global correction factor). When absent, they fall back
    /// to the analytic defaults (`0.1` for equality, `0.33` for range).
    ///
    /// Consumes `self` and returns a new `CostModel` with the estimator
    /// attached. The hardware parameters are preserved.
    #[must_use]
    pub fn with_learned(mut self, learned: LearnedCardinality) -> Self {
        self.learned = Some(learned);
        self
    }

    /// Borrow the attached [`LearnedCardinality`] estimator, if any.
    #[must_use]
    pub fn learned(&self) -> Option<&LearnedCardinality> {
        self.learned.as_ref()
    }

    /// Take ownership of the attached [`LearnedCardinality`] estimator,
    /// leaving `None` in its place.
    pub fn take_learned(&mut self) -> Option<LearnedCardinality> {
        self.learned.take()
    }

    /// Estimate the equality selectivity of `column = value` on `table`.
    ///
    /// When a [`LearnedCardinality`] estimator is attached, this delegates
    /// to [`LearnedCardinality::estimate_selectivity`] (histogram bucket
    /// density). Otherwise it falls back to the analytic default `0.1`
    /// (matching [`CardinalityEstimator::estimate_selectivity`] for
    /// equality predicates).
    #[must_use]
    pub fn estimate_selectivity(&self, table: &str, column: &str, value: u64) -> f64 {
        match &self.learned {
            Some(l) => l.estimate_selectivity(table, column, value),
            None => 0.1,
        }
    }

    /// Estimate the range selectivity of `low <= column <= high` on
    /// `table`.
    ///
    /// When a [`LearnedCardinality`] estimator is attached, this delegates
    /// to [`LearnedCardinality::estimate_range`] (sum of overlapping
    /// histogram buckets). Otherwise it falls back to the analytic default
    /// `0.33` (matching [`CardinalityEstimator::estimate_selectivity`] for
    /// range predicates).
    #[must_use]
    pub fn estimate_range(&self, table: &str, column: &str, low: u64, high: u64) -> f64 {
        match &self.learned {
            Some(l) => l.estimate_range(table, column, low, high),
            None => 0.33,
        }
    }

    /// Estimate the cardinality of an equi-join
    /// `left_table.left_col = right_table.right_col`.
    ///
    /// When a [`LearnedCardinality`] estimator is attached, this delegates
    /// to [`LearnedCardinality::estimate_join`] (per-bucket histogram
    /// overlap if both columns are trained, else FK assumption).
    /// Otherwise it falls back to the FK assumption
    /// `min(left_rows, right_rows)`.
    #[must_use]
    pub fn estimate_join(
        &self,
        left_table: &str,
        right_table: &str,
        left_col: &str,
        right_col: &str,
        left_rows: usize,
        right_rows: usize,
    ) -> f64 {
        match &self.learned {
            Some(l) => {
                l.estimate_join(left_table, right_table, left_col, right_col, left_rows, right_rows)
            }
            None => (left_rows.min(right_rows)) as f64,
        }
    }
}

impl Default for CostModel {
    /// Zen 5 defaults: 3 GHz, 8 AVX-512 lanes, 40 GB/s DRAM, 8-byte cells,
    /// no learned estimator.
    ///
    /// These match the measured throughputs in ADR-023 (24 G cells/sec L3,
    /// 5 G cells/sec DRAM) to within 5%. The `learned` field defaults to
    /// `None` — callers opt in to learned cardinality by calling
    /// [`CostModel::with_learned`].
    fn default() -> Self {
        Self {
            cpu_freq_hz: 3.0e9,
            simd_lanes: 8,
            memory_bandwidth_bps: 40.0e9,
            cell_size: 8,
            learned: None,
        }
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

/// Pick a join order for `relations`, dispatching to DPccp or MCTS based on
/// the relation count.
///
/// - **`n ≤ 15`**: uses [`dpccp`] (optimal, `O(n²·2ⁿ)`).
/// - **`n > 15`**: uses [`MctsJoinOrderer`] (near-optimal, anytime).
///
/// This is the single entry point for join ordering in turboGP — callers do
/// not need to know which algorithm is appropriate for their query size.
/// The returned [`JoinTree`] is drop-in compatible with both planners.
///
/// # Errors
///
/// Propagates errors from the underlying planner:
///
/// - [`Error::InvalidArg`] if `relations` is empty.
/// - [`Error::InvalidArg`] if `relations.len() > 64` (MCTS bitmask width).
/// - [`Error::InvalidArg`] if the join graph is disconnected.
///
/// # Examples
///
/// ```
/// use turbogp::planner::{order_joins, JoinRelation};
///
/// let relations = vec![
///     JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
///     JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0] },
/// ];
/// let tree = order_joins(&relations).expect("2-table join should succeed");
/// assert!(tree.cost() > 0.0);
/// ```
pub fn order_joins(relations: &[JoinRelation]) -> Result<JoinTree> {
    if relations.len() <= 15 {
        dpccp(relations)
    } else {
        MctsJoinOrderer::new().order(relations)
    }
}

/// Plan a join using the tensor-network contraction model (Wave 17).
///
/// This is the third join-ordering entry point in turboGP, alongside
/// [`order_joins`] (DPccp + MCTS) and [`dpccp`] (DPccp only). It models
/// the join as a tensor-network contraction (arXiv:2209.12332) and
/// finds the optimal contraction order for acyclic queries in
/// polynomial time via tree decomposition.
///
/// # Algorithm
///
/// 1. Build a [`TensorNetwork`] from the join hypergraph + per-relation
///    cardinalities.
/// 2. Find the optimal contraction order via
///    [`TensorNetwork::optimal_contraction_order`] (greedy minimum-cost
///    contraction — polynomial-time, optimal for acyclic queries).
/// 3. Convert the contraction order into a [`JoinTree`] via
///    [`contraction_to_join_tree`].
///
/// The resulting [`JoinTree`] uses the same cost formula as DPccp
/// (`cost(left) + cost(right) + |left| · |right|`), so its [`JoinTree::cost`]
/// is directly comparable to a DPccp plan's cost.
///
/// # When to use this vs. [`order_joins`]
///
/// - For **acyclic queries** with a known hypergraph (e.g., compiled
///   from SQL with explicit join predicates), `plan_with_tensor_network`
///   finds the optimal contraction in `O(n³)` time — faster than
///   DPccp's `O(n² · 2ⁿ)` for `n ≥ 10`.
/// - For **cyclic queries** (e.g., triangle, 4-clique), the greedy
///   contraction order is no longer guaranteed optimal — DPccp or MCTS
///   may find a better plan. The tensor-network plan is still a valid
///   starting point.
///
/// # Errors
///
/// - [`Error::InvalidArg`] if `relations` is empty.
/// - [`Error::InvalidArg`] if `relations.len() != graph.relations.len()`.
/// - [`Error::InvalidArg`] if `cardinalities.len() != relations.len()`.
/// - Propagates errors from [`contraction_to_join_tree`] if the
///   contraction order is incomplete or invalid.
///
/// # Examples
///
/// ```
/// use turbogp::planner::{plan_with_tensor_network, JoinHypergraph, JoinRelation};
///
/// let relations = vec![
///     JoinRelation { name: "R".into(), cardinality: 100, joins_with: vec![1] },
///     JoinRelation { name: "S".into(), cardinality: 200, joins_with: vec![0] },
/// ];
/// let graph = JoinHypergraph::from_named(&["A", "B"], &[vec!["A", "B"], vec!["A", "B"]]);
/// let tree = plan_with_tensor_network(&relations, &graph, &[100, 200])
///     .expect("tensor-network plan should succeed");
/// assert!(tree.cost() > 0.0);
/// ```
pub fn plan_with_tensor_network(
    relations: &[JoinRelation],
    graph: &JoinHypergraph,
    cardinalities: &[usize],
) -> Result<JoinTree> {
    if relations.is_empty() {
        return Err(Error::InvalidArg("plan_with_tensor_network: no relations provided".into()));
    }
    if relations.len() != graph.relations.len() {
        return Err(Error::InvalidArg(format!(
            "plan_with_tensor_network: {} relations but hypergraph has {} edges",
            relations.len(),
            graph.relations.len(),
        )));
    }
    if cardinalities.len() != relations.len() {
        return Err(Error::InvalidArg(format!(
            "plan_with_tensor_network: {} relations but {} cardinalities",
            relations.len(),
            cardinalities.len(),
        )));
    }
    let network = TensorNetwork::from_hypergraph(graph, cardinalities);
    let order = network.optimal_contraction_order();
    contraction_to_join_tree(&network, &order, relations)
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
        // Default has no learned estimator attached.
        assert!(cm.learned.is_none());
    }

    // -------------------------------------------------------------------------
    // Learned-cardinality integration (Wave 14)
    // -------------------------------------------------------------------------

    /// `CostModel::estimate_selectivity` falls back to `0.1` when no
    /// learned estimator is attached (matching `CardinalityEstimator`).
    #[test]
    fn cost_model_estimate_selectivity_without_learned_returns_default() {
        let cm = CostModel::default();
        let sel = cm.estimate_selectivity("orders", "id", 42);
        assert!((sel - 0.1).abs() < 1e-9, "no-learned selectivity = {sel}, expected 0.1");
    }

    /// `CostModel::estimate_range` falls back to `0.33` when no learned
    /// estimator is attached.
    #[test]
    fn cost_model_estimate_range_without_learned_returns_default() {
        let cm = CostModel::default();
        let sel = cm.estimate_range("orders", "id", 10, 20);
        assert!((sel - 0.33).abs() < 1e-9, "no-learned range = {sel}, expected 0.33");
    }

    /// `CostModel::estimate_join` falls back to the FK assumption when no
    /// learned estimator is attached.
    #[test]
    fn cost_model_estimate_join_without_learned_uses_fk_assumption() {
        let cm = CostModel::default();
        let join = cm.estimate_join("orders", "customers", "cust_id", "id", 1_000, 100);
        assert!(
            (join - 100.0).abs() < 1e-9,
            "no-learned join = {join}, expected min(1000,100)=100"
        );
    }

    /// `CostModel::with_learned` attaches a learned estimator, and the
    /// selectivity lookup is delegated to the histogram.
    #[test]
    fn cost_model_with_learned_delegates_to_histogram() {
        let mut learned = LearnedCardinality::new();
        // 1000 uniform values in [0, 999] → 100 buckets of width 10,
        // 10 rows per bucket.
        let values: Vec<u64> = (0..1000).collect();
        learned.train_table("orders", "id", &values);

        let cm = CostModel::default().with_learned(learned);
        assert!(cm.learned.is_some());

        // Value 50 lands in bucket 5 ([50, 60)), density = 10/1000 = 0.01.
        let sel = cm.estimate_selectivity("orders", "id", 50);
        assert!((sel - 0.01).abs() < 1e-9, "learned selectivity = {sel}, expected 0.01");

        // Range [25, 45] spans 3 buckets ([20,30), [30,40), [40,50)) → 0.03.
        let rsel = cm.estimate_range("orders", "id", 25, 45);
        assert!((rsel - 0.03).abs() < 1e-9, "learned range = {rsel}, expected 0.03");
    }

    /// `CostModel::with_learned` preserves the hardware parameters.
    #[test]
    fn cost_model_with_learned_preserves_hardware_params() {
        let cm = CostModel::default().with_learned(LearnedCardinality::new());
        assert!((cm.cpu_freq_hz - 3.0e9).abs() < 1.0);
        assert_eq!(cm.simd_lanes, 8);
        assert!((cm.memory_bandwidth_bps - 40.0e9).abs() < 1.0);
        assert_eq!(cm.cell_size, 8);
        assert!(cm.learned.is_some());
    }

    /// `CostModel::take_learned` removes the learned estimator.
    #[test]
    fn cost_model_take_learned_removes_estimator() {
        let mut cm = CostModel::default().with_learned(LearnedCardinality::new());
        assert!(cm.learned.is_some());
        let taken = cm.take_learned();
        assert!(taken.is_some());
        assert!(cm.learned.is_none());
        // After taking, selectivity falls back to the default.
        assert!((cm.estimate_selectivity("t", "c", 1) - 0.1).abs() < 1e-9);
    }

    /// `CostModel::estimate_join` with a learned estimator uses histogram
    /// overlap when both columns are trained.
    #[test]
    fn cost_model_estimate_join_with_learned_uses_histogram_overlap() {
        let mut learned = LearnedCardinality::new();
        let left: Vec<u64> = (0..100).collect();
        learned.train_table("L", "k", &left);
        let right: Vec<u64> = (0..100).collect();
        learned.train_table("R", "k", &right);
        let cm = CostModel::default().with_learned(learned);
        // Both columns have identical histograms covering [0, 100) with
        // 100 buckets of 1 row each. Every bucket overlaps, so the join
        // estimate = Σ min(1, 1) = 100.
        let join = cm.estimate_join("L", "R", "k", "k", 100, 100);
        assert!(
            (join - 100.0).abs() < 1e-9,
            "histogram-overlap join = {join}, expected 100 (full overlap)"
        );
    }

    /// `CostModel` is `Clone` (so callers can snapshot it before attaching
    /// a learned estimator and restore the analytic baseline if calibration
    /// goes wrong).
    #[test]
    fn cost_model_is_clone() {
        let cm = CostModel::default();
        let cm2 = cm.clone();
        assert!((cm.cpu_freq_hz - cm2.cpu_freq_hz).abs() < 1e-9);
        assert_eq!(cm.simd_lanes, cm2.simd_lanes);
        assert!(cm.learned.is_none());
        assert!(cm2.learned.is_none());
    }

    // -------------------------------------------------------------------------
    // Join-ordering dispatcher (Wave 15)
    // -------------------------------------------------------------------------

    /// `order_joins` for `n ≤ 15` uses DPccp: the cost matches
    /// [`dpccp::dpccp`] exactly.
    /// DoD: order_joins n≤15 uses DPccp, n>15 uses MCTS.
    #[test]
    fn order_joins_uses_dpccp_for_small_n() {
        let relations = vec![
            JoinRelation { name: "A".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "B".into(), cardinality: 200, joins_with: vec![0, 2] },
            JoinRelation { name: "C".into(), cardinality: 150, joins_with: vec![1, 3] },
            JoinRelation { name: "D".into(), cardinality: 50, joins_with: vec![2, 4] },
            JoinRelation { name: "E".into(), cardinality: 300, joins_with: vec![3] },
        ];
        let dispatcher_plan = order_joins(&relations).expect("order_joins n=5 should succeed");
        let dpccp_plan = dpccp(&relations).expect("dpccp n=5 should succeed");
        // For n ≤ 15, order_joins delegates to dpccp — same plan, same cost.
        assert!(
            (dispatcher_plan.cost() - dpccp_plan.cost()).abs() < 1e-9,
            "order_joins cost {} should equal dpccp cost {} for n=5",
            dispatcher_plan.cost(),
            dpccp_plan.cost()
        );
    }

    /// `order_joins` for `n > 15` uses MCTS: the call succeeds (DPccp
    /// alone would error out for `n = 20`).
    /// DoD: order_joins n≤15 uses DPccp, n>15 uses MCTS.
    #[test]
    fn order_joins_uses_mcts_for_large_n() {
        // 20-relation chain: DPccp rejects, MCTS handles it.
        let relations: Vec<JoinRelation> = (0..20)
            .map(|i| JoinRelation {
                name: format!("R{i}"),
                cardinality: 100,
                joins_with: {
                    let mut v = Vec::new();
                    if i > 0 {
                        v.push(i - 1);
                    }
                    if i + 1 < 20 {
                        v.push(i + 1);
                    }
                    v
                },
            })
            .collect();
        // Sanity check: DPccp alone rejects this.
        assert!(dpccp(&relations).is_err(), "DPccp should reject n=20");
        // The dispatcher succeeds by falling back to MCTS.
        let plan = order_joins(&relations).expect("order_joins n=20 should succeed via MCTS");
        // The plan should cover all 20 relations and have positive cost.
        assert!(plan.cost() > 0.0, "MCTS plan cost should be positive");
        // Count relations via the JoinTree (recursive walk).
        fn count_tree(t: &JoinTree) -> usize {
            match t {
                JoinTree::Leaf(_) => 1,
                JoinTree::Inner { left, right, .. } => count_tree(left) + count_tree(right),
            }
        }
        assert_eq!(count_tree(&plan), 20, "plan should contain all 20 relations");
    }

    /// `order_joins` for `n = 15` (the DPccp boundary) still uses DPccp.
    #[test]
    fn order_joins_at_dpccp_boundary_uses_dpccp() {
        let relations: Vec<JoinRelation> = (0..15)
            .map(|i| JoinRelation {
                name: format!("R{i}"),
                cardinality: 100,
                joins_with: {
                    let mut v = Vec::new();
                    if i > 0 {
                        v.push(i - 1);
                    }
                    if i + 1 < 15 {
                        v.push(i + 1);
                    }
                    v
                },
            })
            .collect();
        let dispatcher_plan =
            order_joins(&relations).expect("order_joins n=15 should succeed via DPccp");
        let dpccp_plan = dpccp(&relations).expect("dpccp n=15 should succeed");
        assert!(
            (dispatcher_plan.cost() - dpccp_plan.cost()).abs() < 1e-9,
            "order_joins cost {} should equal dpccp cost {} at n=15 boundary",
            dispatcher_plan.cost(),
            dpccp_plan.cost()
        );
    }

    /// `order_joins` for `n = 16` (just past the DPccp boundary) uses MCTS.
    #[test]
    fn order_joins_just_past_dpccp_boundary_uses_mcts() {
        let relations: Vec<JoinRelation> = (0..16)
            .map(|i| JoinRelation {
                name: format!("R{i}"),
                cardinality: 100,
                joins_with: {
                    let mut v = Vec::new();
                    if i > 0 {
                        v.push(i - 1);
                    }
                    if i + 1 < 16 {
                        v.push(i + 1);
                    }
                    v
                },
            })
            .collect();
        assert!(dpccp(&relations).is_err(), "DPccp should reject n=16");
        let plan = order_joins(&relations).expect("order_joins n=16 should succeed via MCTS");
        assert!(plan.cost() > 0.0, "MCTS plan cost should be positive");
    }

    /// `order_joins` rejects an empty input.
    #[test]
    fn order_joins_rejects_empty_input() {
        assert!(order_joins(&[]).is_err(), "order_joins should reject empty input");
    }

    /// Test 17-8: `plan_with_tensor_network` produces a valid plan for a
    /// 3-table join (triangle).
    #[test]
    fn plan_with_tensor_network_three_table_join() {
        let relations = vec![
            JoinRelation { name: "R".into(), cardinality: 100, joins_with: vec![1, 2] },
            JoinRelation { name: "S".into(), cardinality: 100, joins_with: vec![0, 2] },
            JoinRelation { name: "T".into(), cardinality: 100, joins_with: vec![0, 1] },
        ];
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        let tree = plan_with_tensor_network(&relations, &graph, &[100, 100, 100])
            .expect("tensor-network plan should succeed");
        // Cost should match the dpccp plan on the same triangle (the cost
        // formula is identical: cost(S ⋈ j) = cost(S) + cost(j) + |S|·|j|).
        // Triangle has 3 relations → 2 joins → cost = 100*100 + 100*100 = 20000.
        assert!(
            (tree.cost() - 20_000.0).abs() < 1e-6,
            "tensor-network plan cost = {}, expected 20000",
            tree.cost()
        );
        assert_eq!(tree.cardinality(), 100);
    }

    /// `plan_with_tensor_network` on a 5-relation star query produces a
    /// valid tree. The hypergraph has 5 relations on 6 attributes
    /// (center B, leaves A/C/D/E/F).
    #[test]
    fn plan_with_tensor_network_five_relation_star() {
        let relations = vec![
            JoinRelation { name: "R0".into(), cardinality: 100, joins_with: vec![1, 2, 3, 4] },
            JoinRelation { name: "R1".into(), cardinality: 200, joins_with: vec![0] },
            JoinRelation { name: "R2".into(), cardinality: 150, joins_with: vec![0] },
            JoinRelation { name: "R3".into(), cardinality: 50, joins_with: vec![0] },
            JoinRelation { name: "R4".into(), cardinality: 300, joins_with: vec![0] },
        ];
        // R0(A,B), R1(B,C), R2(B,D), R3(B,E), R4(B,F) — 5 relations on 6 attrs.
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C", "D", "E", "F"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["B", "D"], vec!["B", "E"], vec!["B", "F"]],
        );
        let tree = plan_with_tensor_network(&relations, &graph, &[100, 200, 150, 50, 300])
            .expect("tensor-network plan should succeed");
        assert!(tree.cost() > 0.0, "cost should be positive");
        assert!(tree.cardinality() > 0, "cardinality should be positive");
    }

    /// `plan_with_tensor_network` rejects an empty input.
    #[test]
    fn plan_with_tensor_network_rejects_empty_input() {
        let result = plan_with_tensor_network(
            &[],
            &JoinHypergraph { relations: vec![], attributes: vec![] },
            &[],
        );
        assert!(result.is_err(), "should reject empty input");
    }

    /// `plan_with_tensor_network` rejects mismatched lengths.
    #[test]
    fn plan_with_tensor_network_rejects_mismatched_lengths() {
        let relations =
            vec![JoinRelation { name: "R".into(), cardinality: 100, joins_with: vec![] }];
        let graph = JoinHypergraph::from_named(&["A"], &[vec!["A"], vec!["A"]]);
        let result = plan_with_tensor_network(&relations, &graph, &[100]);
        assert!(result.is_err(), "should reject mismatched relations/graph lengths");
    }

    /// `plan_with_tensor_network` rejects mismatched cardinalities.
    #[test]
    fn plan_with_tensor_network_rejects_mismatched_cardinalities() {
        let relations = vec![
            JoinRelation { name: "R".into(), cardinality: 100, joins_with: vec![1] },
            JoinRelation { name: "S".into(), cardinality: 200, joins_with: vec![0] },
        ];
        let graph = JoinHypergraph::from_named(&["A", "B"], &[vec!["A", "B"], vec!["A", "B"]]);
        let result = plan_with_tensor_network(&relations, &graph, &[100]);
        assert!(result.is_err(), "should reject mismatched cardinalities length");
    }
}
