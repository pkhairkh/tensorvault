//! Hybrid Logical Clock (HLC) — ADR-014.
//!
//! A Hybrid Logical Clock combines physical time (typically PTP-synced, with
//! sub-microsecond accuracy inside a CXL fabric or a single data center) with
//! a Lamport-style logical counter. The result is a timestamp that:
//!
//! 1. **Tracks physical time** when it advances (so timestamps are within a
//!    few µs of wall-clock time, useful for human-facing debug).
//! 2. **Strictly monotonic** even when physical time hasn't advanced between
//!    two calls (so two transactions that commit in the same nanosecond get
//!    distinct timestamps — necessary for snapshot isolation).
//! 3. **Totally ordered** — `(physical_ns, logical)` ordered lexicographically
//!    gives a total order with no ties (assuming single-threaded access to the
//!    clock, which the `&mut self` API enforces).
//! 4. **Causality-preserving across nodes** — after `observe(remote)`, the
//!    local clock's new timestamp is strictly greater than both the local
//!    clock's previous value and the remote timestamp (modulo clock skew,
//!    which the logical counter absorbs).
//!
//! ## ADR-014 rationale
//!
//! Pure physical time (e.g. `std::time::SystemTime::now()`) is not monotonic:
//! NTP can step the clock backwards, and two commits in the same nanosecond
//! would tie. Pure Lamport clocks are monotonic and totally ordered, but
//! diverge arbitrarily far from wall-clock time on a busy system.
//!
//! HLC is the standard compromise: keep physical time when it advances, and
//! use the logical counter as a tie-breaker when it doesn't. The counter is
//! reset to 0 whenever physical time advances.
//!
//! ## Why not `AtomicU64`?
//!
//! A real production HLC would use a `seqlock` or compare-and-swap loop to
//! make `now()` / `observe()` lock-free and multi-threaded. This prototype
//! uses `&mut self` to enforce single-threaded access from a single
//! coordinator — the coordinator itself is the serialization point. A future
//! wave could swap the interior mutability for an `AtomicU64` pair without
//! changing the API.

use std::time::{SystemTime, UNIX_EPOCH};

/// A Hybrid Logical Clock timestamp.
///
/// Totally ordered by `(physical_ns, logical)` — the derived `Ord` impl
/// compares `physical_ns` first, then `logical` as a tie-breaker. Two
/// `HlcTimestamp`s produced by the same [`HlcClock`] are never equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HlcTimestamp {
    /// Physical time in nanoseconds since the UNIX epoch.
    pub physical_ns: u64,
    /// Logical counter for tie-breaking when physical time hasn't advanced.
    pub logical: u64,
}

impl HlcTimestamp {
    /// The zero timestamp — useful as a sentinel "before all real timestamps"
    /// value. Real timestamps produced by [`HlcClock::now`] always have
    /// `physical_ns >= 1` (since 1970 is in the distant past), so the zero
    /// timestamp is strictly less than any real one.
    pub const ZERO: Self = Self { physical_ns: 0, logical: 0 };

    /// Render as `phys.<physical_ns>+<logical>` for debug logs.
    pub fn debug_string(&self) -> String {
        format!("phys.{}+{}", self.physical_ns, self.logical)
    }
}

impl std::fmt::Display for HlcTimestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}+{}", self.physical_ns, self.logical)
    }
}

/// HLC clock that combines physical time (PTP-synced) with a Lamport counter.
///
/// See the [module docs](self) for the design rationale.
#[derive(Debug)]
pub struct HlcClock {
    /// Last issued timestamp. The next `now()` / `observe()` call must
    /// produce a strictly greater timestamp than this.
    last: HlcTimestamp,
}

impl HlcClock {
    /// Create a new HLC clock starting at the zero timestamp.
    ///
    /// The first `now()` call will produce a timestamp at the current
    /// physical time with `logical = 0`.
    pub fn new() -> Self {
        Self { last: HlcTimestamp::ZERO }
    }

