//! Calibration loop for the learned cardinality estimator.
//!
//! The calibration loop is the online-learning driver for
//! [`LearnedCardinality`]. After each query executes, the runtime records
//! the (predicted, actual) cardinality pair; the loop updates the
//! estimator's correction factor and tracks the running mean absolute
//! percentage error (MAPE) of the predictions.
//!
//! ## Lifecycle
//!
//! 1. **Construct** a [`CalibrationLoop`] from a (possibly pre-trained)
//!    [`LearnedCardinality`].
//! 2. **Before** each query, read selectivity estimates from
//!    `estimator.estimate_selectivity(...)` and apply
//!    `estimator.correct(estimate)` to get the corrected prediction.
//! 3. **After** each query, call [`CalibrationLoop::record`] with the
//!    `(predicted, actual)` pair. This appends to
//!    [`LearnedCardinality::observations`] and updates the correction
//!    factor via [`LearnedCardinality::observe`].
//! 4. **Periodically**, call [`CalibrationLoop::mape`] to check the
//!    estimator's accuracy. If MAPE drifts above a threshold (e.g., 20 %),
//!    retrain the histograms from a fresh table sample.
//!
//! ## MAPE definition
//!
//! The MAPE is the mean absolute percentage error of the **raw**
//! predictions (before correction):
//!
//! ```text
//! MAPE = (1/n) · Σ |actual_i - predicted_i| / max(actual_i, 1.0)
//! ```
//!
//! The `max(actual, 1.0)` guard prevents divide-by-zero on empty results
//! (an actual cardinality of 0 is treated as 1 for the ratio, which
//! bounds the per-observation error at `predicted`).
//!
//! The correction factor does **not** enter the MAPE computation — MAPE
//! measures the *raw* prediction quality so the runtime can decide when
//! to retrain the histograms (a high MAPE means the histograms are stale,
//! not that the correction is wrong).
//!
//! ## Convergence
//!
//! The correction factor converges exponentially to the true
//! `actual/predicted` ratio under repeated observations. With smoothing
//! factor `α = 0.1`, the half-life is `log(0.5) / log(0.9) ≈ 6.6`
//! observations — so after ~7 observations the correction has moved
//! halfway from its initial value (1.0) to the true ratio. After ~30
//! observations the correction is within 5 % of its steady state.

use crate::planner::learned::LearnedCardinality;

/// A calibration loop wrapping a [`LearnedCardinality`] estimator.
///
/// See the [module docs](self) for the full design.
///
/// # Example
///
/// ```
/// use turbogp::planner::calibration::CalibrationLoop;
/// use turbogp::planner::learned::LearnedCardinality;
///
/// let mut loop_ = CalibrationLoop::new(LearnedCardinality::new());
/// assert!((loop_.correction() - 1.0).abs() < 1e-9);
///
/// // Record 10 biased observations: actual = 2 × predicted.
/// for _ in 0..10 {
///     loop_.record(100.0, 200.0);
/// }
/// // correction moves toward 2.0 (but not yet there — α = 0.1).
/// assert!(loop_.correction() > 1.5, "correction = {}", loop_.correction());
/// assert!(loop_.correction() < 2.0);
/// // MAPE of the raw predictions = 50 % (every prediction was 50 % below
/// // actual: |200 - 100| / 200 = 0.5).
/// assert!((loop_.mape() - 0.5).abs() < 1e-9);
/// ```
pub struct CalibrationLoop {
    /// The wrapped learned estimator.
    estimator: LearnedCardinality,
    /// Number of observations recorded so far.
    observations: usize,
}

impl CalibrationLoop {
    /// Construct a new calibration loop wrapping `estimator`.
    ///
    /// The estimator's existing histograms and correction factor are
    /// preserved — this allows constructing a pre-trained estimator
    /// (e.g., from a saved ANALYZE snapshot) and then continuing to
    /// calibrate it online.
    #[must_use]
    pub fn new(estimator: LearnedCardinality) -> Self {
        Self { estimator, observations: 0 }
    }

    /// Record a `(predicted, actual)` cardinality pair.
    ///
    /// Appends the pair to [`LearnedCardinality::observations`] (for
    /// offline MAPE analysis) and updates the correction factor via
    /// [`LearnedCardinality::observe`].
    pub fn record(&mut self, predicted: f64, actual: f64) {
        self.estimator.observe(predicted, actual);
        self.observations += 1;
    }

    /// Returns the current correction factor (a snapshot of
    /// [`LearnedCardinality::correction`]).
    #[must_use]
    pub fn correction(&self) -> f64 {
        self.estimator.correction
    }

    /// Returns the mean absolute percentage error (MAPE) of all recorded
    /// observations.
    ///
    /// MAPE = `(1/n) · Σ |actual - predicted| / max(actual, 1.0)`.
    ///
    /// Returns `0.0` if no observations have been recorded.
    #[must_use]
    pub fn mape(&self) -> f64 {
        if self.estimator.observations.is_empty() {
            return 0.0;
        }
        let mut sum_ape = 0.0;
        for &(p, a) in &self.estimator.observations {
            sum_ape += (a - p).abs() / a.max(1.0);
        }
        sum_ape / self.estimator.observations.len() as f64
    }

    /// Returns the number of observations recorded so far.
    ///
    /// This equals `self.estimator.observations.len()`; the counter is
    /// kept separately to avoid a HashMap lookup on every record.
    #[must_use]
    pub fn observation_count(&self) -> usize {
        self.observations
    }

    /// Borrow the wrapped learned estimator (read-only).
    ///
    /// Useful for inspecting histograms or querying selectivity
    /// estimates during calibration.
    #[must_use]
    pub fn estimator(&self) -> &LearnedCardinality {
        &self.estimator
    }

