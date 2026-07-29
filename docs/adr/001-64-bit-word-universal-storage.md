# ADR-001: Use 64-bit word as the universal storage unit

## Status
Accepted

## Confidence
95%

## Context

The engine needs a single physical storage unit that:
1. Matches the lane width of the cheapest SIMD instructions (`VPCMPEQQ`, `VPADDQ`, `VPOPCNTDQ`, `VPTERNLOGQ` all operate on 64-bit lanes)
2. Can hold any logical type (f64, i32, bool, pointer, string, null)
3. Is uniform across all columns, enabling one executor for all types

The alternative (type-specific column widths, like DuckDB) is more compact but prevents a uniform kernel table and requires per-type executor dispatch.

## Decision

**Every value in every column is stored as a 64-bit word (u64).** Type information lives in the schema layer (metadata), not in the storage format.

NaN-boxing is used for inline type tagging when needed (pointers, short strings, nulls), but the physical layout is always 8 bytes per cell.

## Consequences

### Positive
- One kernel works for all types: `scan_eq_u64` scans ints, floats, pointers, strings
- SIMD throughput is maximized: 8 lanes per `VPCMPEQQ` instruction
- The kernel table stays small: one kernel per (operator, CPU, tier), not per (operator, CPU, tier, type)
- Schema evolution is cheap: changing a column's type doesn't rewrite storage

### Negative
- 20–50% storage overhead vs type-specific widths (a `bool` uses 8 bytes instead of 1 bit)
- Loses TPC-H to DuckDB by 1.2–1.5× (type-stable columns are more compact)
- Wastes cache on narrow columns (i8, i16)

## Alternatives considered

1. **Type-specific column widths** (DuckDB style) — more compact, but requires per-type kernels and breaks the "one kernel per operator" invariant. Rejected because it doubles the kernel table size.
2. **Bit-packing** (pack 8×i8 into one u64) — saves space but requires unpacking before SIMD, negating the throughput win. Deferred to ADR for variable-length cells (future).
3. **16-bit words** — would allow 32 lanes per AVX-512 instruction, but can't hold f64 or pointers inline. Rejected.

## References
- `docs/architecture/cpu-energy-kb.md` §1.3–1.6 (SIMD instruction widths)
- `docs/architecture/instruction-first.md` §4 (storage format)
