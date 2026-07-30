//! Adaptive plan switching based on observed cardinality.
//!
//! The planner picks a plan based on *estimated* cardinalities. If the
//! estimates are wrong (and they often are — cardinality estimation is the
//! "Achilles heel" of query optimization), the chosen plan can be orders of
//! magnitude worse than the alternative. The [`AdaptiveExecutor`] monitors the
//! observed cardinality at each pipeline stage during execution and signals a
//! plan switch when the observed cardinality diverges from the estimate beyond
//! a configurable threshold.
//!
//! ## Scientific grounding
//!
//! Adaptive query processing (Deshpande, "Adaptive Query Processing", Foundations
//! and Trends in Databases, 2007) covers a family of techniques that adjust
//! the execution plan at runtime based on observed statistics. The two main
//! strategies are:
//!
//! - **Plan switching** (a.k.a. *interleaved planning*): monitor execution,
//!   and if the observed cardinality diverges from the estimate, re-plan the
//!   remaining stages with the new information.
//! - **Eddies** (Avnur & Hellerstein, SIGMOD 2000): route tuples
//!   per-operator adaptively (see [`crate::executor::eddy`]).
//!
//! The [`AdaptiveExecutor`] implements plan switching. It does not execute
//! plans itself — it is a *monitor* that wraps an existing executor and
//! answers the question "should we re-plan?". The actual re-planning is the
//! caller's responsibility (the caller can use the [`crate::planner`] module
//! to re-order joins, pick a different access path, etc., using the observed
//! cardinalities as the new estimates).
//!
//! ## Divergence metric
//!
//! The divergence at stage `i` is:
//!
//! ```text
//! divergence_i = |observed_i - estimated_i| / max(estimated_i, 1)
//! ```
//!
//! The `max(estimated_i, 1)` guard prevents divide-by-zero when the estimate
//! is 0 (we treat "estimated 0, observed N" as infinite divergence if N > 0,
//! and 0 divergence if N == 0). The [`AdaptiveExecutor::max_divergence`]
//! method returns the maximum divergence across all observed stages — this is
//! the metric compared against `switch_threshold`.
//!
//! ## Stickiness
//!
//! Once a switch is triggered (via [`AdaptiveExecutor::observe`]), the
//! `switched` flag stays `true` for the lifetime of the executor. This
//! prevents flapping: if the divergence oscillates around the threshold, we
//! don't want to trigger a switch on every morsel. Callers that want to reset
//! the flag (e.g., after re-planning) can construct a new `AdaptiveExecutor`
//! with the updated estimates.

use crate::executor::plan::LogicalPlan;

/// Monitors query execution and switches plans if the observed cardinality
/// diverges significantly from the estimate.
///
/// See the module docs for the full algorithm and scientific grounding.
pub struct AdaptiveExecutor {
    /// The original plan being monitored. Stored for context (the executor
    /// knows which plan it's monitoring); not used by the divergence logic
    /// directly.
    plan: LogicalPlan,
    /// The estimated cardinality at each stage. Indexed by stage number.
    estimated_cardinalities: Vec<usize>,
    /// The observed cardinality at each stage. Updated by `observe`.
    observed_cardinalities: Vec<usize>,
    /// Whether each stage has been observed at least once. Stages that have
    /// not been observed are excluded from `max_divergence` to avoid false
    /// positives (an unobserved stage has `observed = 0`, which would
    /// otherwise register as a 100% underestimate).
    has_observed: Vec<bool>,
    /// Threshold for plan switching: if `max_divergence() > threshold`, a
    /// switch is recommended.
    switch_threshold: f64,
    /// Whether a plan switch has been triggered. Sticky — once `true`, stays
    /// `true` until the executor is dropped.
    switched: bool,
}

impl AdaptiveExecutor {
    /// Create a new adaptive executor monitoring `plan`, with the given
    /// `estimated` cardinalities per stage and `threshold` for switching.
    ///
    /// The `estimated` vector should have one entry per stage in the plan.
    /// Entries beyond `estimated.len()` cannot be observed (calls to
    /// [`observe`](Self::observe) with `stage >= estimated.len()` are
    /// silently ignored).
    ///
    /// `threshold` is the divergence ratio above which a switch is
    /// recommended. Typical values:
    /// - `0.5` — switch when the observed is >1.5× or <0.5× the estimate
    ///   (conservative; only flag gross misestimates).
    /// - `0.1` — switch when the observed is >1.1× or <0.9× the estimate
    ///   (aggressive; flags even small misestimates).
    pub fn new(plan: LogicalPlan, estimated: Vec<usize>, threshold: f64) -> Self {
        let n = estimated.len();
        Self {
            plan,
            estimated_cardinalities: estimated,
            observed_cardinalities: vec![0; n],
            has_observed: vec![false; n],
            switch_threshold: threshold,
            switched: false,
        }
    }

