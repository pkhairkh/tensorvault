//! Raft cross-rack coordinator (ADR-013, ADR-014).
//!
//! Across racks, coherence is software-managed via Raft over RoCEv2/IB.
//! Commit latency is ~10–15 µs (Raft quorum RTT + local NVMe log write).
//!
//! ## OQ-04 TCP fallback
//!
//! In development (where there's no RoCEv2/IB fabric and no multi-node
//! cluster), the coordinator simulates Raft: it appends the transaction to
//! the local [`Wal`], pretends to "replicate" to a quorum of followers
//! (logged as a no-op), syncs the WAL, and returns the HLC timestamp.
//!
//! The leader invariant is enforced even in simulation: `commit()` returns
//! [`crate::Error::Protocol`] when called on a non-leader. Tests that want
//! to exercise the commit path must call [`RaftCoordinator::become_leader`]
//! first (or construct the coordinator via [`RaftCoordinator::new_as_leader`]).
//!
//! ## Quorum math
//!
//! [`RaftCoordinator::quorum`] is `floor(cluster_size / 2) + 1`. For the
//! standard configurations:
///
/// | cluster_size | quorum |
/// |--------------|--------|
/// | 1            | 1      |
/// | 3            | 2      |
/// | 5            | 3      |
/// | 7            | 4      |
use crate::protocol::clock::HlcClock;
use crate::protocol::clock::HlcTimestamp;
use crate::storage::wal::{Wal, WalRecord};
use crate::Result;
use std::sync::Arc;

/// Record type used by the Raft coordinator when appending a commit record
/// to the WAL. Matches the `record_type` byte in [`WalRecord`] (0 = commit,
/// 1 = abort, 2 = data). The Raft coordinator only ever writes commit
/// records.
const WAL_RECORD_TYPE_COMMIT: u8 = 0;

/// A Raft coordinator for cross-rack transactions.
///
/// See the [module docs](self) for the OQ-04 fallback behavior and the
/// leader invariant.
pub struct RaftCoordinator {
    /// Number of nodes in the Raft cluster.
    pub cluster_size: usize,
    /// This node's ID.
    pub node_id: u64,
    /// HLC clock for issuing monotonic commit timestamps.
    clock: HlcClock,
    /// Local WAL used for the Raft log. `None` means "in-memory only" (no
    /// crash durability). The leader appends each commit to this WAL before
    /// "replicating" to followers.
    wal: Option<Arc<Wal>>,
    /// Whether this node is currently the Raft leader.
    is_leader: bool,
}

impl RaftCoordinator {
    /// Create a new Raft coordinator that starts as a follower.
    ///
    /// Call [`Self::become_leader`] to elect this node (typically after
    /// winning a Raft election, which is not modeled here).
    pub fn new(cluster_size: usize, node_id: u64, wal: Option<Arc<Wal>>) -> Self {
        Self { cluster_size, node_id, clock: HlcClock::new(), wal, is_leader: false }
    }

    /// Convenience constructor: create a coordinator that starts as the
    /// leader. Useful for tests and single-node deployments where the
    /// election is trivial.
    pub fn new_as_leader(cluster_size: usize, node_id: u64, wal: Option<Arc<Wal>>) -> Self {
        let mut c = Self::new(cluster_size, node_id, wal);
        c.is_leader = true;
        c
    }

    /// Quorum size for this cluster: `floor(cluster_size / 2) + 1`.
    ///
    /// This is the minimum number of nodes (including the leader) that must
    /// acknowledge a log entry before it can be committed. For a 3-node
    /// cluster the quorum is 2; for 5 it's 3; for 1 it's 1.
    pub fn quorum(&self) -> usize {
        self.cluster_size / 2 + 1
    }

    /// Is this node currently the Raft leader?
    pub fn is_leader(&self) -> bool {
        self.is_leader
    }

    /// Promote this node to leader.
    ///
    /// In a real Raft implementation this would be the result of winning an
    /// election (RequestVote RPC quorum + leader lease). Here it's a simple
    /// setter — useful for tests and for single-node bootstrap.
    pub fn become_leader(&mut self) {
        self.is_leader = true;
    }

    /// Demote this node back to follower.
    ///
    /// Included for symmetry with [`Self::become_leader`] and to support
    /// leader-transfer scenarios in tests.
    pub fn become_follower(&mut self) {
        self.is_leader = false;
    }