    /// Generate a new timestamp.
    ///
    /// If physical time has advanced past `last.physical_ns`, use the new
    /// physical time and reset the logical counter to 0. Otherwise, increment
    /// the logical counter (keeping the old physical time).
    ///
    /// The returned timestamp is strictly greater than the previous one.
    pub fn now(&mut self) -> HlcTimestamp {
        let pt = Self::physical_now();
        if pt > self.last.physical_ns {
            // Physical time advanced — reset the logical counter.
            self.last = HlcTimestamp { physical_ns: pt, logical: 0 };
        } else {
            // Physical time hasn't advanced (or stepped backwards, which
            // we treat as "didn't advance"). Increment the logical counter
            // to preserve monotonicity.
            self.last =
                HlcTimestamp { physical_ns: self.last.physical_ns, logical: self.last.logical + 1 };
        }
        self.last
    }

    /// Update the clock after receiving a timestamp from another node.
    ///
    /// Per ADR-014:
    ///
    /// - If the received physical time > local, adopt it and set
    ///   `logical = received.logical + 1`.
    /// - If equal, take the max of the logical counters and increment.
    /// - If less, increment the local logical counter (keep local physical).
    ///
    /// In all three cases the returned timestamp is strictly greater than
    /// both the previous local timestamp and the received timestamp — this
    /// is what makes the HLC causality-preserving.
    pub fn observe(&mut self, other: &HlcTimestamp) -> HlcTimestamp {
        if other.physical_ns > self.last.physical_ns {
            // Remote is ahead — adopt its physical time and bump its logical.
            self.last = HlcTimestamp { physical_ns: other.physical_ns, logical: other.logical + 1 };
        } else if other.physical_ns == self.last.physical_ns {
            // Same physical time — take max of logical counters and increment.
            self.last = HlcTimestamp {
                physical_ns: self.last.physical_ns,
                logical: std::cmp::max(self.last.logical, other.logical) + 1,
            };
        } else {
            // Remote is behind — just increment local logical counter.
            self.last =
                HlcTimestamp { physical_ns: self.last.physical_ns, logical: self.last.logical + 1 };
        }
        self.last
    }

    /// Get the current physical time in nanoseconds since the UNIX epoch.
    ///
    /// Returns 0 if the system clock is before the UNIX epoch (which should
    /// never happen on a functioning system, but defensive programming is
    /// cheap).
    fn physical_now() -> u64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(dur) => dur.as_nanos() as u64,
            Err(_) => 0,
        }
    }

    /// Peek at the last-issued timestamp without advancing the clock.
    ///
    /// Mainly useful for tests. Production code should use [`Self::now`].
    pub fn last(&self) -> HlcTimestamp {
        self.last
    }
}

impl Default for HlcClock {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Test 1: `now()` returns monotonically increasing timestamps.
    ///
    /// Issues 1000 timestamps in a tight loop and asserts each is strictly
    /// greater than the previous. Since `physical_now()` has nanosecond
    /// resolution but the loop body is faster than 1 ns, most iterations
    /// will exercise the "physical time hasn't advanced" branch and bump
    /// the logical counter.
    #[test]
    fn hlc_now_is_monotonic() {
        let mut clock = HlcClock::new();
        let mut prev = clock.now();
        for _ in 0..1000 {
            let next = clock.now();
            assert!(next > prev, "non-monotonic: {prev:?} -> {next:?}");
            prev = next;
        }
    }

    /// Test 2: `observe()` with a higher physical time adopts it.
    ///
    /// Local clock is at (100, 5). Observing (200, 3) — which has higher
    /// physical time — must produce a timestamp with physical_ns == 200
    /// and logical == 4 (received.logical + 1).
    #[test]
    fn hlc_observe_adopts_higher_physical() {
        let mut clock = HlcClock::new();
        // Force the clock to a known state by observing a sentinel.
        clock.observe(&HlcTimestamp { physical_ns: 100, logical: 5 });
        assert_eq!(clock.last(), HlcTimestamp { physical_ns: 100, logical: 6 });

        // Now observe a remote timestamp with higher physical time.
        let remote = HlcTimestamp { physical_ns: 200, logical: 3 };
        let observed = clock.observe(&remote);
        assert_eq!(
            observed,
            HlcTimestamp { physical_ns: 200, logical: 4 },
            "observe(higher_pt) must adopt remote physical and bump remote logical"
        );
        assert_eq!(clock.last(), observed);
    }