    /// Called after each morsel is processed at `stage`. Records the observed
    /// cardinality and returns `true` if a plan switch is recommended.
    ///
    /// The switch recommendation is sticky: once `true`, it stays `true` for
    /// all subsequent calls (until the executor is dropped or reset).
    ///
    /// If `stage` is out of bounds (`>= estimated.len()`), the call is a
    /// no-op and returns the current `switched` value.
    pub fn observe(&mut self, stage: usize, observed: usize) -> bool {
        if stage < self.observed_cardinalities.len() {
            self.observed_cardinalities[stage] = observed;
            self.has_observed[stage] = true;
        }
        if self.max_divergence() > self.switch_threshold {
            self.switched = true;
        }
        self.switched
    }

    /// Returns whether the plan should be switched. Sticky: once `true`, stays
    /// `true`.
    pub fn should_switch(&self) -> bool {
        self.switched
    }

    /// Returns the divergence ratio: `max` over all **observed** stages of
    /// `|observed - estimated| / max(estimated, 1)`.
    ///
    /// Stages that have not been observed (no `observe` call yet) are
    /// excluded from the max — an unobserved stage has `observed = 0`, which
    /// would otherwise register as a 100% underestimate and trigger false
    /// switches.
    ///
    /// For an estimated cardinality of 0:
    /// - If observed is also 0, divergence is 0 (correct estimate).
    /// - If observed is N > 0, divergence is `N` (the estimate missed every
    ///   row — divergence equals the observed count).
    pub fn max_divergence(&self) -> f64 {
        self.estimated_cardinalities
            .iter()
            .zip(self.observed_cardinalities.iter())
            .zip(self.has_observed.iter())
            .filter(|(_, &observed)| observed)
            .map(|((&est, &obs), _)| divergence(est, obs))
            .fold(0.0_f64, f64::max)
    }

    /// The switch threshold configured at construction.
    pub fn switch_threshold(&self) -> f64 {
        self.switch_threshold
    }

    /// The number of stages being monitored.
    pub fn stage_count(&self) -> usize {
        self.estimated_cardinalities.len()
    }

    /// Borrow the estimated cardinality at `stage`, or `None` if out of
    /// bounds.
    pub fn estimated(&self, stage: usize) -> Option<usize> {
        self.estimated_cardinalities.get(stage).copied()
    }

    /// Borrow the observed cardinality at `stage`, or `None` if out of
    /// bounds or not yet observed. Returns `Some(0)` only if the stage was
    /// explicitly observed with cardinality 0.
    pub fn observed(&self, stage: usize) -> Option<usize> {
        if *self.has_observed.get(stage)? {
            Some(self.observed_cardinalities[stage])
        } else {
            None
        }
    }

    /// Borrow the original plan being monitored.
    pub fn plan(&self) -> &LogicalPlan {
        &self.plan
    }
}

impl std::fmt::Debug for AdaptiveExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdaptiveExecutor")
            .field("stage_count", &self.estimated_cardinalities.len())
            .field("switch_threshold", &self.switch_threshold)
            .field("switched", &self.switched)
            .field("max_divergence", &self.max_divergence())
            .finish()
    }
}