    /// Commit a transaction via Raft quorum, with simulated replication.
    ///
    /// # Leader path
    ///
    /// 1. Issue an HLC timestamp (the linearization point).
    /// 2. Append a commit record to the local [`Wal`] (the leader's log).
    /// 3. "Replicate" to a quorum of followers — in this prototype, a
    ///    no-op (logged at `tracing::debug!` level). A real implementation
    ///    would issue `AppendEntries` RPCs over RoCEv2/IB and wait for
    ///    quorum ack.
    /// 4. Sync the local WAL (durable commit).
    /// 5. Return the timestamp.
    ///
    /// # Follower path
    ///
    /// Returns [`crate::Error::Protocol`] with the message `"not leader"`.
    /// Followers must forward write requests to the leader; they can't
    /// commit on their own.
    ///
    /// # Errors
    ///
    /// - [`crate::Error::Protocol`] when called on a non-leader.
    /// - [`crate::Error::Io`] when the WAL append or sync fails.
    pub fn commit(&mut self, txn_id: u64) -> Result<HlcTimestamp> {
        if !self.is_leader {
            return Err(crate::Error::Protocol(format!(
                "node {} is not the leader; cannot commit txn {}",
                self.node_id, txn_id
            )));
        }

        // Issue the HLC timestamp. This is the linearization point: the
        // transaction's position in the global total order is fixed at
        // this instant, regardless of how long the replication takes.
        let ts = self.clock.now();

        // Append to the local WAL (the leader's log). If the WAL is None,
        // we skip the durable append — useful for in-memory tests.
        if let Some(ref wal) = self.wal {
            let payload = ts.physical_ns.to_le_bytes();
            wal.append(&WalRecord {
                txn_id,
                record_type: WAL_RECORD_TYPE_COMMIT,
                payload: payload.to_vec(),
            })?;
        }

        // Simulate replicating to a quorum of followers. In a real Raft
        // implementation this would be a fan-out of AppendEntries RPCs over
        // RoCEv2/IB, followed by waiting for quorum ack. Here we just log
        // it (at debug level, to avoid spamming the test output) and
        // proceed.
        tracing::debug!(
            "raft: node {} replicated txn {} to {} of {} nodes (quorum {})",
            self.node_id,
            txn_id,
            self.quorum().saturating_sub(1),
            self.cluster_size.saturating_sub(1),
            self.quorum(),
        );

        // Sync the local WAL. This is the durability boundary — once sync
        // returns, the commit survives a leader crash (a quorum of
        // followers also has the entry, so they can elect a new leader).
        if let Some(ref wal) = self.wal {
            wal.sync()?;
        }

        Ok(ts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::tier::MemoryTier;
    use tempfile::tempdir;

    /// Test 8: `quorum()` is correct for the standard cluster sizes.
    ///
    /// Raft majority quorum is `floor(N/2) + 1`:
    /// - 1 node → quorum 1 (trivial)
    /// - 3 nodes → quorum 2 (tolerates 1 failure)
    /// - 5 nodes → quorum 3 (tolerates 2 failures)
    #[test]
    fn raft_quorum_is_majority() {
        let r = RaftCoordinator::new(1, 0, None);
        assert_eq!(r.quorum(), 1);
        let r = RaftCoordinator::new(3, 0, None);
        assert_eq!(r.quorum(), 2);
        let r = RaftCoordinator::new(5, 0, None);
        assert_eq!(r.quorum(), 3);
        // Edge cases: even cluster sizes (atypical for Raft, but the math
        // should still be sane).
        let r = RaftCoordinator::new(2, 0, None);
        assert_eq!(r.quorum(), 2); // 2/2 + 1 = 2 (no failure tolerance)
        let r = RaftCoordinator::new(7, 0, None);
        assert_eq!(r.quorum(), 4); // 7/2 + 1 = 4 (tolerates 3 failures)
    }

    /// Test 9: leader commit succeeds and returns a valid timestamp.
    ///
    /// Constructs a coordinator via `new_as_leader`, commits, and asserts:
    /// - The returned timestamp is non-zero.
    /// - The WAL has exactly one commit record.
    /// - The WAL record's `txn_id` matches.
    /// - The WAL record's `record_type` is `0` (commit).
    #[test]
    fn raft_leader_commit_succeeds() {
        let dir = tempdir().unwrap();
        let wal = Arc::new(Wal::open(dir.path().join("raft.log"), MemoryTier::Nvme).unwrap());
        let mut r = RaftCoordinator::new_as_leader(3, 0, Some(wal.clone()));

        assert!(r.is_leader());
        let ts = r.commit(99).unwrap();
        assert!(ts.physical_ns > 0, "physical_ns must be non-zero");
        assert_eq!(wal.records_written(), 1);

        // Verify the WAL record round-trips correctly.
        let mut reader = crate::storage::wal::WalReader::open(wal.path()).unwrap();
        let rec = reader.next_record().unwrap().expect("expected one WAL record");
        assert_eq!(rec.txn_id, 99);
        assert_eq!(rec.record_type, WAL_RECORD_TYPE_COMMIT);
        assert_eq!(rec.payload.len(), 8);
        let recovered_phys = u64::from_le_bytes(rec.payload[..].try_into().unwrap());
        assert_eq!(recovered_phys, ts.physical_ns);
    }

    /// Test 9b: leader commit without a WAL still succeeds (in-memory mode).
    #[test]
    fn raft_leader_commit_without_wal_succeeds() {
        let mut r = RaftCoordinator::new_as_leader(3, 0, None);
        let ts = r.commit(1).unwrap();
        assert!(ts.physical_ns > 0);

        // Monotonic across multiple commits.
        let ts2 = r.commit(2).unwrap();
        assert!(ts2 > ts);
    }

    /// Test 9c: leader commits are monotonic across many calls.
    #[test]
    fn raft_leader_commit_is_monotonic() {
        let dir = tempdir().unwrap();
        let wal = Arc::new(Wal::open(dir.path().join("raft_mono.log"), MemoryTier::Nvme).unwrap());
        let mut r = RaftCoordinator::new_as_leader(5, 0, Some(wal));
        let mut prev = r.commit(0).unwrap();
        for i in 1..50u64 {
            let next = r.commit(i).unwrap();
            assert!(next > prev, "non-monotonic at i={i}: {prev:?} -> {next:?}");
            prev = next;
        }
    }

    /// Test 10: non-leader commit returns an error.
    ///
    /// A freshly-constructed coordinator (via `new`) starts as a follower
    /// and must reject commits with `Error::Protocol`.
    #[test]
    fn raft_non_leader_commit_returns_error() {
        let mut r = RaftCoordinator::new(3, 0, None);
        assert!(!r.is_leader());

        let err = r.commit(42).unwrap_err();
        match err {
            crate::Error::Protocol(msg) => {
                assert!(msg.contains("not the leader"), "unexpected Protocol message: {msg}");
            }
            other => panic!("expected Error::Protocol, got {other:?}"),
        }
    }

    /// Test 10b: non-leader commit does not write to the WAL.
    ///
    /// Even if a WAL is configured, a follower's `commit()` call must not
    /// append anything — followers can't unilaterally commit.
    #[test]
    fn raft_non_leader_commit_does_not_touch_wal() {
        let dir = tempdir().unwrap();
        let wal =
            Arc::new(Wal::open(dir.path().join("raft_follower.log"), MemoryTier::Nvme).unwrap());
        let mut r = RaftCoordinator::new(3, 1, Some(wal.clone()));
        assert!(!r.is_leader());

        let _err = r.commit(7).unwrap_err();
        assert_eq!(wal.records_written(), 0, "follower must not append to WAL");
    }

    /// Test: `become_leader` / `become_follower` round-trip.
    #[test]
    fn raft_become_leader_and_follower() {
        let mut r = RaftCoordinator::new(3, 0, None);
        assert!(!r.is_leader());

        r.become_leader();
        assert!(r.is_leader());

        // Commit should now succeed.
        let _ = r.commit(1).unwrap();

        r.become_follower();
        assert!(!r.is_leader());

        // Commit should now fail.
        let _ = r.commit(2).unwrap_err();
    }

    /// Test: `new_as_leader` constructor sets the leader flag.
    #[test]
    fn raft_new_as_leader_sets_flag() {
        let r = RaftCoordinator::new_as_leader(3, 0, None);
        assert!(r.is_leader());
    }
}
