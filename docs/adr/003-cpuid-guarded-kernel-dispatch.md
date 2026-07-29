# ADR-003: CPUID-guarded kernel dispatch for BMI2/AVX-512

## Status
Accepted

## Confidence
95%

## Context

CPU features vary wildly:
- `PEXT`/`PDEP` (BMI2): 3 cycles on Intel/Zen3+, 18 cycles (microcoded) on Zen/Zen2 — 250× difference
- AVX-512: zero frequency penalty on Zen 4/5, 100 MHz on Sapphire Rapids, 300–500 MHz on Skylake-X
- `VPOPCNTDQ`: available on Ice Lake+ and Zen 5, not on Zen 4

Using the wrong kernel silently degrades performance by 10–250×.

## Decision

**At startup, probe CPUID and register only the kernels matching the detected CPU.** The kernel table dispatches based on `(Operator, CpuTarget, MemoryTier)`:

```rust
let cpu = detect_cpu(); // CPUID-based
let table = KernelTable::new(); // registers kernels for `cpu`
let kernel = table.select(Operator::ScanEqU64, tier); // returns best kernel
```

For BMI2 specifically: guard with `is_x86_feature_detected!("bmi2")` and check for Zen 3+ via CPUID family/model. If Zen/Zen2, use the software `pext` fallback.

## Consequences

### Positive
- Correct performance on every CPU generation
- No silent degradation (the wrong kernel is never selected)
- New CPUs get new kernels added to the table without breaking old ones

### Negative
- CPUID detection adds ~1 ms to startup (negligible)
- Must maintain multiple kernel variants per operator (engineering cost)

## Alternatives considered

1. **Always use AVX-512** — would throttle Skylake-X by 500 MHz. Rejected.
2. **Always use AVX2** — would lose 2× on Sapphire Rapids/Zen 5. Rejected.
3. **Runtime JIT** — could generate the optimal kernel at runtime, but adds complexity. Deferred to the trace JIT (future ADR).

## References
- `docs/architecture/cpu-energy-kb.md` §1.2 (BMI2 landmine), §1.3 (AVX-512 throttling)
- `src/kernel/cpu.rs` (CPU detection implementation)
