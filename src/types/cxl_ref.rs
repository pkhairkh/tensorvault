//! `CxlRef<T>` — a linear reference to CXL-resident data.
//!
//! See the [module docs](crate::types) for the big picture and ADR-013 for the
//! design rationale.

use crate::memory::region::{Region, RegionStats};
use crate::memory::tier::MemoryTier;
use std::marker::PhantomData;
use std::ptr::NonNull;

/// A linear reference to CXL-resident data.
///
/// `CxlRef` enforces three invariants at compile time:
///
/// 1. **Linear use** — no `Clone` or `Copy`, so the handle can only be moved,
///    never duplicated. The compiler rejects `let a = cxl_ref; let b = cxl_ref;`.
/// 2. **Rack-locality** — `!Send` and `!Sync`, so the handle cannot escape the
///    rack (CXL 3.0 fabric) scope. Crossing a rack boundary requires going
///    through [`RaftRef`](crate::types::RaftRef) instead.
/// 3. **Tier-correctness** — [`CxlRef::new`] panics if the region is not in
///    the CXL tier, so a `CxlRef` is always backed by CXL memory.
///
/// The `Drop` impl records a read access in the region's stats, so the handle
/// is observable to the LRU placement policy (ADR-010) even when the caller
/// forgets to touch the region explicitly.
pub struct CxlRef<'a, T: ?Sized> {
    /// Pointer to the CXL-resident data. Non-null by construction.
    ptr: NonNull<T>,
    /// Pointer to the owning region's stats, used by `Drop` to record the
    /// access. The region outlives this handle (tied by the `'a` lifetime).
    stats: NonNull<RegionStats>,
    /// `&'a mut T` ties the borrow lifetime; `*mut ()` opts out of
    /// auto-`Send`/`Sync` (see ADR-013).
    _marker: PhantomData<(&'a mut T, *mut ())>,
}

// NOTE: Intentionally NO `impl Clone` and NO `impl Copy` for `CxlRef`.
// This is what makes the handle *linear* — it can be moved exactly once.
//
// NOTE: Intentionally NO `unsafe impl Send` / `unsafe impl Sync`. The
// `PhantomData<*mut ()>` marker above makes `CxlRef` `!Send` and `!Sync`
// by default, which is exactly what we want: CXL data must not escape the
// rack scope.

impl<'a, T: ?Sized> CxlRef<'a, T> {
    /// Create a new `CxlRef` for `data` resident in `region`.
    ///
    /// # Panics
    ///
    /// Panics if `region.tier` is not [`MemoryTier::Cxl`]. This is a
    /// programmer error — the type system cannot enforce it without dependent
    /// types, so we fall back to a runtime assertion that fails loudly.
    pub fn new(data: &'a mut T, region: &'a Region) -> Self {
        assert!(region.tier == MemoryTier::Cxl, "CxlRef requires CXL tier, got {}", region.tier);
        Self { ptr: NonNull::from(data), stats: NonNull::from(&region.stats), _marker: PhantomData }
    }

    /// Consume the handle and return a shared reference to the data.
    ///
    /// This is a *consuming* accessor — the handle is moved and cannot be
    /// reused. The returned reference borrows the underlying data for `'a`,
    /// outliving the handle itself.
    ///
    /// `Drop` runs after the reference is produced and records a read access
    /// in the region's stats.
    pub fn get(self) -> &'a T {
        // SAFETY: `ptr` was derived from a valid `&'a mut T` in `new()`, and
        // the data lives for `'a`. `Drop` (which runs after this return)
        // touches only `self.stats`, never `self.ptr`, so the returned
        // reference remains valid.
        unsafe { self.ptr.as_ref() }
    }

    /// Consume the handle and return a mutable reference to the data.
    ///
    /// Like [`get`](Self::get), this consumes the handle. `Drop` records a
    /// read access (the write itself is the caller's responsibility).
    pub fn get_mut(mut self) -> &'a mut T {
        // SAFETY: Same as `get()`. The caller has exclusive access via the
        // returned `&'a mut T` because the handle was consumed (linear).
        unsafe { self.ptr.as_mut() }
    }
}

impl<'a, T: ?Sized> Drop for CxlRef<'a, T> {
    fn drop(&mut self) {
        // SAFETY: `stats` was derived from a valid `&RegionStats` in `new()`,
        // and the region outlives `'a` (so it outlives this handle).
        // `record_read` is a non-atomic-friendly atomic op, but `RegionStats`
        // is `Sync` so the shared access here is sound.
        unsafe { self.stats.as_ref() }.record_read();
    }
}