    /// Test 3: `observe()` with same physical time increments logical.
    ///
    /// Local clock is at (100, 5). Observing (100, 3) — same physical time,
    /// lower logical — must produce (100, 6) (max(5,3)+1 = 6).
    /// Then observing (100, 9) — same physical, higher logical — must
    /// produce (100, 10) (max(6,9)+1 = 10).
    #[test]
    fn hlc_observe_same_physical_takes_max_logical() {
        let mut clock = HlcClock::new();
        clock.observe(&HlcTimestamp { physical_ns: 100, logical: 5 });
        assert_eq!(clock.last(), HlcTimestamp { physical_ns: 100, logical: 6 });

        // Lower remote logical: max(6, 3) + 1 = 7
        let observed = clock.observe(&HlcTimestamp { physical_ns: 100, logical: 3 });
        assert_eq!(observed, HlcTimestamp { physical_ns: 100, logical: 7 });

        // Higher remote logical: max(7, 9) + 1 = 10
        let observed = clock.observe(&HlcTimestamp { physical_ns: 100, logical: 9 });
        assert_eq!(observed, HlcTimestamp { physical_ns: 100, logical: 10 });
    }

    /// Test 3b: `observe()` with lower physical time increments local logical.
    ///
    /// Local clock is at (200, 5). Observing (100, 99) — lower physical,
    /// higher logical — must produce (200, 6) (keep local physical,
    /// increment local logical). The remote's logical is irrelevant when
    /// its physical time is lower.
    #[test]
    fn hlc_observe_lower_physical_increments_local_logical() {
        let mut clock = HlcClock::new();
        clock.observe(&HlcTimestamp { physical_ns: 200, logical: 5 });
        assert_eq!(clock.last(), HlcTimestamp { physical_ns: 200, logical: 6 });

        let observed = clock.observe(&HlcTimestamp { physical_ns: 100, logical: 99 });
        assert_eq!(observed, HlcTimestamp { physical_ns: 200, logical: 7 });
    }

    /// Test 4: timestamps are totally ordered — no ties, even across
    /// interleaved `now()` and `observe()` calls.
    ///
    /// Issues a mix of `now()` and `observe(remote)` calls, then asserts the
    /// collected timestamps are strictly increasing (no duplicates).
    #[test]
    fn hlc_timestamps_are_totally_ordered() {
        let mut clock = HlcClock::new();
        let mut timestamps: Vec<HlcTimestamp> = Vec::new();

        for i in 0..50 {
            timestamps.push(clock.now());
            // Interleave observe() with a remote timestamp that's sometimes
            // higher, sometimes lower, sometimes equal to the local clock.
            let remote = HlcTimestamp {
                physical_ns: clock.last().physical_ns.saturating_add(i % 7),
                logical: i % 3,
            };
            timestamps.push(clock.observe(&remote));
        }

        // Check strict monotonic increase — no ties allowed.
        for w in timestamps.windows(2) {
            assert!(w[1] > w[0], "tie or regression: {:?} -> {:?}", w[0], w[1]);
        }
    }

    /// Test 5: `HlcTimestamp::ZERO` is strictly less than any real timestamp
    /// produced by `now()`. Useful as a sentinel for "uninitialized".
    #[test]
    fn hlc_zero_is_below_real_timestamps() {
        let mut clock = HlcClock::new();
        let ts = clock.now();
        assert!(ts > HlcTimestamp::ZERO, "real timestamp must be > ZERO");
    }

    /// Test 6: `physical_now()` returns a sane value (non-zero on any
    /// functioning system, since the UNIX epoch was 56+ years ago).
    #[test]
    fn hlc_physical_now_is_nonzero() {
        let pt = HlcClock::physical_now();
        // As of 2026, the UNIX epoch in nanoseconds is ~1.7e18. Any value
        // above 1e18 is "obviously sane". Below 1e15 (year 2001) would
        // indicate a broken clock.
        assert!(pt > 1_000_000_000_000_000, "physical_now() = {pt} is implausibly small");
    }

    /// Test 7: `Display` and `debug_string` produce sensible output.
    #[test]
    fn hlc_timestamp_display() {
        let ts = HlcTimestamp { physical_ns: 1234, logical: 5 };
        assert_eq!(ts.to_string(), "1234+5");
        assert_eq!(ts.debug_string(), "phys.1234+5");
    }

    /// Test 8: `Default` impl is equivalent to `new()`.
    #[test]
    fn hlc_default_equals_new() {
        let a = HlcClock::new();
        let b = HlcClock::default();
        assert_eq!(a.last(), b.last());
        assert_eq!(a.last(), HlcTimestamp::ZERO);
    }
}
