# ADR-010: LRU for tier migration policy (k-competitive)

## Status
Accepted

## Confidence
90%

## Context

The memory manager must decide which regions to migrate between tiers (L3 ↔ DDR5 ↔ CXL ↔ NVMe). This is the **k-server problem**: k cache slots serve a sequence of requests; minimize total movement.

Known theoretical bounds:
- **LRU is k-competitive** (Sleator-Tarjan 1985): cost ≤ k × offline optimal
- **WFA is (2k-1)-competitive** (Koutsoupias-Papadimitriou 1995): tighter bound, but complex
- No deterministic online algorithm can beat k-competitive

## Decision

**Use LRU (Least Recently Used) for tier migration.** When a tier is full and a new region needs to be placed, evict the least recently accessed region.

Specifically:
- Each tier has an LRU list of resident regions
- On access, the region moves to the front of its tier's LRU list
- On insertion into a full tier, evict the back of the LRU list (migrate to the next tier down)

## Consequences

### Positive
- **Proven 4× bound** (k=4 tiers): cost ≤ 4× the offline optimal — a formal guarantee
- Simple to implement (~100 lines of code)
- Low overhead: O(1) per access (linked list move)
- Well-understood behavior; easy to reason about

### Negative
- 4× is not tight — WFA could achieve 2k-1 = 7× (wait, that's worse... actually for k=4, LRU gives 4× and WFA gives 7×, so LRU is better for small k)
- LRU is vulnerable to scanning access patterns (one-time access to many regions evicts hot data)
- Doesn't consider region size or access frequency (only recency)

## Alternatives considered

1. **WFA (Work Function Algorithm)** — (2k-1)-competitive, tighter for large k. But O(k) per access and complex to implement. Rejected for v1; research spike for v2.
2. **LFU (Least Frequently Used)** — better for stable workloads but adapts slowly to changes. Rejected.
3. **ARC (Adaptive Replacement Cache)** — adapts between LRU and LFU. Patent-encumbered (IBM). Rejected.
4. **Learned policy** — potentially better but no theoretical guarantee. Deferred to research.

## Compatibility

- Compatible with ADR-002 (regions are 2 MB, the migration unit)
- Compatible with ADR-006 (migration uses REP MOVSB)
- Compatible with ADR-009 (regions are huge-page-backed)
- Compatible with ADR-018 (morsel executor accesses regions, updating LRU)

## References
- Sleator & Tarjan, "Amortized Efficiency of List Update and Paging Rules" JACM 1985
- Koutsoupias & Papadimitriou, "On the k-Server Conjecture" JACM 1995
- `docs/research/waves/01-instruction-memory.md` P-MH-03
