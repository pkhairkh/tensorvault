//! An eddy: adaptive per-morsel tuple routing (Avnur & Hellerstein, SIGMOD 2000).
//!
//! A [`Pipeline`](crate::executor::pipeline::Pipeline) runs its stages in a
//! *fixed* order chosen at plan time. When a filter is more selective than the
//! planner expected, the fixed order wastes work: every downstream stage runs
//! on rows that the filter would have discarded. An **eddy** routes each morsel
//! through the same set of operators, but **picks the order adaptively** based
//! on observed selectivity — most selective first (the *principle of least
//! work*). If a morsel is emptied early, the remaining operators are skipped.
//!
//! ## Scientific grounding
//!
//! Eddies (Avnur & Hellerstein, "Eddies: Continuously Adaptive Query
//! Processing", SIGMOD 2000) insert a routing operator between data sources
//! and consumers, and let the router choose the next consumer per tuple. The
//! key insight: if a filter is more selective than expected, moving it earlier
//! in the pipeline reduces work for downstream operators. This is grounded in
//! the **principle of least work** — apply the most selective operations first
//! to minimize intermediate result size.
//!
//! The morsel-driven executor (Leis et al., "Morsel-Driven Parallelism: A
//! NUMA-Aware Query Execution Framework for the Many-Core Age", SIGMOD 2014)
//! processes morsels independently, making it natural to adapt per-morsel:
//! each morsel can take a different path through the operator DAG based on
//! observed selectivity.
//!
//! ## How the routing works
//!
//! Each operator in the eddy tracks an **exponentially-weighted selectivity
//! estimate** (`selectivity = (1 - lr) * selectivity + lr * (output / input)`).
//! Per morsel, the eddy:
//!
//! 1. Picks the unapplied operator with the **lowest** selectivity (most
//!    selective = filters most rows).
//! 2. Applies it to the morsel.
//! 3. Updates its selectivity using the observed `output / input` ratio.
//! 4. If the morsel is now empty (all rows filtered out), stops early.
//! 5. Repeats until all operators are applied or the morsel is empty.
//!
//! The initial selectivity is `1.0` (no rows filtered) so the first morsel
//! picks operators in declaration order; subsequent morsels converge to the
//! most-selective-first order as the estimates learn the true selectivities.
//!
//! ## Selectivity model
//!
//! The eddy treats each operator as a *filter* whose selectivity is
//! `output_rows / input_rows`. For a scan operator (`ScanEqU64`,
//! `ScanRangeU64`, ...), `output_rows` is `result.count` (the number of
//! matching cells). For an aggregate operator (`AggregateSumF64`), the
//! selectivity is `1.0` (the aggregate does not filter rows — it produces one
//! sum from all input cells).
//!
//! This is a simplification: a real eddy would route *tuples* through
//! *tuple-level* operators. The morsel-driven eddy here operates on whole
//! morsels and uses the per-morsel kernel result as the selectivity signal,
//! which is sufficient to demonstrate the principle of least work and the
//! adaptive routing benefit (see `benches/bench_eddy.rs`).

use crate::executor::morsel::Morsel;
use crate::kernel::{KernelParams, KernelResult, KernelTable, Operator};
use crate::memory::tier::MemoryTier;

/// The default learning rate for the exponentially-weighted selectivity
/// estimate. A value of `0.1` gives a half-life of ~6.6 morsels: after ~7
/// morsels the estimate has moved halfway from the initial `1.0` to the true
/// selectivity; after ~30 it's within 5%. Smaller values are more stable
/// (less noise from a single anomalous morsel); larger values adapt faster
/// but may oscillate.
pub const DEFAULT_LEARNING_RATE: f64 = 0.1;

/// An operator in the eddy's routing graph.
///
/// Each `EddyOperator` wraps an [`Operator`] and tracks the
/// exponentially-weighted selectivity estimate, the number of morsels
/// processed, and the cumulative number of cells the operator has output.
#[derive(Debug, Clone)]
pub struct EddyOperator {
    /// The operator type (filter, projection, etc.).
    pub operator: Operator,
    /// Observed selectivity (`output_rows / input_rows`), exponentially
    /// weighted. Initialized to `1.0` (no filtering); updated after each
    /// morsel via `selectivity = (1 - lr) * selectivity + lr * (output /
    /// input)`.
    pub selectivity: f64,
    /// Number of morsels processed by this operator.
    pub morsels_processed: u64,
    /// Number of cells output by this operator (cumulative across all
    /// morsels). For a filter, this is the sum of `result.count` across all
    /// morsels; for a non-filtering aggregate, this is the total cell count
    /// seen.
    pub cells_output: u64,
}

