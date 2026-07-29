# ADR-002: 4 KB page / 2 MB region / 2 GB tablet storage hierarchy

## Status
Accepted

## Confidence
95%

## Context

The storage format needs a hierarchy of units that aligns with:
- OS page size and TLB granularity (4 KB)
- Huge page size for TLB pressure reduction (2 MB)
- NUMA placement granularity for tier-aware allocation (2 GB)

## Decision

**Three-level storage hierarchy:**
- **Page**: 4 KB = 64-byte header + 4032 bytes (504 u64 cells). The I/O unit.
- **Region**: 2 MB = 512 pages. The migration unit (matches huge page).
- **Tablet**: 2 GB = 1024 regions. The NUMA placement unit.

## Consequences

### Positive
- 4 KB page matches OS page, x86 TLB, and 64 cache lines — scanning one page fits in L1
- 2 MB region matches huge page — reduces TLB misses by 512×
- 2 GB tablet is large enough to amortize NUMA placement overhead
- Clean separation: I/O (page), migration (region), placement (tablet)

### Negative
- 4 KB page is small for sequential scans (many page boundaries) — mitigated by region-level prefetch
- 2 GB tablet is inflexible for small tables — mitigated by allowing sparse tablets

## Alternatives considered

1. **8 KB or 16 KB pages** — would reduce page-boundary overhead but mismatch OS page size, causing double-mapping. Rejected.
2. **1 GB regions** — too large for fine-grained migration. Rejected.
3. **No tablet level** — regions placed directly on NUMA nodes. Rejected: NUMA placement overhead is per-allocation, so batching into tablets reduces it.

## References
- `docs/architecture/cpu-energy-kb.md` §2.1 (memory hierarchy latency)
- `src/storage/page.rs`, `src/storage/tablet.rs` (implementation)