/// Compute the divergence ratio for a single stage:
/// `|observed - estimated| / max(estimated, 1)`.
///
/// See [`AdaptiveExecutor::max_divergence`] for the edge-case handling
/// (estimated = 0).
fn divergence(estimated: usize, observed: usize) -> f64 {
    if estimated == 0 {
        if observed == 0 {
            0.0
        } else {
            // Estimate was 0 but we observed N rows — the estimate missed
            // every row. Divergence equals the observed count (an infinite
            // ratio would be more accurate but harder to compare against a
            // threshold; using the observed count is a reasonable
            // approximation that flags the misestimate).
            observed as f64
        }
    } else {
        let diff = (observed as i64 - estimated as i64).unsigned_abs() as f64;
        diff / estimated as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::plan::{LogicalPlan, PlanNode};
    use crate::kernel::{KernelParams, Operator};

    /// Build a trivial LogicalPlan for testing (a single Scan node).
    fn test_plan() -> LogicalPlan {
        LogicalPlan::new(PlanNode::Scan {
            region_id: 0,
            operator: Operator::ScanEqU64,
            params: KernelParams::default(),
        })
    }

    // -----------------------------------------------------------------------
    // DoD test 5: AdaptiveExecutor divergence detection triggers switch at
    // threshold.
    // -----------------------------------------------------------------------
    #[test]
    fn adaptive_divergence_triggers_switch_at_threshold() {
        // Estimated 100, observed 1000 → divergence = 9.0. With threshold
        // 0.5, this should trigger a switch.
        let plan = test_plan();
        let mut exec = AdaptiveExecutor::new(plan, vec![100], 0.5);

        assert!(!exec.should_switch(), "no switch before any observation");

        let switched = exec.observe(0, 1000);
        assert!(switched, "10x misestimate should trigger a switch");
        assert!(exec.should_switch());

        let div = exec.max_divergence();
        assert!((div - 9.0).abs() < 1e-9, "divergence should be 9.0 (|1000-100|/100), got {div}");
    }

    // -----------------------------------------------------------------------
    // DoD test 6: AdaptiveExecutor no switch when estimates are accurate.
    // -----------------------------------------------------------------------
    #[test]
    fn adaptive_no_switch_when_estimates_accurate() {
        // Estimated 100, observed 105 → divergence = 0.05. With threshold
        // 0.5, this should NOT trigger a switch.
        let plan = test_plan();
        let mut exec = AdaptiveExecutor::new(plan, vec![100], 0.5);

        let switched = exec.observe(0, 105);
        assert!(!switched, "5% misestimate should not trigger a switch");
        assert!(!exec.should_switch());

        let div = exec.max_divergence();
        assert!((div - 0.05).abs() < 1e-9, "divergence should be 0.05 (|105-100|/100), got {div}");
    }

    // -----------------------------------------------------------------------
    // Additional tests for edge cases and the divergence metric.
    // -----------------------------------------------------------------------

    #[test]
    fn adaptive_switch_is_sticky_once_triggered() {
        // Once switched, subsequent observations (even accurate ones) should
        // not un-switch.
        let plan = test_plan();
        let mut exec = AdaptiveExecutor::new(plan, vec![100], 0.5);

        // First: trigger a switch.
        let s1 = exec.observe(0, 1000);
        assert!(s1);

        // Then: observe an accurate cardinality.
        let s2 = exec.observe(0, 100);
        assert!(s2, "switch should stay sticky after being triggered");
        assert!(exec.should_switch());
    }

    #[test]
    fn adaptive_max_divergence_takes_max_across_stages() {
        // Two stages: stage 0 has divergence 0.1, stage 1 has divergence 2.0.
        // max_divergence should return 2.0.
        let plan = test_plan();
        let mut exec = AdaptiveExecutor::new(plan, vec![100, 50], 0.5);

        exec.observe(0, 110); // divergence = 0.1
        exec.observe(1, 150); // divergence = 2.0

        let div = exec.max_divergence();
        assert!((div - 2.0).abs() < 1e-9, "max divergence should be 2.0, got {div}");
    }

    #[test]
    fn adaptive_zero_estimated_zero_observed_is_zero_divergence() {
        let plan = test_plan();
        let mut exec = AdaptiveExecutor::new(plan, vec![0], 0.5);
        exec.observe(0, 0);
        assert!((exec.max_divergence() - 0.0).abs() < 1e-9);
        assert!(!exec.should_switch());
    }

    #[test]
    fn adaptive_zero_estimated_nonzero_observed_is_large_divergence() {
        // Estimated 0, observed 100 → divergence = 100.0 (the observed
        // count, since dividing by max(0, 1) = 1).
        let plan = test_plan();
        let mut exec = AdaptiveExecutor::new(plan, vec![0], 0.5);
        exec.observe(0, 100);
        let div = exec.max_divergence();
        assert!((div - 100.0).abs() < 1e-9, "divergence should be 100.0, got {div}");
        assert!(exec.should_switch());
    }

    #[test]
    fn adaptive_observe_out_of_bounds_stage_is_noop() {
        let plan = test_plan();
        let mut exec = AdaptiveExecutor::new(plan, vec![100], 0.5);
        // Stage 5 is out of bounds (only stage 0 exists). Should be a no-op.
        let switched = exec.observe(5, 1000);
        assert!(!switched, "out-of-bounds observation should be a no-op");
        assert!(!exec.should_switch());
    }

    #[test]
    fn adaptive_underestimate_triggers_switch() {
        // Estimated 1000, observed 100 → divergence = 0.9 (90% underestimate).
        // With threshold 0.5, this should trigger a switch.
        let plan = test_plan();
        let mut exec = AdaptiveExecutor::new(plan, vec![1000], 0.5);
        let switched = exec.observe(0, 100);
        assert!(switched, "90% underestimate should trigger a switch");
        let div = exec.max_divergence();
        assert!((div - 0.9).abs() < 1e-9, "divergence should be 0.9, got {div}");
    }

    #[test]
    fn adaptive_exact_match_is_zero_divergence() {
        let plan = test_plan();
        let mut exec = AdaptiveExecutor::new(plan, vec![100, 200, 300], 0.5);
        exec.observe(0, 100);
        exec.observe(1, 200);
        exec.observe(2, 300);
        assert!((exec.max_divergence() - 0.0).abs() < 1e-9);
        assert!(!exec.should_switch());
    }

    #[test]
    fn adaptive_threshold_zero_triggers_on_any_misestimate() {
        // threshold = 0 → any non-zero divergence triggers.
        let plan = test_plan();
        let mut exec = AdaptiveExecutor::new(plan, vec![100], 0.0);
        exec.observe(0, 101); // divergence = 0.01
        assert!(exec.should_switch(), "threshold 0 should trigger on any misestimate");
    }

    #[test]
    fn adaptive_threshold_large_never_triggers() {
        // threshold = 1e9 → effectively never triggers.
        let plan = test_plan();
        let mut exec = AdaptiveExecutor::new(plan, vec![100], 1e9);
        exec.observe(0, 1_000_000); // divergence = 9999
        assert!(!exec.should_switch(), "huge threshold should not trigger");
    }

    #[test]
    fn adaptive_stage_count_matches_estimated_length() {
        let plan = test_plan();
        let exec = AdaptiveExecutor::new(plan, vec![10, 20, 30], 0.5);
        assert_eq!(exec.stage_count(), 3);
    }

    #[test]
    fn adaptive_estimated_and_observed_accessors() {
        let plan = test_plan();
        let mut exec = AdaptiveExecutor::new(plan, vec![10, 20], 0.5);
        assert_eq!(exec.estimated(0), Some(10));
        assert_eq!(exec.estimated(1), Some(20));
        assert_eq!(exec.estimated(2), None);

        exec.observe(0, 15);
        assert_eq!(exec.observed(0), Some(15));
        assert_eq!(exec.observed(1), None); // not yet observed
    }

    #[test]
    fn adaptive_plan_accessor_returns_original_plan() {
        let plan = test_plan();
        let exec = AdaptiveExecutor::new(plan, vec![10], 0.5);
        assert!(matches!(exec.plan().root, PlanNode::Scan { .. }));
    }

    #[test]
    fn adaptive_switch_threshold_accessor() {
        let plan = test_plan();
        let exec = AdaptiveExecutor::new(plan, vec![10], 0.42);
        assert!((exec.switch_threshold() - 0.42).abs() < 1e-9);
    }

    #[test]
    fn adaptive_debug_format_works() {
        let plan = test_plan();
        let exec = AdaptiveExecutor::new(plan, vec![10, 20], 0.5);
        let s = format!("{exec:?}");
        assert!(s.contains("AdaptiveExecutor"));
        assert!(s.contains("stage_count"));
        assert!(s.contains("switch_threshold"));
        assert!(s.contains("switched"));
        assert!(s.contains("max_divergence"));
    }

    #[test]
    fn divergence_function_handles_typical_cases() {
        // Exact match.
        assert!((divergence(100, 100) - 0.0).abs() < 1e-9);
        // Overestimate.
        assert!((divergence(100, 150) - 0.5).abs() < 1e-9);
        // Underestimate.
        assert!((divergence(100, 50) - 0.5).abs() < 1e-9);
        // 10x overestimate.
        assert!((divergence(100, 1000) - 9.0).abs() < 1e-9);
        // 10x underestimate.
        assert!((divergence(1000, 100) - 0.9).abs() < 1e-9);
        // Zero estimated, zero observed.
        assert!((divergence(0, 0) - 0.0).abs() < 1e-9);
        // Zero estimated, nonzero observed.
        assert!((divergence(0, 50) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn adaptive_10x_misestimate_triggers_switch_at_threshold_1() {
        // The benchmark scenario: cardinality estimate 10x off.
        // divergence = |1000 - 100| / 100 = 9.0.
        // With threshold 1.0 (100% off), 9.0 > 1.0 → switch.
        let plan = test_plan();
        let mut exec = AdaptiveExecutor::new(plan, vec![100], 1.0);
        let switched = exec.observe(0, 1000);
        assert!(switched, "10x misestimate should trigger switch at threshold 1.0");
        assert!((exec.max_divergence() - 9.0).abs() < 1e-9);
    }
}