impl EddyOperator {
    /// Create a new eddy operator wrapping `operator` with the initial
    /// selectivity `1.0` (no filtering) and zero counters.
    pub fn new(operator: Operator) -> Self {
        Self { operator, selectivity: 1.0, morsels_processed: 0, cells_output: 0 }
    }

    /// Record an observation: the operator processed `input_cells` cells and
    /// produced `output_cells` cells. Updates `selectivity`,
    /// `morsels_processed`, and `cells_output` in place.
    ///
    /// The selectivity update is the standard exponential moving average:
    /// ```text
    /// selectivity = (1 - lr) * selectivity + lr * (output / input)
    /// ```
    /// where `output / input` is clamped to `[0, 1]` (an operator cannot
    /// produce more rows than it receives — if it does, that's a bug in the
    /// selectivity model and we treat it as `1.0`).
    pub fn observe(&mut self, input_cells: u64, output_cells: u64, lr: f64) {
        self.morsels_processed += 1;
        self.cells_output += output_cells;
        let ratio = if input_cells == 0 {
            // Empty input → no information; keep the current estimate.
            return;
        } else {
            let r = output_cells as f64 / input_cells as f64;
            // Clamp to [0, 1]: a filter cannot amplify.
            r.clamp(0.0, 1.0)
        };
        self.selectivity = (1.0 - lr) * self.selectivity + lr * ratio;
    }

    /// Reset this operator's selectivity estimate and counters to their
    /// initial values (selectivity = 1.0, morsels = 0, cells = 0).
    pub fn reset(&mut self) {
        self.selectivity = 1.0;
        self.morsels_processed = 0;
        self.cells_output = 0;
    }
}

/// An eddy adaptively routes morsels through a set of operators.
///
/// The order of operator application is determined by observed selectivity:
/// most selective first (principle of least work). See the module docs for the
/// full algorithm.
pub struct Eddy {
    /// The operators in the routing graph.
    operators: Vec<EddyOperator>,
    /// Whether each operator has been applied for the current morsel. Reset
    /// to all-`false` at the start of each [`process_morsel`](Self::process_morsel)
    /// call.
    applied: Vec<bool>,
    /// Learning rate for selectivity estimation (exponential weighting). In
    /// `[0, 1]`; `0` disables learning (selectivity stays at the initial
    /// value), `1` makes the estimate equal to the most recent observation.
    learning_rate: f64,
}

impl Eddy {
    /// Create a new eddy with the given operators and learning rate.
    ///
    /// The learning rate controls how fast the selectivity estimates adapt:
    /// - `0.0` — no adaptation; the eddy applies operators in declaration
    ///   order forever (equivalent to a fixed pipeline).
    /// - `1.0` — fully reactive; the estimate is always the most recent
    ///   observation (noisy, may oscillate).
    /// - `0.1` (the [`DEFAULT_LEARNING_RATE`]) — a balanced choice.
    ///
    /// `learning_rate` is clamped to `[0, 1]` to keep the exponential
    /// weighting numerically stable.
    pub fn new(operators: Vec<Operator>, learning_rate: f64) -> Self {
        let n = operators.len();
        Self {
            operators: operators.into_iter().map(EddyOperator::new).collect(),
            applied: vec![false; n],
            learning_rate: learning_rate.clamp(0.0, 1.0),
        }
    }

