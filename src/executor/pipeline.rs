//! A pipeline of operators that processes morsels in L1 (ADR-018).
//!
//! A [`Pipeline`] is a fixed sequence of [`Operator`]s (e.g.
//! `Scan → Filter → Aggregate`) that a worker applies to each morsel it
//! receives. The pipeline is *push-based and data-centric*: the worker pushes
//! a morsel into the pipeline, each stage runs in turn on the morsel's L1
//! data, and the per-stage results accumulate in `results` for the caller to
//! reduce across morsels.
//!
//! ## Why a pipeline?
//!
//! The Volcano pull model (iterator `next()`) is 5–10× slower than push-based
//! execution because every `next()` call is a function call that the CPU's
//! branch predictor cannot fuse across, and intermediate values spill to
//! DRAM between operators. The morsel-driven model (Leis 2014, ADR-018)
//! keeps intermediate data in L1/L2 by running the whole pipeline on a single
//! morsel before moving to the next morsel. The pipeline here is the
//! per-morsel, per-worker unit of execution.
//!
//! ## Pipeline breakers
//!
//! Some operators cannot pipeline — they need to consume their entire input
//! before producing any output. The canonical example is the build side of a
//! hash join: the hash table must be fully constructed before the probe side
//! can run. Such operators are *pipeline breakers* and force materialization
//! of intermediate data to DRAM via [`PipelineBreaker`]. After the breaker is
//! drained, a new pipeline begins on the materialized data.
//!
//! ## Note on stage-to-stage data flow
//!
//! The current implementation is a *simple* pipeline: each stage runs the same
//! morsel as input, and the per-stage result is appended to `results`. This is
//! sufficient for kernels that produce a scalar aggregate (count, sum) — the
//! caller can sum the per-morsel counts across all morsels. A future version
//! that supports filter-then-aggregate (where the filter's output mask feeds
//! the aggregate's input) will thread the morsel through each stage's output;
//! that requires a richer stage representation than `Operator` and is deferred
//! to a later wave.

use crate::executor::morsel::Morsel;
use crate::kernel::{KernelParams, KernelResult, KernelTable, Operator};
use crate::memory::tier::MemoryTier;
use crate::Result;

/// A pipeline of operators that processes morsels in L1.
///
/// Example: `Scan → Filter → Aggregate`.
///
/// Construct with [`Pipeline::new`] from a `Vec<Operator>`. Each call to
/// [`execute_morsel`](Pipeline::execute_morsel) runs every stage on the given
/// morsel and appends one [`KernelResult`] per stage to `results`. Use
/// [`results`](Pipeline::results) to inspect the accumulated outputs and
/// [`reset`](Pipeline::reset) to clear the accumulator before processing a
/// new batch of morsels (typically before each new region).
pub struct Pipeline {
    /// The stages of the pipeline (kernels to execute in order).
    stages: Vec<Operator>,
    /// Accumulated results from the last stage of each morsel, in
    /// (morsel_index, stage_index) order — i.e. `results[morsel_idx *
    /// stages.len() + stage_idx]`. Caller-facing consumers typically reduce
    /// across morsels for a fixed stage.
    results: Vec<KernelResult>,
}

impl Pipeline {
    /// Create a new pipeline with the given stages.
    pub fn new(stages: Vec<Operator>) -> Self {
        Self { stages, results: Vec::new() }
    }

    /// Run a single morsel through all stages of the pipeline.
    ///
    /// For each stage, the pipeline looks up the kernel for
    /// `(operator, MemoryTier::L3)` in the kernel table (morsel data is L1-
    /// resident, but the kernel table only registers L3/Ddr5/Cxl tiers; L3 is
    /// the closest match), overrides `params.cell_count` with the morsel's
    /// valid length, and executes the kernel on the morsel's data.
    ///
    /// Each stage's [`KernelResult`] is appended to `self.results`. After
    /// processing all morsels for a region, call [`results`](Self::results) to
    /// inspect the accumulated outputs.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Unsupported`] if the kernel table has no
    /// kernel registered for any stage's operator.
    pub fn execute_morsel(
        &mut self,
        morsel: &Morsel,
        kernel_table: &KernelTable,
        params: &KernelParams,
    ) -> Result<()> {
        let mut params = *params;
        params.cell_count = morsel.len();
        // 64-byte output buffer: comfortably fits any KernelResult (32 bytes)
        // with alignment headroom. Allocated on the stack — no per-morsel
        // heap allocation.
        let mut output = [0u8; 64];
        for &op in &self.stages {
            let kernel = kernel_table.select(op, MemoryTier::L3).ok_or_else(|| {
                crate::Error::Unsupported(format!("no kernel for operator {op:?}"))
            })?;
            // SAFETY: `morsel.as_slice()` borrows the morsel's `Vec<u64>` for
            // the duration of the call; `as_ptr()` is valid for
            // `morsel.len() * 8` readable bytes. `output` is a 64-byte stack
            // array, valid for 64 writable bytes — more than
            // `size_of::<KernelResult>()`. The kernel was selected from the
            // kernel table, which only registers kernels whose CPU feature
            // flags are present on this machine (ADR-003).
            let result = unsafe {
                kernel.execute(
                    morsel.as_slice().as_ptr() as *const u8,
                    output.as_mut_ptr(),
                    &params,
                )
            };
            self.results.push(result);
        }
        Ok(())
    }

