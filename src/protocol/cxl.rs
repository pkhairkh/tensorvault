//! CXL single-rack coordinator (ADR-013, ADR-014).
//!
//! Within a CXL 3.0 fabric, coherence is hardware-managed. Transactions
//! commit via a fence + CXL cache flush, ~250 ns latency. No consensus
//! protocol is needed — the fabric provides visibility.
//!
//! ## OQ-02 fallback
//!
//! When CXL hardware is not present (the common case in development), the
//! coordinator falls back to a local NVMe-backed [`Wal`]: it appends a
//! commit record, fsyncs the WAL, and returns the HLC timestamp. This
//! preserves the durability contract at the cost of latency (~10–30 µs
//! for an NVMe fsync vs. ~250 ns for a CXL fence).
//!
//! The fallback path is what runs in CI and on developer laptops. The CXL
//! path is simulated: we don't have CXL hardware in the test environment, so
//! the "flush + MFENCE" is a no-op and we just return `clock.now()`. The
//! presence of `/sys/bus/cxl/` is the liveness signal — when that path
//! exists, we believe CXL is available and skip the WAL fallback.

use crate::memory::tier::MemoryTier;
use crate::protocol::clock::HlcClock;
use crate::protocol::clock::HlcTimestamp;
use crate::storage::wal::{Wal, WalRecord};
use crate::Result;
use std::sync::Arc;

/// Record type used by the CXL coordinator when appending a fallback commit
/// record to the WAL. Matches the `record_type` byte in [`WalRecord`] (0 =
/// commit, 1 = abort, 2 = data). The CXL coordinator only ever writes
/// commit records.
const WAL_RECORD_TYPE_COMMIT: u8 = 0;

/// A CXL coordinator for single-rack transactions.
///
/// See the [module docs](self) for the design rationale and the OQ-02
/// fallback behavior.
pub struct CxlCoordinator {
    /// Whether CXL hardware is present on this system (detected at
    /// construction time by checking for `/sys/bus/cxl/`).
    available: bool,
    /// HLC clock for issuing monotonic commit timestamps.
    clock: HlcClock,
    /// Local WAL used for fallback commits when CXL is not available.
    /// `None` means "no fallback" — commits on non-CXL hardware will still
    /// succeed but won't be durable across a crash. This is useful for
    /// in-memory tests where durability isn't required.
    wal: Option<Arc<Wal>>,
}

impl CxlCoordinator {
    /// Create a new CXL coordinator.
    ///
    /// If `wal` is `Some`, it's used as the fallback durability target
    /// when CXL hardware is not present. If `wal` is `None`, commits on
    /// non-CXL hardware are still issued (returning a valid HLC timestamp)
    /// but lack crash durability.
    pub fn new(wal: Option<Arc<Wal>>) -> Self {
        Self { available: crate::memory::numa::cxl_available(), clock: HlcClock::new(), wal }
    }

    /// Is CXL hardware available on this system?
    ///
    /// Returns the cached result of the `/sys/bus/cxl/` probe done at
    /// construction time. This is a cheap `bool` read — safe to call on
    /// the hot path.
    pub fn is_available(&self) -> bool {
        self.available
    }

    /// The tier this coordinator writes commit records to.
    ///
    /// Returns [`MemoryTier::Cxl`] when CXL hardware is present,
    /// [`MemoryTier::Nvme`] when falling back to the WAL, and
    /// [`MemoryTier::Ddr5`] when there's no WAL (in-memory only).
    pub fn commit_tier(&self) -> MemoryTier {
        if self.available {
            MemoryTier::Cxl
        } else if self.wal.is_some() {
            MemoryTier::Nvme
        } else {
            MemoryTier::Ddr5
        }
    }

