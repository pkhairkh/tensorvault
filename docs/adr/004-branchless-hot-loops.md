# ADR-004: Branchless hot loops via mask accumulation + CMOV

## Status
Accepted

## Confidence
90%

## Context

A mispredicted branch costs 15–21 cycles + ~2–4 nJ on modern x86 (see `cpu-energy-kb.md` §1.9). In scan kernels, the tail loop (processing remaining cells after the SIMD chunk) has branches that mispredict on the last iteration. On adversarial data (alternating match/no-match), mispredict rate can hit 50%, wasting half the cycles.

## Decision

**All hot-loop kernels use branchless patterns: mask accumulation + CMOV instead of conditional branches.**

Pattern:
```rust
// BAD: branch on every cell
if cell == target { count += 1; }

// GOOD: branchless via mask
let mask = (cell == target) as u64;
count += mask;
```

For the SIMD tail, use `VPCMPEQQ` to produce a mask, then `POPCNT` the mask — no branches at all.

## Consequences

### Positive
- 5× speedup on adversarial data (alternating match/no-match)
- Eliminates the mispredict tax (~2–4 nJ per avoided mispredict)
- Predictable performance (no dependency on branch predictor state)

### Negative
- Slightly more code complexity (mask logic is less readable than if/else)
- May be slower on uniform data (where branches are always predicted correctly) — but the difference is < 5%

## Alternatives considered

1. **Accept mispredicts** — 15–21 cycles per mispredict is too expensive. Rejected.
2. **Software pipelining** — complex and doesn't eliminate the branch. Rejected.
3. **Compiler `#[inline(always)]` + `likely`/`unlikely` hints** — helps but doesn't eliminate mispredicts. Used as a supplement, not a replacement.

## References
- `docs/architecture/cpu-energy-kb.md` §1.9 (branch mispredict cost)
- Lemire, "Branchless programming" — blog series on branchless techniques
