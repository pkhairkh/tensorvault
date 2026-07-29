//! `RaftRef<T>` — an affine reference to cross-rack data.
//!
//! See the [module docs](crate::types) for the big picture and ADR-013 for the
//! design rationale.

use std::marker::PhantomData;
use std::ptr::NonNull;

/// An affine reference to cross-rack data.
///
/// `RaftRef` is the cross-rack counterpart to [`CxlRef`](crate::types::CxlRef):
///
/// - **Affine** (not linear) — no `Clone` or `Copy`, so the handle cannot be
///   duplicated, but it *can* be dropped without use. [`get`](Self::get)
///   borrows `&self` rather than consuming, so multiple shared reads are
///   permitted (Rust's borrow checker enforces the no-aliasing rule for
///   [`get_mut`](Self::get_mut)).
/// - **`Send` + `Sync`** — the handle can travel across rack boundaries,
///   because the underlying data is replicated via Raft and is therefore
///   accessible from any rack. The `unsafe impl`s below opt into this
///   explicitly, bounded on `T: Send` / `T: Sync` so we don't accidentally
///   claim thread-safety for types that lack it.
///
/// `RaftRef` does not carry a region (the data is not in a local tier), so
/// there is no `Drop` accounting.
pub struct RaftRef<'a, T: ?Sized> {
    /// Pointer to the cross-rack data. Non-null by construction.
    ptr: NonNull<T>,
    /// `&'a mut T` ties the borrow lifetime; `*mut ()` is the marker used by
    /// ADR-013 so that `Send`/`Sync` must be opted into explicitly (which we
    /// do via the `unsafe impl`s below) rather than inherited automatically.
    _marker: PhantomData<(&'a mut T, *mut ())>,
}

// NOTE: Intentionally NO `impl Clone` and NO `impl Copy` for `RaftRef`.
// This is what makes the handle *affine* — at most one outstanding
// mutable borrow, no duplication.

impl<'a, T: ?Sized> RaftRef<'a, T> {
    /// Create a new `RaftRef` for `data`.
    ///
    /// Unlike [`CxlRef::new`](crate::types::CxlRef::new), no region or tier
    /// check is performed — `RaftRef` is tier-agnostic by design (the data is
    /// cross-rack and replicated, not resident in any single local tier).
    pub fn new(data: &'a mut T) -> Self {
        Self { ptr: NonNull::from(data), _marker: PhantomData }
    }

    /// Borrow the handle and return a shared reference to the data.
    ///
    /// Unlike [`CxlRef::get`](crate::types::CxlRef::get), this does *not*
    /// consume the handle — affine semantics allow multiple shared reads.
    /// The returned reference borrows `&self`, so the handle must outlive the
    /// reference.
    pub fn get(&self) -> &T {
        // SAFETY: `ptr` was derived from a valid `&'a mut T` in `new()`, and
        // the data lives for `'a`. We hold a shared borrow of `self`, which
        // prevents `get_mut` from being called concurrently (Rust's aliasing
        // rules), so the shared access here is sound.
        unsafe { self.ptr.as_ref() }
    }

    /// Exclusively borrow the handle and return a mutable reference to the
    /// data.
    pub fn get_mut(&mut self) -> &mut T {
        // SAFETY: Same as `get()`, but we hold `&mut self`, which gives us
        // exclusive access and prevents any other borrow from existing.
        unsafe { self.ptr.as_mut() }
    }
}

// `RaftRef` can travel across rack boundaries (and therefore across threads)
// because the underlying data is replicated via Raft: any rack can serve a
// read, and writes go through the Raft log. We bound on `T: Send` / `T: Sync`
// so we don't claim thread-safety for types that lack it.
//
// SAFETY: `RaftRef` is a thin wrapper around `NonNull<T>`. Sending it to
// another thread is sound iff `T: Send` (the receiver can `get_mut` and
// obtain `&mut T`). Sharing `&RaftRef<T>` across threads is sound iff
// `T: Sync` (the sharers can `get` and obtain `&T` concurrently).
unsafe impl<'a, T: ?Sized + Send> Send for RaftRef<'a, T> {}

// SAFETY: See the comment above. `&RaftRef<T>` only permits `get`, which
// yields `&T`, so we need `T: Sync`.
unsafe impl<'a, T: ?Sized + Sync> Sync for RaftRef<'a, T> {}
