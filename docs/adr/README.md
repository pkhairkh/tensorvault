# Architecture Decision Records

> ADRs document the decisions where we have ≥80% confidence, chosen to be
> mutually compatible. Each ADR follows a standard format: context, decision,
> consequences, alternatives, confidence.
>
> Decisions below 80% confidence are in [OPEN_QUESTIONS.md](./OPEN_QUESTIONS.md).

## ADR index

| # | Title | Confidence | Status |
|---|-------|-----------|--------|
| [001](./001-64-bit-word-universal-storage.md) | Use 64-bit word as the universal storage unit | 95% | Accepted |
| [002](./002-page-region-tablet-hierarchy.md) | 4 KB page / 2 MB region / 2 GB tablet storage hierarchy | 95% | Accepted |
| [003](./003-cpuid-guarded-kernel-dispatch.md) | CPUID-guarded kernel dispatch for BMI2/AVX-512 | 95% | Accepted |
| [004](./004-branchless-hot-loops.md) | Branchless hot loops via mask accumulation + CMOV | 90% | Accepted |
| [005](./005-cache-line-alignment-for-atomics.md) | Cache-line alignment for all atomic-containing structs | 95% | Accepted |
| [006](./006-rep-movsb-for-bulk-copy.md) | REP MOVSB with ERMS for bulk page copy | 100% | Accepted |
| [007](./007-fixed-1024-cell-batch.md) | Fixed 1024-cell batch size for SIMD amortization | 85% | Accepted |
| [008](./008-numa-thread-pinning.md) | NUMA-aware thread pinning via pthread_setaffinity_np | 90% | Accepted |
| [009](./009-huge-pages-for-regions.md) | Transparent huge pages + explicit mmap for regions | 85% | Accepted |
| [010](./010-lru-tier-migration.md) | LRU for tier migration policy (k-competitive) | 90% | Accepted |
| [011](./011-zns-aware-wal.md) | ZNS-aware WAL via io_uring | 85% | Accepted |
| [012](./012-crc32c-page-checksum.md) | CRC32C + per-page XOR parity for checksum | 85% | Accepted |
| [013](./013-linear-typed-memory-handles.md) | Linear-typed memory handles (CxlRef, RaftRef) | 85% | Accepted |
| [014](./014-hlc-over-ptp.md) | HLC over PTP for clock synchronization | 80% | Accepted |
| [015](./015-empirical-bernstein-approximate-sql.md) | Empirical Bernstein + sequential stopping for (ε,δ) | 85% | Accepted |
| [016](./016-greedy-submodular-index-selection.md) | Greedy submodular maximization for index selection | 85% | Accepted |
| [017](./017-brute-vpopcntdq-then-lsh.md) | Similarity: brute VPOPCNTDQ ≤10⁶, LSH above | 85% | Accepted |
| [018](./018-data-centric-morsel-executor.md) | Data-centric morsel-driven pipeline execution | 90% | Accepted |
| [019](./019-dpccp-join-ordering.md) | DPccp for n≤15 joins, IDP for n>15 | 85% | Accepted |
| [020](./020-kingman-admission-control.md) | Kingman ρ-guard + token bucket for admission | 80% | Accepted |
| [021](./021-tpc-h-accept-loss.md) | TPC-H: run as-is, accept 1.2–1.5× loss | 95% | Accepted |
| [022](./022-rapl-energy-benchmarking.md) | RAPL + external meter for energy benchmarking | 85% | Accepted |
| [023](./023-calibrated-analytic-cost-model.md) | Calibrated analytic cost model (Kingman + measured AVX-512) | 85% | Accepted |
| [024](./024-mcdiarmid-eps-delta-joins.md) | McDiarmid bounded-differences for (ε,δ) through joins | 85% | Accepted |
| [025](./025-rans-cold-tier-only.md) | rANS compression for cold-tier columns only (CXL, NVMe) | 80% | Accepted |

**25 ADRs accepted. 7 open questions remain** (see [OPEN_QUESTIONS.md](./OPEN_QUESTIONS.md)).

## Compatibility matrix

These ADRs are chosen to be **mutually compatible** — no two accepted ADRs
conflict. Key compatibility relationships:

| ADRs | Why they're compatible |
|------|----------------------|
| 001 (64-bit word) + 004 (branchless) + 017 (VPOPCNTDQ) | All assume 64-bit lanes; VPCMPEQQ/VPOPCNTDQ operate on u64 |
| 002 (page/region/tablet) + 006 (REP MOVSB) + 009 (huge pages) | Region size = huge page = 2 MB; copy uses ERMS |
| 003 (CPUID dispatch) + 007 (1024 batch) + 018 (morsel) | Morsel size = batch size; kernel selected per CPU |
| 008 (NUMA pinning) + 010 (LRU migration) + 018 (morsel) | Morsels are NUMA-local; migration moves whole regions |
| 013 (linear types) + 014 (HLC) | Both are type/time system foundations; no overlap |
| 015 (Bernstein) + 016 (submodular) + 019 (DPccp) | All feed the planner; different decision points |
| 020 (Kingman admission) + 018 (morsel) | Admission controls query rate; morsel controls execution |
| 021 (TPC-H loss) + 022 (RAPL) | Honest baseline + honest energy measurement |

## ADR format

Each ADR follows this structure:

```
# ADR-NNN: Title

## Status
Accepted

## Confidence
XX% (with rationale)

## Context
[Why this decision is needed]

## Decision
[What we decided]

## Consequences
### Positive
### Negative

## Alternatives considered
[What else we looked at and why we didn't pick it]

## References
[Papers, docs, evidence]
```

## See also

- [OPEN_QUESTIONS.md](./OPEN_QUESTIONS.md) — decisions below 80% confidence
- [../ROUGH_DRAFT.md](../ROUGH_DRAFT.md) — the rough draft (will become the
  fine draft once all ADRs are settled)
- [../problems/](../problems/) — the full problem catalog