    /// Borrow the wrapped learned estimator (mutable).
    ///
    /// Useful for training histograms mid-calibration (e.g., when MAPE
    /// drifts above a threshold and a fresh `ANALYZE` is triggered).
    pub fn estimator_mut(&mut self) -> &mut LearnedCardinality {
        &mut self.estimator
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `new()` preserves the wrapped estimator's state.
    #[test]
    fn new_preserves_estimator_state() {
        let mut est = LearnedCardinality::new();
        est.correction = 1.5;
        let values: Vec<u64> = (0..100).collect();
        est.train_table("t", "c", &values);
        let cl = CalibrationLoop::new(est);
        assert!((cl.correction() - 1.5).abs() < 1e-9);
        assert!(cl.estimator().histograms.contains_key(&("t".into(), "c".into())));
    }

    /// `record()` increments the observation count and updates the
    /// correction.
    #[test]
    fn record_increments_count_and_updates_correction() {
        let mut cl = CalibrationLoop::new(LearnedCardinality::new());
        assert_eq!(cl.observation_count(), 0);
        cl.record(100.0, 200.0);
        assert_eq!(cl.observation_count(), 1);
        // correction = 0.9 · 1.0 + 0.1 · 2.0 = 1.1.
        assert!((cl.correction() - 1.1).abs() < 1e-9);
    }

    /// `correction()` reflects the current correction factor.
    #[test]
    fn correction_reflects_current_value() {
        let mut cl = CalibrationLoop::new(LearnedCardinality::new());
        assert!((cl.correction() - 1.0).abs() < 1e-9);
        for _ in 0..200 {
            cl.record(100.0, 50.0);
        }
        assert!(
            (cl.correction() - 0.5).abs() < 0.01,
            "correction after many biased observations = {}, expected ~0.5",
            cl.correction()
        );
    }

    /// `mape()` returns 0.0 with no observations.
    #[test]
    fn mape_empty_returns_zero() {
        let cl = CalibrationLoop::new(LearnedCardinality::new());
        assert_eq!(cl.mape(), 0.0);
    }

    /// `mape()` for a single observation = `|actual - predicted| / actual`.
    #[test]
    fn mape_single_observation() {
        let mut cl = CalibrationLoop::new(LearnedCardinality::new());
        cl.record(100.0, 200.0);
        // MAPE = |200 - 100| / 200 = 0.5.
        assert!((cl.mape() - 0.5).abs() < 1e-9, "mape = {}", cl.mape());
    }

    /// MAPE: after 100 observations with 10 % noise, MAPE < 15 %.
    ///
    /// DoD: MAPE: after 100 observations with 10 % noise, MAPE < 15 %.
    #[test]
    fn mape_after_100_observations_with_10pct_noise_is_under_15pct() {
        let mut state: u64 = 0xCAFE_BABE_DEAD_BEEF;
        let mut cl = CalibrationLoop::new(LearnedCardinality::new());
        for _ in 0..100 {
            let actual = 100.0 + (next_f64(&mut state) * 900.0);
            let noise = (next_f64(&mut state) * 2.0 - 1.0) * 0.10;
            let predicted = actual * (1.0 + noise);
            cl.record(predicted, actual);
        }
        let mape = cl.mape();
        assert!(
            mape < 0.15,
            "MAPE after 100 observations with 10% noise = {:.4} ({}%), expected < 15%",
            mape,
            mape * 100.0
        );
    }

    /// `mape()` does not divide by zero when `actual = 0`.
    #[test]
    fn mape_handles_zero_actual() {
        let mut cl = CalibrationLoop::new(LearnedCardinality::new());
        cl.record(100.0, 0.0);
        // |0 - 100| / max(0, 1) = 100.
        assert!((cl.mape() - 100.0).abs() < 1e-9, "mape with actual=0 = {}", cl.mape());
    }

    /// `estimator_mut()` allows training histograms mid-calibration.
    #[test]
    fn estimator_mut_allows_training() {
        let mut cl = CalibrationLoop::new(LearnedCardinality::new());
        let values: Vec<u64> = (0..100).collect();
        cl.estimator_mut().train_table("t", "c", &values);
        let sel = cl.estimator().estimate_selectivity("t", "c", 50);
        assert!(sel > 0.0);
    }

    /// `observation_count()` matches the wrapped estimator's observation
    /// count.
    #[test]
    fn observation_count_matches_estimator() {
        let mut cl = CalibrationLoop::new(LearnedCardinality::new());
        for _ in 0..42 {
            cl.record(10.0, 10.0);
        }
        assert_eq!(cl.observation_count(), 42);
        assert_eq!(cl.estimator().observations.len(), 42);
    }

    /// Correction factor converges toward the true ratio under repeated
    /// biased observations (DoD).
    ///
    /// DoD: Correction factor converges toward the true ratio.
    #[test]
    fn correction_converges_to_true_ratio() {
        let mut cl = CalibrationLoop::new(LearnedCardinality::new());
        // True ratio = 4.0 (estimator always under-predicts by 4×).
        for _ in 0..500 {
            cl.record(25.0, 100.0);
        }
        assert!(
            (cl.correction() - 4.0).abs() < 0.01,
            "correction = {}, expected ~4.0",
            cl.correction()
        );
    }

    // -------------------------------------------------------------------------
    // PRNG helper (deterministic splitmix64 → f64 in [0, 1))
    // -------------------------------------------------------------------------

    /// Step a splitmix64 state and return the next `u64`.
    fn next_u64(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Convert a splitmix64 output to `f64` in `[0, 1)`.
    fn next_f64(state: &mut u64) -> f64 {
        let v = next_u64(state) >> 11;
        v as f64 / (1u64 << 53) as f64
    }
}
