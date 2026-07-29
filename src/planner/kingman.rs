//! Kingman's queueing formula for admission control and tail-latency
//! prediction (ADR-020).
//!
//! A single-server queue with arrival rate λ, service rate μ, and coefficients
//! of variation `c_a` (arrival) and `c_s` (service) has predicted mean wait:
//!
//! ```text
//! W = (ρ / (1 − ρ)) · ((c_a² + c_s²) / 2) · (1/μ)
//! ```
//!
//! where `ρ = λ/μ` is utilization. Kingman's formula is exact for M/M/1 and a
//! tight upper bound for G/G/1, making it suitable for online admission
//! control: keep `ρ` below ~0.8 to bound p99 at ~2× the unloaded latency.
//!
//! This module implements [`KingmanPredictor`], which maintains online
//! estimates of `(λ, μ, c_a, c_s)` from a stream of `(arrival_interval,
//! service_time)` observations using Welford's algorithm for numerical
//! stability. The same predictor feeds both the planner's cost model (ADR-023)
//! and the executor's admission control (ADR-020).

/// An online Kingman's-formula predictor for a single-server queue.
///
/// The four public fields are the *current* estimates of `(λ, μ, c_a, c_s)`
/// and are the source of truth for `utilization()`, `predicted_wait()`, and
/// `predicted_p99()`. They are updated in place by [`update`](Self::update)
/// from observed `(arrival_interval, service_time)` pairs.
///
/// Construct with [`KingmanPredictor::new`] to seed the estimates, or with
/// [`KingmanPredictor::default`] for an idle (zero-rate) queue.
pub struct KingmanPredictor {
    /// Arrival rate (requests/sec). Updated by [`update`](Self::update).
    pub lambda: f64,
    /// Service rate (requests/sec). Updated by [`update`](Self::update).
    pub mu: f64,
    /// Coefficient of variation of arrival intervals (`stddev/mean`).
    pub c_a: f64,
    /// Coefficient of variation of service times (`stddev/mean`).
    pub c_s: f64,

    // ---- Welford running statistics (private) ----
    // These are kept in lockstep with the public fields above so that
    // `update()` can incrementally refine the estimates without storing the
    // full sample history. See
    // <https://en.wikipedia.org/wiki/Algorithms_for_calculating_variance#Welford's_online_algorithm>.
    /// Running mean of arrival intervals (seconds).
    arrival_mean: f64,
    /// Running M2 (sum of squared deviations from the mean) for arrival
    /// intervals. Divide by `n-1` to get the sample variance.
    arrival_m2: f64,
    /// Running mean of service times (seconds).
    service_mean: f64,
    /// Running M2 for service times.
    service_m2: f64,
    /// Number of observations seen so far.
    n: u64,
}

impl KingmanPredictor {
    /// Create a new predictor seeded with explicit `(λ, μ, c_a, c_s)` values.
    ///
    /// The first call to [`update`](Self::update) will overwrite these with
    /// the running statistics, so callers wanting a purely-static estimate
    /// (e.g. for a test or a cold-start prior) should simply not call
    /// `update`.
    #[must_use]
    pub fn new(lambda: f64, mu: f64, c_a: f64, c_s: f64) -> Self {
        Self {
            lambda,
            mu,
            c_a,
            c_s,
            arrival_mean: 0.0,
            arrival_m2: 0.0,
            service_mean: 0.0,
            service_m2: 0.0,
            n: 0,
        }
    }

    /// Current utilization `ρ = λ/μ`.
    ///
    /// Returns 0.0 if `μ ≤ 0` (no service capacity yet observed).
    #[must_use]
    pub fn utilization(&self) -> f64 {
        if self.mu > 0.0 {
            self.lambda / self.mu
        } else {
            0.0
        }
    }

    /// Predicted mean wait time (seconds) in queue, per Kingman's formula:
    ///
    /// ```text
    /// W = (ρ / (1 − ρ)) · ((c_a² + c_s²) / 2) · (1/μ)
    /// ```
    ///
    /// Returns `f64::INFINITY` if `ρ ≥ 1` (the queue is unstable and the
    /// wait grows without bound) and 0.0 if `ρ ≤ 0` (no load).
    #[must_use]
    pub fn predicted_wait(&self) -> f64 {
        let rho = self.utilization();
        if rho >= 1.0 {
            return f64::INFINITY;
        }
        if rho <= 0.0 || self.mu <= 0.0 {
            return 0.0;
        }
        let mean_service = 1.0 / self.mu;
        (rho / (1.0 - rho)) * ((self.c_a.powi(2) + self.c_s.powi(2)) / 2.0) * mean_service
    }

