//! Protocol boundary coordinator.
//!
//! The transaction coordinator runs at protocol boundaries:
//! - **Within a rack** (CXL 3.0 fabric): hardware coherence, ~250 ns commit
//! - **Across racks** (RoCEv2 / IB): software coherence via Raft, ~10 µs commit
//! - **Across regions**: async replication, ms-class
//!
//! Each coordinator issues [`HlcTimestamp`](clock::HlcTimestamp)s (ADR-014) so
//! that transactions are totally ordered even when physical time hasn't
//! advanced.

pub mod clock;
pub mod cxl;
pub mod raft;

pub use clock::{HlcClock, HlcTimestamp};
pub use cxl::CxlCoordinator;
pub use raft::RaftCoordinator;
