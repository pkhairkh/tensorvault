//! Tests for the linear/affine memory handles and the expanded `Error` enum.

use crate::memory::region::Region;
use crate::memory::tier::MemoryTier;
use crate::types::{CxlRef, RaftRef};
use crate::Error;

/// Test 1: `CxlRef::new()` panics when the region tier is not CXL.
///
/// This is the runtime tier assertion that backs the compile-time type
/// discipline — a `CxlRef` must only ever wrap CXL-resident data.
#[test]
#[should_panic(expected = "CxlRef requires CXL tier")]
fn cxl_ref_new_panics_for_non_cxl_tier() {
    let mut value: u64 = 42;
    // DDR5 is the wrong tier — `CxlRef::new` must refuse this.
    let region = Region::allocate(0, MemoryTier::Ddr5);
    let _ = CxlRef::new(&mut value, &region);
}

/// Test 2: `CxlRef::new()` succeeds when the region tier IS CXL.
#[test]
fn cxl_ref_new_succeeds_for_cxl_tier() {
    let mut value: u64 = 42;
    let region = Region::allocate(0, MemoryTier::Cxl);
    // Construction must not panic.
    let cxl_ref = CxlRef::new(&mut value, &region);
    // `get` consumes the handle; calling it proves construction succeeded
    // and that the resulting reference is usable.
    let r = cxl_ref.get();
    assert_eq!(*r, 42);
}

/// Test 3: `CxlRef::get()` returns the correct value.
#[test]
fn cxl_ref_get_returns_correct_value() {
    let mut value: u64 = 0xDEAD_BEEF;
    let region = Region::allocate(7, MemoryTier::Cxl);
    let cxl_ref = CxlRef::new(&mut value, &region);
    let r = cxl_ref.get();
    assert_eq!(*r, 0xDEAD_BEEF);
}

/// Test 4: `RaftRef::get()` returns the correct value.
#[test]
fn raft_ref_get_returns_correct_value() {
    let mut value: u64 = 0xCAFE_BABE;
    let raft_ref = RaftRef::new(&mut value);
    let r = raft_ref.get();
    assert_eq!(*r, 0xCAFE_BABE);
}

/// Test 5: The new `Error` variants format correctly via `Display`.
#[test]
fn error_new_variants_format_correctly() {
    assert_eq!(format!("{}", Error::Tier("data not in CXL".into())), "tier error: data not in CXL");
    assert_eq!(
        format!("{}", Error::Protocol("CXL leaked to Raft txn".into())),
        "protocol error: CXL leaked to Raft txn"
    );
    assert_eq!(
        format!("{}", Error::Parse("unexpected token 'FROM'".into())),
        "parse error: unexpected token 'FROM'"
    );
    assert_eq!(format!("{}", Error::Timeout(5_000)), "timeout after 5000 ms");
}

/// Test 6 (bonus): `CxlRef`'s `Drop` impl records a read access in the
/// region's stats, so the placement policy (ADR-010) can observe the touch
/// even when the caller forgets to call `get` explicitly.
#[test]
fn cxl_ref_drop_records_read_in_region_stats() {
    use std::sync::atomic::Ordering;

    let mut value: u64 = 0;
    let region = Region::allocate(11, MemoryTier::Cxl);
    let reads_before = region.stats.reads.load(Ordering::Relaxed);
    {
        let cxl_ref = CxlRef::new(&mut value, &region);
        // Drop without calling `get` — Drop must still record the access.
        drop(cxl_ref);
    }
    let reads_after = region.stats.reads.load(Ordering::Relaxed);
    assert_eq!(reads_after, reads_before + 1);
}