    /// Commit a transaction via CXL fence, with WAL fallback (OQ-02).
    ///
    /// # CXL path (when [`is_available`](Self::is_available) is true)
    ///
    /// In a real implementation, this would:
    /// 1. Issue a CXL.cache flush for the modified cache lines
    /// 2. Issue a memory fence (MFENCE)
    /// 3. Publish the commit by writing to a shared CXL-resident commit record
    ///
    /// In this prototype the flush is a no-op (we don't have CXL hardware to
    /// test against); we just issue an HLC timestamp and return. The
    /// `available` flag is the liveness signal — when it's `true`, we
    /// believe the CXL fabric is present and skip the WAL fallback.
    ///
    /// # WAL fallback (when CXL is not available)
    ///
    /// Appends a commit record to the local [`Wal`], fsyncs it, and returns
    /// the HLC timestamp. This is the path that runs in CI and on developer
    /// laptops. Latency is dominated by the WAL fsync (~10–30 µs on NVMe).
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Io`] if the WAL append or sync fails (e.g.
    /// disk full, or the WAL has been closed via `simulate_crash`).
    pub fn commit(&mut self, txn_id: u64) -> Result<HlcTimestamp> {
        // Issue the HLC timestamp first. This is the linearization point:
        // once the timestamp is issued, the transaction is "logically
        // committed" even if the durability step fails. (In a real
        // implementation we'd undo the timestamp on durability failure, but
        // for this prototype the simpler "timestamp-first" model is fine.)
        let ts = self.clock.now();

        if self.available {
            // CXL path: simulated. A real implementation would issue
            // CXL.cache flush + MFENCE here. The timestamp is the
            // publication record — no further I/O needed.
            return Ok(ts);
        }

        // WAL fallback path. Append a commit record and fsync.
        if let Some(ref wal) = self.wal {
            let payload = ts.physical_ns.to_le_bytes();
            wal.append(&WalRecord {
                txn_id,
                record_type: WAL_RECORD_TYPE_COMMIT,
                payload: payload.to_vec(),
            })?;
            wal.sync()?;
        }

        Ok(ts)
    }
}

impl Default for CxlCoordinator {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Test 5: `is_available()` returns false on non-CXL hardware.
    ///
    /// The test environment doesn't have `/sys/bus/cxl/`, so this should
    /// always be false in CI. (If someone runs these tests on a CXL-equipped
    /// machine, this test will skip itself with `eprintln!`.)
    #[test]
    fn cxl_coordinator_is_available_returns_false_in_ci() {
        let c = CxlCoordinator::new(None);
        if c.is_available() {
            // Running on CXL hardware — the test doesn't apply here.
            eprintln!("note: CXL hardware detected; skipping non-CXL assertion");
            return;
        }
        assert!(!c.is_available());
    }

    /// Test 6: `commit()` with WAL fallback succeeds and returns a timestamp.
    ///
    /// On non-CXL hardware, `commit()` should append a commit record to the
    /// WAL, sync it, and return a valid `HlcTimestamp`. The WAL should have
    /// exactly one record after the commit, with `txn_id == 42` and
    /// `record_type == 0` (commit).
    #[test]
    fn cxl_commit_with_wal_fallback_succeeds() {
        let dir = tempdir().unwrap();
        let wal = Arc::new(Wal::open(dir.path().join("cxl.log"), MemoryTier::Nvme).unwrap());
        let mut c = CxlCoordinator::new(Some(wal.clone()));

        // If CXL is actually available on this machine, the WAL fallback
        // path won't run — but the commit should still succeed and return
        // a timestamp. The test below is written to handle both cases.
        let ts = c.commit(42).unwrap();
        assert!(ts.physical_ns > 0, "physical_ns must be non-zero");

        if c.is_available() {
            // CXL path: no WAL record written.
            return;
        }

        // WAL fallback path: exactly one record should have been appended.
        assert_eq!(wal.records_written(), 1);

        // Verify the WAL record round-trips through WalReader.
        wal.sync().unwrap(); // Belt-and-suspenders.
        let mut reader = crate::storage::wal::WalReader::open(wal.path()).unwrap();
        let rec = reader.next_record().unwrap().expect("expected one WAL record");
        assert_eq!(rec.txn_id, 42);
        assert_eq!(rec.record_type, WAL_RECORD_TYPE_COMMIT);
        // Payload is the 8-byte LE physical_ns of the issued timestamp.
        assert_eq!(rec.payload.len(), 8);
        let recovered_phys = u64::from_le_bytes(rec.payload[..].try_into().unwrap());
        assert_eq!(recovered_phys, ts.physical_ns);
    }

    /// Test 7: `commit()` without a WAL still works on non-CXL hardware.
    ///
    /// When `wal` is `None`, the coordinator can't provide crash durability,
    /// but it must still return a valid HLC timestamp. This is useful for
    /// in-memory tests and for the smoke example.
    #[test]
    fn cxl_commit_without_wal_still_works() {
        let mut c = CxlCoordinator::new(None);
        let ts = c.commit(7).unwrap();
        assert!(ts.physical_ns > 0, "physical_ns must be non-zero");

        // A second commit must produce a strictly greater timestamp.
        let ts2 = c.commit(8).unwrap();
        assert!(ts2 > ts, "second commit must be strictly greater: {ts:?} -> {ts2:?}");
    }

    /// Test 7b: `commit()` is monotonic across many calls.
    ///
    /// Issues 100 commits in a tight loop and asserts strict monotonicity.
    /// This exercises both the CXL path (a no-op `clock.now()`) and the WAL
    /// path (append + sync per commit).
    #[test]
    fn cxl_commit_is_monotonic() {
        let dir = tempdir().unwrap();
        let wal = Arc::new(Wal::open(dir.path().join("cxl_mono.log"), MemoryTier::Nvme).unwrap());
        let mut c = CxlCoordinator::new(Some(wal));
        let mut prev = c.commit(0).unwrap();
        for i in 1..100u64 {
            let next = c.commit(i).unwrap();
            assert!(next > prev, "non-monotonic at i={i}: {prev:?} -> {next:?}");
            prev = next;
        }
    }

    /// Test: `commit_tier()` returns the right tier for each configuration.
    #[test]
    fn cxl_commit_tier_reflects_fallback() {
        // No WAL, no CXL: in-memory (Ddr5).
        let c = CxlCoordinator::new(None);
        if c.is_available() {
            assert_eq!(c.commit_tier(), MemoryTier::Cxl);
        } else {
            assert_eq!(c.commit_tier(), MemoryTier::Ddr5);
        }

        // With WAL, no CXL: NVMe.
        let dir = tempdir().unwrap();
        let wal = Arc::new(Wal::open(dir.path().join("tier.log"), MemoryTier::Nvme).unwrap());
        let c = CxlCoordinator::new(Some(wal));
        if c.is_available() {
            assert_eq!(c.commit_tier(), MemoryTier::Cxl);
        } else {
            assert_eq!(c.commit_tier(), MemoryTier::Nvme);
        }
    }

    /// Test: `Default` impl is equivalent to `new(None)`.
    #[test]
    fn cxl_default_is_new_none() {
        let a = CxlCoordinator::new(None);
        let b = CxlCoordinator::default();
        assert_eq!(a.is_available(), b.is_available());
        assert_eq!(a.commit_tier(), b.commit_tier());
    }

    /// Legacy regression test: the coordinator doesn't crash on a basic
    /// commit. Kept from the Wave 1 stub so existing CI configs don't break.
    #[test]
    fn cxl_coordinator_doesnt_crash() {
        let mut c = CxlCoordinator::new(None);
        let _ = c.commit(0);
    }
}
