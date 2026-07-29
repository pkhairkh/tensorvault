# ADR-007: Fixed 1024-cell batch size for SIMD amortization

## Status
Accepted

## Confidence
85%

## Context

SIMD setup cost (load constants, align, loop tail handling) is ~5–15 cycles. Break-even:
- SSE2 (128-bit): ~8–16 elements
- AVX2 (256-bit): ~32–64 elements
- AVX-512 (512-bit): ~64–128 elements

Below break-even, SIMD is slower than scalar. Above break-even, throughput is flat (amortized). The batch size also affects cache residency — too large and the batch spills from L1 to L2.

## Decision

**Use a fixed batch size of 1024 cells (8 KB) for all vectorized operators.**

This is the industry standard (ClickHouse, DuckDB both use 1024–4096). It:
- Is well past the AVX-512 break-even (128)
- Fits in L1 (32 KB typical, 8 KB batch leaves room for other data)
- Is a power of 2 (simplifies alignment and loop logic)

## Consequences

### Positive
- SIMD setup is fully amortized (< 0.1% overhead)
- Batch fits in L1, enabling the data-centric morsel pipeline (ADR-018)
- Simple: no adaptive logic needed
- Matches industry convention (easy to benchmark against DuckDB/ClickHouse)

### Negative
- Suboptimal for very small tables (< 1024 rows) — scalar fallback handles those
- May be too large for L1 on some ARM cores (16 KB L1) — future ARM port may need adjustment

## Alternatives considered

1. **Adaptive batch size** — tune based on L1 size and data type. Adds complexity for < 5% gain. Rejected.
2. **4096-cell batch** — slightly better amortization but risks L1 spill. Rejected for v1; revisit with profile data.
3. **256-cell batch** — below the sweet spot for AVX-512. Rejected.

## References
- `docs/architecture/cpu-energy-kb.md` §8.2 (SIMD amortization break-even)
- ClickHouse engineering blog, "Vectorized Query Execution"