    /// Predicted p99 response time (seconds), using a lognormal approximation:
    ///
    /// ```text
    /// p99 ≈ mean_response · (1 + 2.33 · c_s)
    /// ```
    ///
    /// where `mean_response = W + 1/μ` is the mean sojourn time (wait +
    /// service). The factor `2.33` is the standard-normal 99th percentile.
    ///
    /// For small `c_s` this is a tight approximation; for large `c_s` it
    /// overestimates (the exact lognormal p99 uses
    /// `exp(2.33 · sqrt(ln(1+c_s²)))` which is bounded below 1.0 for
    /// moderate `c_s`).
    #[must_use]
    pub fn predicted_p99(&self) -> f64 {
        if self.mu <= 0.0 {
            return f64::INFINITY;
        }
        let mean_response = self.predicted_wait() + 1.0 / self.mu;
        mean_response * (1.0 + 2.33 * self.c_s)
    }

    /// Update `(λ, μ, c_a, c_s)` from a single observation using Welford's
    /// online algorithm.
    ///
    /// - `arrival_interval` — seconds since the previous arrival.
    /// - `service_time` — seconds the server spent serving this request.
    ///
    /// After the first call, `lambda` and `mu` reflect the single observed
    /// interval/service time (i.e. `1/arrival_interval`, `1/service_time`)
    /// and `c_a`, `c_s` are 0 (a single sample has no variance). After two
    /// or more calls, all four fields are proper sample estimates.
    pub fn update(&mut self, arrival_interval: f64, service_time: f64) {
        self.n = self.n.saturating_add(1);
        let n = self.n as f64;

        // Welford for arrival intervals.
        let delta_a = arrival_interval - self.arrival_mean;
        self.arrival_mean += delta_a / n;
        let delta_a2 = arrival_interval - self.arrival_mean;
        self.arrival_m2 += delta_a * delta_a2;

        // Welford for service times.
        let delta_s = service_time - self.service_mean;
        self.service_mean += delta_s / n;
        let delta_s2 = service_time - self.service_mean;
        self.service_m2 += delta_s * delta_s2;

        // Publish derived rates and coefficients of variation.
        if self.arrival_mean > 0.0 {
            self.lambda = 1.0 / self.arrival_mean;
            let var_a = if n > 1.0 { self.arrival_m2 / (n - 1.0) } else { 0.0 };
            self.c_a = if self.arrival_mean > 0.0 { var_a.sqrt() / self.arrival_mean } else { 0.0 };
        }

        if self.service_mean > 0.0 {
            self.mu = 1.0 / self.service_mean;
            let var_s = if n > 1.0 { self.service_m2 / (n - 1.0) } else { 0.0 };
            self.c_s = if self.service_mean > 0.0 { var_s.sqrt() / self.service_mean } else { 0.0 };
        }
    }
}