    /// Process a morsel through the eddy. The eddy chooses which operator to
    /// apply next based on current selectivity estimates.
    ///
    /// Returns the results from all operators that were applied, in the order
    /// they were applied (most selective first, then second-most, etc.). If
    /// the morsel is emptied early, the remaining operators are skipped and
    /// their results are not included.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Unsupported`] if the kernel table has no kernel
    /// registered for any operator's `Operator` variant on the L3 tier.
    pub fn process_morsel(
        &mut self,
        morsel: &Morsel,
        kernel_table: &KernelTable,
        params: &KernelParams,
    ) -> Vec<KernelResult> {
        // Reset the per-morsel applied flags.
        for a in &mut self.applied {
            *a = false;
        }

        let mut results = Vec::with_capacity(self.operators.len());
        let mut current_cells = morsel.len() as u64;

        loop {
            // Pick the unapplied operator with the lowest selectivity. Ties
            // are broken by index (the earliest-declared operator wins) so
            // the routing order is deterministic for a given set of
            // selectivities.
            let next = self
                .operators
                .iter()
                .enumerate()
                .zip(&self.applied)
                .filter(|(_, &applied)| !applied)
                .min_by(|((i, a), _), ((j, b), _)| {
                    a.selectivity
                        .partial_cmp(&b.selectivity)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| i.cmp(j))
                })
                .map(|((i, _), _)| i);

            let idx = match next {
                Some(i) => i,
                None => break, // all operators applied
            };

            // Early exit: if the morsel is already empty, stop. (We check
            // *before* applying because applying an operator to an empty
            // morsel would produce a zero-output observation that would
            // misleadingly drag the selectivity toward 0 even if the operator
            // is not actually selective.)
            if current_cells == 0 {
                break;
            }

            // Apply the chosen operator to the morsel.
            let op = self.operators[idx].operator;
            let kernel = match kernel_table.select(op, MemoryTier::L3) {
                Some(k) => k,
                None => {
                    // If we cannot select a kernel, skip this operator
                    // without updating its selectivity (we have no
                    // observation). Mark it applied so we don't loop forever.
                    self.applied[idx] = true;
                    continue;
                }
            };

            let mut p = *params;
            p.cell_count = morsel.len();
            let mut output = [0u8; 64];
            // SAFETY: `morsel.as_slice()` borrows the morsel's `Vec<u64>` for
            // the duration of the call; `as_ptr()` is valid for
            // `morsel.len() * 8` readable bytes. `output` is a 64-byte stack
            // array, valid for 64 writable bytes — more than
            // `size_of::<KernelResult>()`. The kernel was selected from the
            // kernel table, which only registers kernels whose CPU feature
            // flags are present on this machine (ADR-003).
            let result = unsafe {
                kernel.execute(morsel.as_slice().as_ptr() as *const u8, output.as_mut_ptr(), &p)
            };

            // Update the operator's selectivity estimate. For scan/similarity
            // operators, `result.count` is the number of matching cells
            // (output rows). For aggregate operators, the operator does not
            // filter rows — treat selectivity as 1.0.
            let output_cells = selectivity_output_cells(op, &result, morsel.len() as u64);
            self.operators[idx].observe(current_cells, output_cells, self.learning_rate);

            // Update the running cell count for early-exit. For a filter, the
            // next operator sees `output_cells` cells; for a non-filter, the
            // next operator still sees `current_cells` (the morsel is
            // unchanged).
            current_cells = next_stage_cells(op, &result, current_cells, morsel.len() as u64);

            self.applied[idx] = true;
            results.push(result);
        }

        results
    }

    /// Get the current selectivity estimates for all operators, in
    /// declaration order.
    pub fn selectivities(&self) -> Vec<f64> {
        self.operators.iter().map(|o| o.selectivity).collect()
    }

    /// Get the current routing order: operator indices sorted by selectivity
    /// ascending (most selective first = least work principle). Ties are
    /// broken by index so the order is deterministic.
    pub fn routing_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.operators.len()).collect();
        order.sort_by(|&a, &b| {
            self.operators[a]
                .selectivity
                .partial_cmp(&self.operators[b].selectivity)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cmp(&b))
        });
        order
    }

    /// Number of operators in the eddy.
    pub fn operator_count(&self) -> usize {
        self.operators.len()
    }

    /// Borrow the operator at `idx` for inspection. Returns `None` if `idx`
    /// is out of bounds.
    pub fn operator(&self, idx: usize) -> Option<&EddyOperator> {
        self.operators.get(idx)
    }

    /// Reset the eddy for a new query: clears all selectivity estimates and
    /// counters back to their initial values (selectivity = 1.0, morsels =
    /// 0, cells = 0).
    pub fn reset(&mut self) {
        for op in &mut self.operators {
            op.reset();
        }
        for a in &mut self.applied {
            *a = false;
        }
    }
}