    /// Returns the accumulated per-stage results from all morsels processed
    /// since the last [`reset`](Self::reset).
    ///
    /// The layout is `results[morsel_idx * stages.len() + stage_idx]`. Callers
    /// that want a per-stage reduction across morsels should iterate by stride
    /// `stages.len()`; see the test `pipeline_2_stage_produces_count_and_sum`
    /// for an example.
    pub fn results(&self) -> &[KernelResult] {
        &self.results
    }

    /// Number of stages in the pipeline.
    pub fn stage_count(&self) -> usize {
        self.stages.len()
    }

    /// Clears accumulated results, readying the pipeline for a new batch of
    /// morsels.
    ///
    /// Does NOT clear `stages` — the pipeline structure is fixed at
    /// construction. Only the per-morsel result accumulator is reset.
    pub fn reset(&mut self) {
        self.results.clear();
    }
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("stages", &self.stages)
            .field("result_count", &self.results.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// PipelineBreaker
// ---------------------------------------------------------------------------

/// A pipeline breaker forces materialization to DRAM (e.g., hash join build
/// side).
///
/// When a pipeline contains a breaking operator (sort, hash build, full
/// outer join), the data cannot stay in L1 across the breaker — the breaker
/// must consume its entire input before producing any output. The
/// [`PipelineBreaker`] is the buffer that holds the materialized data: morsels
/// `push` their cells into the breaker as they arrive, and once all morsels
/// have been processed, the next pipeline stage calls [`drain`](Self::drain)
/// to read the materialized data.
///
/// The breaker is a `Vec<u64>` (a flat 8-byte-cell buffer). A real
/// implementation would back this with a NUMA-aware huge-page allocation to
/// avoid TLB pressure on the probe side; the Vec is sufficient for v1 and
/// keeps the API simple.
#[derive(Debug, Default)]
pub struct PipelineBreaker {
    /// The materialized data.
    materialized: Vec<u64>,
}

impl PipelineBreaker {
    /// Create a new, empty pipeline breaker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a batch of cells to the materialized buffer.
    ///
    /// Called once per morsel by the pipeline stage that produces the breaker
    /// input. The cells are copied into the breaker's owned `Vec<u64>`; the
    /// morsel itself can be dropped or reused after this call returns.
    pub fn push(&mut self, data: &[u64]) {
        self.materialized.extend_from_slice(data);
    }

    /// Return all materialized data and clear the breaker.
    ///
    /// After `drain`, the breaker is empty and ready for a new build phase.
    /// The returned `Vec<u64>` is the breaker's owned buffer — no copy.
    pub fn drain(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.materialized)
    }

    /// Number of materialized cells currently held by the breaker.
    pub fn len(&self) -> usize {
        self.materialized.len()
    }

    /// Returns `true` if the breaker holds no materialized cells.
    pub fn is_empty(&self) -> bool {
        self.materialized.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::morsel::Morsel;
    use crate::kernel::{KernelParams, KernelTable, Operator};
    use std::sync::Arc;

    #[test]
    fn pipeline_2_stage_produces_count_and_sum() {
        // Build a 2-stage pipeline: ScanEq(target=1.0) → AggregateSum.
        // Both stages run on the same morsel (per the current "simple
        // pipeline" model). We pick cells that are f64::to_bits of small
        // floats so the sum aggregate produces a recognizable total.
        let stages = vec![Operator::ScanEqU64, Operator::AggregateSumF64];
        let mut pipeline = Pipeline::new(stages);
        let kt = Arc::new(KernelTable::new());

        // cells = bits(1.0, 2.0, 1.0, 3.0, 1.0, 4.0)
        //   ScanEq(target=bits(1.0)) → count = 3 (cells 0, 2, 4)
        //   AggregateSum             → sum   = 1+2+1+3+1+4 = 12.0
        let values = [1.0_f64, 2.0, 1.0, 3.0, 1.0, 4.0];
        let cells: Vec<u64> = values.iter().map(|v| v.to_bits()).collect();
        let morsel = Morsel::new(0, 0, &cells);

        let params = KernelParams { target_u64: 1.0_f64.to_bits(), ..Default::default() };
        pipeline.execute_morsel(&morsel, &kt, &params).expect("pipeline executes");

        // 2 stages → 2 results.
        assert_eq!(pipeline.results().len(), 2);
        assert_eq!(pipeline.stage_count(), 2);

        // Stage 0: ScanEq → count of cells == bits(1.0).
        let scan_result = pipeline.results()[0];
        assert_eq!(scan_result.count, 3, "ScanEq should find 3 ones");

        // Stage 1: AggregateSum → sum of f64::from_bits(cell) for all cells.
        let agg_result = pipeline.results()[1];
        let expected_sum: f64 = values.iter().sum();
        assert!(
            (agg_result.sum - expected_sum).abs() < 1e-9,
            "AggregateSum should be {expected_sum}, got {}",
            agg_result.sum
        );
        assert_eq!(agg_result.count, cells.len() as u64);
    }

    #[test]
    fn pipeline_reset_clears_results() {
        let stages = vec![Operator::ScanEqU64];
        let mut pipeline = Pipeline::new(stages);
        let kt = Arc::new(KernelTable::new());

        let cells = vec![1_u64, 2, 3, 1];
        let morsel = Morsel::new(0, 0, &cells);
        let params = KernelParams { target_u64: 1, ..Default::default() };
        pipeline.execute_morsel(&morsel, &kt, &params).unwrap();
        assert_eq!(pipeline.results().len(), 1);

        pipeline.reset();
        assert!(pipeline.results().is_empty());

        // Re-run on a different morsel to confirm reset didn't break the
        // pipeline structure.
        let cells2 = vec![5_u64, 5, 5];
        let morsel2 = Morsel::new(1, 0, &cells2);
        pipeline.execute_morsel(&morsel2, &kt, &params).unwrap();
        assert_eq!(pipeline.results().len(), 1);
    }

    #[test]
    fn pipeline_accumulates_across_multiple_morsels() {
        // Two morsels, one stage (ScanEq). The pipeline should accumulate
        // one result per (morsel, stage) — i.e. 2 results total.
        let stages = vec![Operator::ScanEqU64];
        let mut pipeline = Pipeline::new(stages);
        let kt = Arc::new(KernelTable::new());

        let m1 = Morsel::new(0, 0, &[1_u64, 1, 1, 2]);
        let m2 = Morsel::new(0, 4, &[1_u64, 2, 2, 2]);
        let params = KernelParams { target_u64: 1, ..Default::default() };

        pipeline.execute_morsel(&m1, &kt, &params).unwrap();
        pipeline.execute_morsel(&m2, &kt, &params).unwrap();

        assert_eq!(pipeline.results().len(), 2);
        // First morsel: 3 ones.
        assert_eq!(pipeline.results()[0].count, 3);
        // Second morsel: 1 one.
        assert_eq!(pipeline.results()[1].count, 1);
        // Caller-reduced total: 4.
        let total: u64 = pipeline.results().iter().map(|r| r.count).sum();
        assert_eq!(total, 4);
    }

    #[test]
    fn pipeline_empty_stages_is_noop() {
        let mut pipeline = Pipeline::new(vec![]);
        let kt = Arc::new(KernelTable::new());
        let morsel = Morsel::new(0, 0, &[1_u64, 2, 3]);
        pipeline.execute_morsel(&morsel, &kt, &KernelParams::default()).unwrap();
        assert!(pipeline.results().is_empty());
        assert_eq!(pipeline.stage_count(), 0);
    }

    #[test]
    fn pipeline_breaker_push_three_batches_drain_returns_all() {
        let mut breaker = PipelineBreaker::new();
        assert!(breaker.is_empty());
        assert_eq!(breaker.len(), 0);

        breaker.push(&[1_u64, 2, 3]);
        breaker.push(&[4_u64, 5]);
        breaker.push(&[6_u64, 7, 8, 9]);

        assert_eq!(breaker.len(), 9);
        assert!(!breaker.is_empty());

        let drained = breaker.drain();
        assert_eq!(drained, vec![1_u64, 2, 3, 4, 5, 6, 7, 8, 9]);

        // Drain empties the breaker.
        assert!(breaker.is_empty());
        assert_eq!(breaker.len(), 0);
    }

    #[test]
    fn pipeline_breaker_drain_on_empty_returns_empty_vec() {
        let mut breaker = PipelineBreaker::new();
        let drained = breaker.drain();
        assert!(drained.is_empty());
    }

    #[test]
    fn pipeline_breaker_reusable_after_drain() {
        let mut breaker = PipelineBreaker::new();
        breaker.push(&[10_u64, 20]);
        let _ = breaker.drain();
        // Reuse: push and drain again.
        breaker.push(&[30_u64, 40, 50]);
        let drained = breaker.drain();
        assert_eq!(drained, vec![30_u64, 40, 50]);
    }

    #[test]
    fn pipeline_debug_format_works() {
        let pipeline = Pipeline::new(vec![Operator::ScanEqU64, Operator::AggregateSumF64]);
        let s = format!("{pipeline:?}");
        assert!(s.contains("Pipeline"));
        assert!(s.contains("stages"));
        assert!(s.contains("result_count"));
    }
}
