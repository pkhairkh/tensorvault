# ADR-006: REP MOVSB with ERMS for bulk page copy

## Status
Accepted

## Confidence
100%

## Context

Region migration (ADR-010) requires copying 2 MB of data between tiers. The copy must be:
- Fast (~1 byte/cycle achievable)
- Hardware-prefetched (so the copy doesn't stall on cache misses)
- Available on all x86-64 CPUs (no AVX-512 dependency)

## Decision

**Use `REP MOVSB` with ERMS (Fast Short REP MOV) for all bulk memory copies.**

In Rust, this means using `ptr::copy_nonoverlapping` (which compilers lower to `REP MOVSB` on x86-64 with ERMS) or `memcpy` from libc. Do NOT use hand-written AVX-512 copy loops — they are slower than `REP MOVSB` for buffers > 128 bytes because they lack the hardware prefetcher integration.

## Consequences

### Positive
- ~1 byte/cycle throughput (2 MB in ~2 ms at 1 GHz, ~0.7 ms at 3 GHz)
- Hardware prefetcher handles the streaming pattern automatically
- Works on all x86-64 CPUs since Ivy Bridge (2012)
- Zero setup overhead (no SIMD register initialization)

### Negative
- Not available on ARM (use `memcpy` which compilers optimize per-platform)
- Slightly slower than AVX-512 for buffers < 128 bytes (use scalar for those)

## Alternatives considered

1. **AVX-512 `VMOVDQA64` loop** — 2× the instruction count, no prefetcher integration. 10–20% slower for large buffers. Rejected.
2. **`memcpy` from libc** — same underlying `REP MOVSB`, but adds function call overhead. Used as fallback on non-x86.
3. **`mmap` + `mremap` (copy-on-write)** — avoids the copy entirely but requires page-aligned data and OS support. Deferred to ADR-010 (migration mechanics).

## References
- `docs/architecture/cpu-energy-kb.md` §1.7 (REP MOVSB with ERMS)
- Intel Optimization Manual, "Fast String Operations"