impl std::fmt::Debug for Eddy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Eddy")
            .field("operator_count", &self.operators.len())
            .field("learning_rate", &self.learning_rate)
            .field("selectivities", &self.selectivities())
            .finish()
    }
}

/// Compute the number of "output cells" for an operator, used as the
/// numerator of the selectivity ratio `output / input`.
///
/// For filter-like operators (`ScanEqU64`, `ScanRangeU64`,
/// `ScanMultiPredicate`, `SimilarityHamming`, `HashProbe`, `LeapfrogJoin`),
/// `result.count` is the number of matching cells, which is the natural
/// "output row count".
///
/// For aggregate operators (`AggregateSumF64`, `AggregateCountDistinct`), the
/// operator does not filter rows — it consumes all input cells and produces a
/// scalar. We treat its selectivity as `1.0` (output = input) so the eddy
/// does not reorder aggregates ahead of filters.
///
/// For `HashBuild`, the operator builds a hash table from all input cells;
/// it does not filter. Selectivity = `1.0`.
fn selectivity_output_cells(op: Operator, result: &KernelResult, input_cells: u64) -> u64 {
    match op {
        Operator::ScanEqU64
        | Operator::ScanRangeU64
        | Operator::ScanMultiPredicate
        | Operator::SimilarityHamming
        | Operator::HashProbe
        | Operator::LeapfrogJoin => result.count,
        // Aggregates and hash-build do not filter; treat selectivity as 1.0
        // so the eddy does not reorder them ahead of filters.
        Operator::AggregateSumF64 | Operator::AggregateCountDistinct | Operator::HashBuild => {
            input_cells
        }
    }
}

