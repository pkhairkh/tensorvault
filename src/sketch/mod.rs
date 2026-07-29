//! # Sketch — approximate, mergeable summaries.
//!
//! Probabilistic data structures for streaming / column statistics
//! (ADR-015). Every structure in this module is:
//!
//! - **One-pass** — a single update touches a bounded number of words.
//! - **Mergeable** — `a.merge(b)` produces a sketch equivalent to having
//!   observed `a`'s stream then `b`'s stream.
//! - **Sublinear space** — relative to the cardinality they summarise.
//!
//! ## Members
//!
//! - [`hll`] — HyperLogLog for distinct-count estimation (RSE ≈ 1.04/√m).
//! - [`count_min`] — Count-Min sketch for heavy-hitter / frequency
//!   estimation. Never underestimates.
//! - [`tdigest`] — t-Digest for streaming quantile estimation. This is a
//!   simplified merging-centroid implementation tuned for correctness over
//!   raw throughput.

pub mod count_min;
pub mod hll;
pub mod tdigest;
