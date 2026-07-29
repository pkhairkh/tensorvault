# ADR-009: Transparent huge pages + explicit mmap for regions

## Status
Accepted

## Confidence
85%

## Context

TLB misses cost ~7–30 cycles per miss (2-level page walk). Using 4 KB pages for a 2 MB region means 512 TLB entries; the TLB (typically 64–1536 entries) can't hold them all. Using 2 MB huge pages reduces TLB entries by 512×.

The region size is 2 MB (ADR-002), matching the huge page granularity. But we need to explicitly request huge pages.

## Decision

**Allocate all regions via `mmap` with `MAP_HUGETLB` flag, falling back to transparent huge pages (THP) via `madvise(MADV_HUGEPAGE)`.**

```rust
let ptr = unsafe {
    mmap(
        std::ptr::null_mut(),
        REGION_SIZE, // 2 MB
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS | MAP_HUGETLB,
        -1, 0,
    )
};
// Fallback to THP if MAP_HUGETLB fails (fragmentation)
if ptr == MAP_FAILED {
    let ptr = unsafe { mmap(...) }; // without MAP_HUGETLB
    unsafe { madvise(ptr, REGION_SIZE, MADV_HUGEPAGE) };
}
```

## Consequences

### Positive
- TLB miss rate drops by > 100× for region-scoped scans
- `MAP_HUGETLB` guarantees contiguous physical memory (no THP fragmentation)
- THP fallback handles the case where huge pages are exhausted

### Negative
- `MAP_HUGETLB` may fail under memory pressure (huge pages are a finite resource)
- THP can introduce latency spikes if `khugepaged` defragments at the wrong time
- 2 MB allocation granularity wastes memory for small tables

## Alternatives considered

1. **4 KB pages only** — TLB thrashing on large scans. Rejected.
2. **1 GB huge pages** — too large for region-level migration. Rejected.
3. **THP only (no `MAP_HUGETLB`)** — less reliable; THP may not always back the region. Rejected as primary, kept as fallback.

## References
- `docs/architecture/cpu-energy-kb.md` §4.2 (TLB miss cost)
- Linux man page, `mmap(2)`, `madvise(2)`