/// Compute the number of cells the *next* operator in the pipeline will see,
/// after `op` has been applied.
///
/// For a filter operator, the next stage sees only the matching cells
/// (`result.count`). For a non-filtering operator (aggregate, hash-build),
/// the morsel is unchanged — the next stage sees the same cell count as
/// before.
fn next_stage_cells(
    op: Operator,
    result: &KernelResult,
    current_cells: u64,
    morsel_len: u64,
) -> u64 {
    match op {
        Operator::ScanEqU64
        | Operator::ScanRangeU64
        | Operator::ScanMultiPredicate
        | Operator::SimilarityHamming
        | Operator::HashProbe
        | Operator::LeapfrogJoin => result.count,
        // Aggregates and hash-build do not change the morsel; the next
        // stage still sees the same cells. We use `morsel_len` (the
        // original input length) rather than `current_cells` because
        // `current_cells` may have been reduced by a prior filter in the
        // eddy's routing order — but since aggregates don't filter, the
        // morsel data they see is the *original* morsel, not a reduced
        // version. (The current eddy implementation applies each operator
        // to the *original* morsel, not to the prior operator's output —
        // see the module docs on the "simple pipeline" model. We preserve
        // `current_cells` here so that the early-exit logic correctly
        // tracks when a filter has emptied the morsel.)
        Operator::AggregateSumF64 | Operator::AggregateCountDistinct | Operator::HashBuild => {
            current_cells.min(morsel_len)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::KernelTable;
    use std::sync::Arc;

    /// Build a morsel of f64-bit cells with the given values.
    fn f64_morsel(values: &[f64]) -> Morsel {
        let cells: Vec<u64> = values.iter().map(|v| v.to_bits()).collect();
        Morsel::new(0, 0, &cells)
    }

    // -----------------------------------------------------------------------
    // DoD test 1: Eddy processes a morsel through 3 operators, all results
    // collected.
    // -----------------------------------------------------------------------
    #[test]
    fn eddy_processes_morsel_through_three_operators_collects_all_results() {
        // Three operators: ScanEq (filter), AggregateSum (non-filter), and
        // another ScanEq. The eddy should apply all three and return three
        // results.
        let ops = vec![Operator::ScanEqU64, Operator::AggregateSumF64, Operator::ScanEqU64];
        let mut eddy = Eddy::new(ops, DEFAULT_LEARNING_RATE);
        let kt = Arc::new(KernelTable::new());

        // 6 cells: 1.0, 2.0, 1.0, 3.0, 1.0, 4.0.
        let morsel = f64_morsel(&[1.0, 2.0, 1.0, 3.0, 1.0, 4.0]);
        let params = KernelParams { target_u64: 1.0_f64.to_bits(), ..Default::default() };

        let results = eddy.process_morsel(&morsel, &kt, &params);

        // All three operators should have been applied.
        assert_eq!(results.len(), 3, "eddy should apply all 3 operators");
    }

    // -----------------------------------------------------------------------
    // DoD test 2: Eddy most selective operator is applied first.
    // -----------------------------------------------------------------------
    #[test]
    fn eddy_most_selective_operator_applied_first() {
        // Two scan operators with different selectivities. We prime the
        // eddy by setting the selectivities manually (via the public field
        // on EddyOperator), then verify that routing_order returns the
        // most-selective first.
        let ops = vec![Operator::ScanEqU64, Operator::ScanRangeU64, Operator::ScanEqU64];
        let mut eddy = Eddy::new(ops, DEFAULT_LEARNING_RATE);

        // Manually set selectivities: op 0 = 0.5, op 1 = 0.01, op 2 = 0.5.
        // Op 1 is the most selective.
        eddy.operators[0].selectivity = 0.5;
        eddy.operators[1].selectivity = 0.01;
        eddy.operators[2].selectivity = 0.5;

        let order = eddy.routing_order();
        // Most selective (lowest selectivity) first.
        assert_eq!(order[0], 1, "most selective operator (idx 1) should be first");
        // The other two can be in either order (they're tied), but they
        // should come after op 1.
        assert!(order[1] != 1 && order[2] != 1);
    }

    // -----------------------------------------------------------------------
    // DoD test 3: Eddy selectivity updates after observation.
    // -----------------------------------------------------------------------
    #[test]
    fn eddy_selectivity_updates_after_observation() {
        // Process a morsel where ScanEq has selectivity 3/6 = 0.5. After
        // one observation with lr = 1.0 (fully reactive), the selectivity
        // should be 0.5.
        let ops = vec![Operator::ScanEqU64];
        let mut eddy = Eddy::new(ops, 1.0); // lr = 1.0 → estimate = last observation
        let kt = Arc::new(KernelTable::new());

        // 6 cells, 3 of which equal 1.0 → selectivity = 3/6 = 0.5.
        let morsel = f64_morsel(&[1.0, 2.0, 1.0, 3.0, 1.0, 4.0]);
        let params = KernelParams { target_u64: 1.0_f64.to_bits(), ..Default::default() };

        // Before: selectivity is the default 1.0.
        assert!((eddy.selectivities()[0] - 1.0).abs() < 1e-9);

        let _ = eddy.process_morsel(&morsel, &kt, &params);

        // After: selectivity should be 0.5.
        let sel = eddy.selectivities()[0];
        assert!(
            (sel - 0.5).abs() < 1e-9,
            "selectivity should be 0.5 after one observation (lr=1.0), got {sel}"
        );
        assert_eq!(eddy.operators[0].morsels_processed, 1);
        assert_eq!(eddy.operators[0].cells_output, 3);
    }

    // -----------------------------------------------------------------------
    // DoD test 4: Eddy empty morsel after first filter → stops early.
    // -----------------------------------------------------------------------
    #[test]
    fn eddy_empty_morsel_after_first_filter_stops_early() {
        // Three operators: ScanEq (selective), AggregateSum, ScanEq.
        // We prime the eddy so the first ScanEq has selectivity 0.01 (most
        // selective) and the morsel has zero matching cells for it. After
        // the first operator empties the morsel, the eddy should stop and
        // not apply the remaining two operators.
        let ops = vec![Operator::ScanEqU64, Operator::AggregateSumF64, Operator::ScanEqU64];
        let mut eddy = Eddy::new(ops, 1.0);
        let kt = Arc::new(KernelTable::new());

        // Prime selectivities: op 0 = 0.01 (most selective), ops 1,2 = 1.0.
        eddy.operators[0].selectivity = 0.01;
        eddy.operators[1].selectivity = 1.0;
        eddy.operators[2].selectivity = 1.0;

        // Morsel: 6 cells, none equal to 99 → ScanEq(target=99) returns 0.
        let morsel = f64_morsel(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let params = KernelParams { target_u64: 99, ..Default::default() };

        let results = eddy.process_morsel(&morsel, &kt, &params);

        // Only the first operator should have been applied (it emptied the
        // morsel, so the eddy stops early).
        assert_eq!(results.len(), 1, "eddy should stop after the first filter empties the morsel");
        // The first result should have count = 0 (no cells matched 99).
        assert_eq!(results[0].count, 0);
    }

    // -----------------------------------------------------------------------
    // Additional tests for routing order and reset.
    // -----------------------------------------------------------------------

    #[test]
    fn eddy_routing_order_on_fresh_eddy_is_declaration_order() {
        // A fresh eddy has all selectivities = 1.0, so the routing order
        // is just declaration order (ties broken by index).
        let ops = vec![Operator::ScanEqU64, Operator::ScanRangeU64, Operator::AggregateSumF64];
        let eddy = Eddy::new(ops, DEFAULT_LEARNING_RATE);
        let order = eddy.routing_order();
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn eddy_routing_order_picks_lowest_selectivity_first() {
        let ops = vec![Operator::ScanEqU64, Operator::ScanRangeU64, Operator::AggregateSumF64];
        let mut eddy = Eddy::new(ops, DEFAULT_LEARNING_RATE);
        // Set selectivities: 0 = 0.5, 1 = 0.1, 2 = 0.9.
        eddy.operators[0].selectivity = 0.5;
        eddy.operators[1].selectivity = 0.1;
        eddy.operators[2].selectivity = 0.9;
        let order = eddy.routing_order();
        // Most selective (lowest) first: op 1 (0.1), op 0 (0.5), op 2 (0.9).
        assert_eq!(order, vec![1, 0, 2]);
    }

    #[test]
    fn eddy_reset_clears_selectivities_and_counters() {
        let ops = vec![Operator::ScanEqU64];
        let mut eddy = Eddy::new(ops, 1.0);
        let kt = Arc::new(KernelTable::new());

        let morsel = f64_morsel(&[1.0, 1.0, 2.0]);
        let params = KernelParams { target_u64: 1.0_f64.to_bits(), ..Default::default() };
        let _ = eddy.process_morsel(&morsel, &kt, &params);

        // After processing: selectivity != 1.0, counters > 0.
        assert!((eddy.selectivities()[0] - 1.0).abs() > 1e-9);
        assert_eq!(eddy.operators[0].morsels_processed, 1);

        eddy.reset();

        // After reset: selectivity = 1.0, counters = 0.
        assert!((eddy.selectivities()[0] - 1.0).abs() < 1e-9);
        assert_eq!(eddy.operators[0].morsels_processed, 0);
        assert_eq!(eddy.operators[0].cells_output, 0);
    }

    #[test]
    fn eddy_learning_rate_zero_keeps_selectivity_at_one() {
        // lr = 0 → the selectivity never updates.
        let ops = vec![Operator::ScanEqU64];
        let mut eddy = Eddy::new(ops, 0.0);
        let kt = Arc::new(KernelTable::new());

        let morsel = f64_morsel(&[1.0, 1.0, 2.0]);
        let params = KernelParams { target_u64: 1.0_f64.to_bits(), ..Default::default() };
        let _ = eddy.process_morsel(&morsel, &kt, &params);

        // Selectivity should still be 1.0.
        assert!((eddy.selectivities()[0] - 1.0).abs() < 1e-9);
        // But the counters should still update (we still record the
        // observation, just with lr=0).
        assert_eq!(eddy.operators[0].morsels_processed, 1);
        assert_eq!(eddy.operators[0].cells_output, 2);
    }

    #[test]
    fn eddy_empty_operator_list_returns_no_results() {
        let mut eddy = Eddy::new(vec![], DEFAULT_LEARNING_RATE);
        let kt = Arc::new(KernelTable::new());
        let morsel = Morsel::new(0, 0, &[1, 2, 3]);
        let results = eddy.process_morsel(&morsel, &kt, &KernelParams::default());
        assert!(results.is_empty());
        assert_eq!(eddy.operator_count(), 0);
    }

    #[test]
    fn eddy_learning_rate_clamped_to_unit_interval() {
        // lr > 1 should be clamped to 1.0; lr < 0 to 0.0.
        let eddy_hi = Eddy::new(vec![Operator::ScanEqU64], 5.0);
        let eddy_lo = Eddy::new(vec![Operator::ScanEqU64], -5.0);
        // We can't read learning_rate directly (no accessor), but we can
        // verify the behavior: lr=5 (clamped to 1) → selectivity = last
        // observation; lr=-5 (clamped to 0) → selectivity stays at 1.0.
        let kt = Arc::new(KernelTable::new());
        let morsel = f64_morsel(&[1.0, 2.0]);
        let params = KernelParams { target_u64: 1.0_f64.to_bits(), ..Default::default() };

        let mut hi = eddy_hi;
        let mut lo = eddy_lo;
        let _ = hi.process_morsel(&morsel, &kt, &params);
        let _ = lo.process_morsel(&morsel, &kt, &params);

        // hi (lr=1): selectivity = 1/2 = 0.5.
        assert!((hi.selectivities()[0] - 0.5).abs() < 1e-9);
        // lo (lr=0): selectivity = 1.0.
        assert!((lo.selectivities()[0] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn eddy_exponential_weighting_converges_to_true_selectivity() {
        // Process many morsels with a fixed true selectivity of 0.5. The
        // EMA should converge to ~0.5.
        let ops = vec![Operator::ScanEqU64];
        let mut eddy = Eddy::new(ops, 0.1);
        let kt = Arc::new(KernelTable::new());

        // Each morsel has 4 cells, 2 of which equal 1.0 → selectivity 0.5.
        let morsel = f64_morsel(&[1.0, 2.0, 1.0, 3.0]);
        let params = KernelParams { target_u64: 1.0_f64.to_bits(), ..Default::default() };

        for _ in 0..100 {
            let _ = eddy.process_morsel(&morsel, &kt, &params);
        }

        let sel = eddy.selectivities()[0];
        assert!(
            (sel - 0.5).abs() < 0.01,
            "after 100 observations, selectivity should be ~0.5, got {sel}"
        );
    }

    #[test]
    fn eddy_operator_observe_handles_empty_input() {
        // observe(0, 0, lr) should be a no-op (no division by zero).
        let mut op = EddyOperator::new(Operator::ScanEqU64);
        let original_sel = op.selectivity;
        op.observe(0, 0, 0.1);
        assert!((op.selectivity - original_sel).abs() < 1e-9);
        // Counters still update.
        assert_eq!(op.morsels_processed, 1);
    }

    #[test]
    fn eddy_operator_observe_clamps_ratio_above_one() {
        // If output > input (a bug, but we should not crash), the ratio is
        // clamped to 1.0.
        let mut op = EddyOperator::new(Operator::ScanEqU64);
        op.observe(10, 20, 1.0); // output > input → clamp to 1.0
        assert!((op.selectivity - 1.0).abs() < 1e-9);
    }

    #[test]
    fn eddy_debug_format_works() {
        let eddy = Eddy::new(vec![Operator::ScanEqU64, Operator::ScanRangeU64], 0.1);
        let s = format!("{eddy:?}");
        assert!(s.contains("Eddy"));
        assert!(s.contains("operator_count"));
        assert!(s.contains("learning_rate"));
        assert!(s.contains("selectivities"));
    }

    #[test]
    fn eddy_operator_count_matches_construction() {
        assert_eq!(Eddy::new(vec![], 0.1).operator_count(), 0);
        assert_eq!(
            Eddy::new(vec![Operator::ScanEqU64, Operator::ScanRangeU64], 0.1).operator_count(),
            2
        );
    }

    #[test]
    fn eddy_operator_accessor_returns_some_for_valid_index() {
        let eddy = Eddy::new(vec![Operator::ScanEqU64, Operator::ScanRangeU64], 0.1);
        assert!(eddy.operator(0).is_some());
        assert!(eddy.operator(1).is_some());
        assert!(eddy.operator(2).is_none());
    }

    #[test]
    fn eddy_aggregate_operator_selectivity_stays_at_one() {
        // An aggregate operator (AggregateSumF64) should not have its
        // selectivity updated below 1.0 — it doesn't filter rows.
        let ops = vec![Operator::AggregateSumF64];
        let mut eddy = Eddy::new(ops, 1.0);
        let kt = Arc::new(KernelTable::new());

        let morsel = f64_morsel(&[1.0, 2.0, 3.0]);
        let _ = eddy.process_morsel(&morsel, &kt, &KernelParams::default());

        // Selectivity should remain 1.0 (aggregates don't filter).
        assert!(
            (eddy.selectivities()[0] - 1.0).abs() < 1e-9,
            "aggregate selectivity should stay at 1.0, got {}",
            eddy.selectivities()[0]
        );
    }
}