impl Default for KingmanPredictor {
    /// An idle predictor: zero arrival rate, unit service rate (so a single
    /// request takes 1 second — picked arbitrarily so `utilization()` is 0
    /// rather than `NaN`). Callers should call [`update`](Self::update) or
    /// [`new`](Self::new) before trusting the predictions.
    fn default() -> Self {
        Self::new(0.0, 1.0, 1.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ρ = λ/μ is computed correctly across a range of values.
    #[test]
    fn utilization_is_lambda_over_mu() {
        let k = KingmanPredictor::new(50.0, 100.0, 1.0, 1.0);
        assert!((k.utilization() - 0.5).abs() < 1e-9);

        let k = KingmanPredictor::new(99.0, 100.0, 1.0, 1.0);
        assert!((k.utilization() - 0.99).abs() < 1e-9);

        // μ = 0 → utilization 0 (no service capacity).
        let k = KingmanPredictor::new(10.0, 0.0, 1.0, 1.0);
        assert_eq!(k.utilization(), 0.0);
    }

    /// ρ = 0.5, c_a = c_s = 1, μ = 100 → W = (0.5/0.5) · 1 · 0.01 = 0.01 s.
    #[test]
    fn predicted_wait_rho_half_is_reasonable() {
        let k = KingmanPredictor::new(50.0, 100.0, 1.0, 1.0);
        let w = k.predicted_wait();
        // Should be 10 ms exactly with these inputs.
        assert!((w - 0.01).abs() < 1e-9, "expected ~10 ms, got {w}");
        // Sanity: a reasonable wait is positive and well under a second.
        assert!(w > 0.0);
        assert!(w < 1.0);
    }

    /// ρ = 0.99 produces a wait ~99× the ρ=0.5 wait (Kingman's
    /// `ρ/(1−ρ)` term goes 1 → 99).
    #[test]
    fn predicted_wait_rho_99_much_larger_than_rho_50() {
        let k_low = KingmanPredictor::new(50.0, 100.0, 1.0, 1.0);
        let k_high = KingmanPredictor::new(99.0, 100.0, 1.0, 1.0);
        let w_low = k_low.predicted_wait();
        let w_high = k_high.predicted_wait();
        assert!(w_high > 10.0 * w_low, "ρ=0.99 wait ({w_high}) should be ≫ ρ=0.5 wait ({w_low})");
        // Exact ratio: (0.99/0.01) / (0.5/0.5) = 99.
        assert!((w_high / w_low - 99.0).abs() < 1e-6);
    }

    /// ρ ≥ 1 → infinite wait (unstable queue).
    #[test]
    fn predicted_wait_saturates_at_unstable_load() {
        let k = KingmanPredictor::new(150.0, 100.0, 1.0, 1.0);
        assert!(k.predicted_wait().is_infinite());

        // Edge: ρ = 1 exactly.
        let k = KingmanPredictor::new(100.0, 100.0, 1.0, 1.0);
        assert!(k.predicted_wait().is_infinite());
    }

    /// p99 ≥ mean response time (it's the mean scaled by a factor ≥ 1).
    #[test]
    fn predicted_p99_exceeds_mean_response() {
        let k = KingmanPredictor::new(50.0, 100.0, 1.0, 1.0);
        let mean_response = k.predicted_wait() + 1.0 / k.mu;
        let p99 = k.predicted_p99();
        assert!(p99 > mean_response, "p99 ({p99}) should exceed mean response ({mean_response})");
        // With c_s = 1, factor = 1 + 2.33 = 3.33.
        assert!((p99 / mean_response - 3.33).abs() < 1e-6);
    }

    /// After many identical observations, c_a and c_s collapse to 0 (no
    /// variance) and λ, μ converge to the inverse of the observed interval.
    #[test]
    fn update_converges_to_constant_observation() {
        let mut k = KingmanPredictor::new(1.0, 1.0, 1.0, 1.0);
        for _ in 0..1000 {
            k.update(0.01, 0.005);
        }
        // λ → 1/0.01 = 100, μ → 1/0.005 = 200.
        assert!((k.lambda - 100.0).abs() < 1.0, "lambda = {}", k.lambda);
        assert!((k.mu - 200.0).abs() < 1.0, "mu = {}", k.mu);
        // No variance → c_a, c_s → 0.
        assert!(k.c_a < 0.01, "c_a = {}", k.c_a);
        assert!(k.c_s < 0.01, "c_s = {}", k.c_s);
    }

    /// With alternating intervals (0.01, 0.03), the predictor converges to
    /// mean = 0.02 (λ = 50) and stddev = 0.01 (c_a = 0.5).
    #[test]
    fn update_tracks_variance() {
        let mut k = KingmanPredictor::new(1.0, 1.0, 1.0, 1.0);
        for _ in 0..500 {
            k.update(0.01, 0.005);
            k.update(0.03, 0.005);
        }
        assert!((k.lambda - 50.0).abs() < 1.0, "lambda = {}", k.lambda);
        assert!((k.c_a - 0.5).abs() < 0.05, "c_a = {} (expected ~0.5)", k.c_a);
        // Service times are constant → c_s → 0.
        assert!(k.c_s < 0.01, "c_s = {}", k.c_s);
    }

    /// Default predictor has ρ = 0 (no load) and wait = 0.
    #[test]
    fn default_predictor_is_idle() {
        let k = KingmanPredictor::default();
        assert_eq!(k.utilization(), 0.0);
        assert_eq!(k.predicted_wait(), 0.0);
    }
}
