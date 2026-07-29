# ADR-005: Cache-line alignment for all atomic-containing structs

## Status
Accepted

## Confidence
95%

## Context

A split LOCK (atomic access crossing a cache line boundary) costs 3,000–10,000 cycles + ~50–200 nJ on Ice Lake+ (see `cpu-energy-kb.md` §1.8). This is the single most expensive operation on modern x86 — 100× worse than a normal cache miss.

The hash table's `LOCK CMPXCHG` for slot insertion is the likely culprit if any struct is misaligned.

## Decision

**All structs containing atomic operations are aligned to 64 bytes (cache line width) via `#[repr(align(64))]`.**

```rust
#[repr(align(64))]
struct HashTableSlot {
    key: AtomicU64,
    value: AtomicU64,
    // padding to 64 bytes is automatic
}
```

Additionally, enable Linux's `split_lock_detect=off` kernel parameter is NOT used — we want the kernel to WARN on split locks so we catch them in testing.

## Consequences

### Positive
- Eliminates the 3,000–10,000 cycle split-lock penalty
- Kernel `split_lock_detect=fatal` in CI catches any regression
- All atomics are on their own cache line → no false sharing

### Negative
- 8–32 bytes of padding per struct (12–50% memory overhead on small structs)
- Alignment allocation may fail under memory pressure (mitigated by huge pages, ADR-009)

## Alternatives considered

1. **Runtime detection only** — catch split locks in production but don't prevent them. Rejected: prevention is better than detection.
2. **32-byte alignment** — insufficient; cache lines are 64 bytes on all target CPUs.
3. **No alignment, rely on the compiler** — compilers don't guarantee cache-line alignment for atomics. Rejected.

## References
- `docs/architecture/cpu-energy-kb.md` §1.8 (split LOCK cost)
- Chips and Cheese, "Investigating split locks on x86"
