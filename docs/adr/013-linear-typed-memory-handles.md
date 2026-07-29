# ADR-013: Linear-typed memory handles (CxlRef, RaftRef)

## Status
Accepted

## Confidence
85%

## Context

Protocol safety is currently a runtime check. A CXL-resident region's data could accidentally be sent to a cross-rack transaction, violating the protocol boundary. This is a correctness bug that's hard to catch in testing but could cause data corruption in production.

Linear type theory (Girard 1987) enforces that a value is used exactly once. Affine type theory allows zero or one use. Rust's affine type system already prevents duplication; we just need to add the linear discipline for protocol-specific references.

## Decision

**Introduce two new wrapper types via Rust newtypes + `Drop` impls:**

```rust
/// Linear reference to CXL-resident data.
/// Cannot be duplicated (no Clone, no Copy).
/// Cannot escape the rack scope (no Send, no Sync).
pub struct CxlRef<T> {
    ptr: NonNull<T>,
    _marker: PhantomData<*mut ()>, // !Send + !Sync
}

/// Affine reference to cross-rack data (via Raft).
/// Can be dropped, but not duplicated.
pub struct RaftRef<T> {
    ptr: NonNull<T>,
    _marker: PhantomData<*mut ()>, // Send + Sync via explicit impl
}

// Local references are unconstrained (current behavior)
pub type LocalRef<T> = &T;
```

**Critical**: NO `Clone` or `Copy` impls for `CxlRef` or `RaftRef`. This makes them linear/affine — they can only be moved, not duplicated.

## Consequences

### Positive
- **Compile-time protocol safety**: CXL data cannot leak to a cross-rack transaction — the type system prevents it
- **Zero runtime overhead**: the types are compile-time only (no runtime checks)
- **Self-documenting**: the type signature tells you where data lives
- Eliminates an entire class of bugs (protocol boundary violations)

### Negative
- **Ergonomic friction**: moving `CxlRef` explicitly is more verbose than passing `&T`
- **Borrowing complexity**: multiple readers need a `&CxlRef` (shared borrow), which Rust handles but requires care
- **Migration effort**: existing code using raw references needs updating

## Alternatives considered

1. **Runtime checks only** — a `debug_assert!(tier == CXL)` at the protocol boundary. Catches bugs in testing but not in production. Rejected.
2. **External linear type checker** (e.g., a custom Clippy lint) — more precise but adds build complexity. Deferred.
3. **Session types** (Honda 1993) — enforce protocol ordering, not just locality. Overkill for this use case; deferred to future ADR for transaction protocols.

## Compatibility

- Compatible with ADR-008 (NUMA pinning): `CxlRef` is tied to the CXL NUMA node
- Compatible with ADR-010 (LRU migration): migrating a region invalidates its `CxlRef` (the `Drop` impl handles this)
- Compatible with ADR-018 (morsel executor): morsels carry `LocalRef`s; cross-tier access requires explicit `CxlRef`/`RaftRef`

## References
- Girard, "Linear Logic" TCS 1987
- Wadler, "Linear Types Can Change the World!" 1990
- Walker, "Substructural Type Systems" in Pierce's "Advanced Topics in Types and Programming Languages" 2002
