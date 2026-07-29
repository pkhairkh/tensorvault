# turboGP Technical Specification

> **Formal specification of the instruction-first, memory-centric database
> engine.** This document defines the interfaces, data structures, algorithms,
> and protocols. It is the reference for implementers.
>
> **Status**: v0.2.0 — research prototype
> **Authority**: [25 ADRs](./adr/) (≥80% confidence) + [7 open questions](./adr/OPEN_QUESTIONS.md)
> **Related**: [FINE_DRAFT.md](./FINE_DRAFT.md) (narrative), [ARCHITECTURE.md](./ARCHITECTURE.md) (summary)

---

## Table of Contents

1. [Scope and Conformance](#1-scope-and-conformance)
2. [Terminology](#2-terminology)
3. [Storage Format](#3-storage-format)
4. [Kernel Table](#4-kernel-table)
5. [Memory Manager](#5-memory-manager)
6. [Write-Ahead Log](#6-write-ahead-log)
7. [Executor](#7-executor)
8. [Protocol Coordinator](#8-protocol-coordinator)
9. [Schema Layer](#9-schema-layer)
10. [Query Language](#10-query-language)
11. [Cost Model](#11-cost-model)
12. [Calibration](#12-calibration)

---

## 1. Scope and Conformance

### 1.1 Scope

This specification defines the turboGP database engine: a tier-aware,
instruction-tuned, schema-fluid relational engine. It covers:

- The on-disk and in-memory storage format
- The kernel table API and dispatch mechanism
- The memory hierarchy manager
- The write-ahead log
- The morsel-driven executor
- The protocol boundary coordinator
- The schema layer with MDL selection
- The SQL extension syntax
- The cost model

### 1.2 Conformance levels

- **MUST**: required for conformance (violations are bugs)
- **SHOULD**: recommended (deviations must be documented)
- **MAY**: optional (implementation choice)

### 1.3 Target hardware

| Platform | Status |
|----------|--------|
| x86-64 with AVX-512 (Ice Lake+, Zen 4+) | Primary target — fully supported |
| x86-64 with AVX2 (Haswell+) | Supported — 2–3× slower than AVX-512 |
| x86-64 scalar only | Fallback — always works |
| ARM64 with SVE2 (Graviton 4, Neoverse V2) | Planned — kernels not yet implemented (OQ-07) |
| ARM64 with NEON | Planned — fallback for SVE2 |
| RISC-V with RVV | Future — not planned for v1 |

### 1.4 Non-goals

- TPC-H parity with DuckDB (ADR-021: accept 1.2–1.5× loss)
- Distributed scale-out (Cassandra/Spanner style)
- OLTP point-query optimization beyond single-box
- Full SQL standard compliance (extensions are opt-in)

---

## 2. Terminology

| Term | Definition |
|------|-----------|
| **Cell** | A single 64-bit word — the universal storage unit (ADR-001) |
| **Page** | 4096 bytes — 64-byte header + 504 cells (ADR-002) |
| **Region** | 2 MB — 512 pages — the migration unit (ADR-002) |
| **Tablet** | 2 GB — 1024 regions — the NUMA placement unit (ADR-002) |
| **Kernel** | A hand-tuned instruction sequence for one (operator, CPU, tier) tuple |
| **Kernel table** | The registry of all kernels, indexed by (operator, CPU, tier) |
| **Morsel** | A 1024-cell batch processed by one worker thread (ADR-007, ADR-018) |
| **Tier** | A level of the memory hierarchy (L1/L2, L3, DDR5, HBM, CXL, NVMe, Network) |
| **(ε, δ) guarantee** | The result is within ε of the true value with probability ≥ 1−δ |

---

## 3. Storage Format

### 3.1 Cell (ADR-001)

```rust
/// A 64-bit word. The universal storage unit.
/// Every value in every column is stored as a Cell.
#[repr(transparent)]
pub struct Cell(pub u64);
```

**MUST**: All column data is stored as `Vec<u64>`. Type information is metadata.

**MUST**: NaN-boxing is used for inline type tagging:
- `0x0000_0000_0000_0000` = NULL
- Exponent ≠ 0x7FF, ≠ 0 = real f64 (identity boxing)
- `0xFFF0_xxxx_xxxx_xxxx` = tagged i32
- `0xFFF1_xxxx_xxxx_xxxx` = tagged bool
- `0xFFF2_xxxx_xxxx_xxxx` = tagged pointer (48-bit)
- `0xFFF3_xxxx_xxxx_xxxx` = tagged date (i32 days)
- `0x0001_xxxx_xxxx_xxxx` = short string (≤6 bytes, subnormal namespace)

### 3.2 Page (ADR-002)

```rust
/// Page size: 4096 bytes.
pub const PAGE_SIZE: usize = 4096;

/// Header size: 64 bytes (1 cache line).
pub const HEADER_SIZE: usize = 64;

/// Cells per page: (4096 - 64) / 8 = 504.
pub const PAGE_CELLS: usize = (PAGE_SIZE - HEADER_SIZE) / 8;

/// Page header — 64 bytes, cache-line aligned.
#[repr(C)]
pub struct PageHeader {
    pub page_type: u64,       // kernel ID for this page's data
    pub tier_hint: u64,       // preferred memory tier
    pub homogeneity: u64,     // bitmask of present cell types
    pub row_count: u64,       // number of valid cells
    pub checksum: u64,        // CRC32C of cell data
    pub parity: u64,          // XOR parity of cell data
    pub predecessor: u64,     // previous page in LSM chain (0 = none)
    pub successor: u64,       // next page in LSM chain (0 = none)
}
```

**MUST**: Pages are 64-byte aligned (cache line).

**MUST**: CRC32C checksum is computed via SSE4.2 `_mm_crc32_u64`.

**SHOULD**: XOR parity is computed by XOR-ing all 8-byte words in the cell data.

### 3.3 Region (ADR-002)

```rust
/// Region size: 2 MB (matches huge page).
pub const REGION_SIZE: usize = 2 * 1024 * 1024;

/// Pages per region: 2 MB / 4 KB = 512.
pub const REGION_PAGES: usize = REGION_SIZE / PAGE_SIZE;
```

**MUST**: Regions are allocated via `mmap` with `MAP_HUGETLB` (ADR-009).

**MUST**: Each region carries access statistics (reads, writes, last access time).

### 3.4 Tablet (ADR-002)

```rust
/// Tablet size: 2 GB (NUMA placement unit).
pub const TABLET_SIZE: usize = 2 * 1024 * 1024 * 1024;

/// Regions per tablet: 2 GB / 2 MB = 1024.
pub const TABLET_REGIONS: usize = TABLET_SIZE / REGION_SIZE;
```

**MUST**: Tablets are pinned to a specific NUMA node.

### 3.5 Compression (ADR-025)

**MUST**: L3 and DDR5 tiers store pages uncompressed.

**MUST**: CXL and NVMe tiers MAY store pages rANS-compressed. The page header
bit 0 of `page_type` indicates compression:
- `page_type & 0x1 == 0` → uncompressed
- `page_type & 0x1 == 1` → rANS-compressed (8 interleaved streams)

**MUST**: The scheduler transparently decodes compressed pages before running
scan kernels.

---

## 4. Kernel Table

### 4.1 Kernel trait (ADR-003)

```rust
/// Identifies an operator that kernels implement.
pub enum Operator {
    ScanEqU64,
    ScanRangeU64,
    HashBuild,
    HashProbe,
    AggregateSumF64,
    AggregateCountDistinct,
    SimilarityHamming,
}

/// Parameters passed to a kernel at execution time.
pub struct KernelParams {
    pub target_u64: u64,
    pub low_u64: u64,
    pub high_u64: u64,
    pub max_distance: u32,
    pub cell_count: usize,
}

/// Result of a kernel execution.
pub struct KernelResult {
    pub count: u64,
    pub sum: f64,
    pub mask: u64,
}

/// A kernel: a hand-tuned instruction sequence for (CPU, tier).
pub trait Kernel: Send + Sync {
    fn operator(&self) -> Operator;
    fn cpu(&self) -> CpuTarget;
    fn tier(&self) -> MemoryTier;
    fn name(&self) -> &'static str;
    unsafe fn execute(
        &self,
        input: *const u8,
        output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult;
}
```

### 4.2 CPU detection (ADR-003)

```rust
pub enum CpuTarget {
    Scalar,      // always available
    X86Avx2,     // Haswell+ (2013)
    X86Avx512,   // Ice Lake+ / Zen 4+ (2019)
    ArmNeon,     // all ARM64
    ArmSve,      // Neoverse V2 / Graviton 4
}
```

**MUST**: At startup, `detect_cpu()` probes CPUID and returns the highest
available target.

**MUST**: BMI2 (`PEXT`/`PDEP`) is guarded separately:
- Intel and Zen 3+: use hardware PEXT (3 cycles)
- Zen/Zen2: use software fallback (do NOT use microcoded PEXT — 18 cycles)

### 4.3 Kernel table dispatch

```rust
pub struct KernelTable {
    kernels: HashMap<(Operator, CpuTarget, MemoryTier), Arc<dyn Kernel>>,
    detected_cpu: CpuTarget,
}
```

**MUST**: `KernelTable::new()` registers all built-in kernels for the
detected CPU.

**MUST**: `table.select(op, tier)` returns the best kernel:
1. Try exact match `(op, detected_cpu, tier)`
2. Fall back to `(op, Scalar, tier)`
3. Last resort: any kernel for `op`

### 4.4 Required kernels (v1)

| Operator | Scalar | AVX2 | AVX-512 | Key instruction |
|----------|--------|------|---------|----------------|
| ScanEqU64 | ✅ | ✅ | ✅ | `VPCMPEQQ` + `KMOVQ` |
| ScanRangeU64 | ✅ | — | ✅ | `VPCMPGEQ` + `VPCMPLEQ` + `KAND` |
| HashBuild | ✅ | — | — | (SwissTable construction) |
| HashProbe | ✅ | — | ✅ | `VPCMPEQB` on metadata |
| AggregateSumF64 | ✅ | ✅ | ✅ | `VADDPD` |
| AggregateCountDistinct | ✅ | — | — | (HashSet — HLL planned) |
| SimilarityHamming | ✅ | — | ✅ | `VXORQ` + `VPOPCNTDQ` |

### 4.5 Branchless requirement (ADR-004)

**MUST**: All hot-loop kernels use branchless patterns:
```rust
// BAD — branch on every cell
if cell == target { count += 1; }

// GOOD — branchless via mask
count += (cell == target) as u64;
```

**MUST**: SIMD tail loops use mask accumulation + `POPCNT`, not conditional
branches.

### 4.6 Alignment requirement (ADR-005)

**MUST**: All structs containing `Atomic*` fields are `#[repr(align(64))]`.

**MUST**: CI runs with Linux `split_lock_detect=fatal` to catch regressions.

---

## 5. Memory Manager

### 5.1 Memory tiers

```rust
pub enum MemoryTier {
    L1L2,    // ~1–4 ns, per-core, auto-managed
    L3,      // ~15 ns, per-socket, 32–192 MB
    Ddr5,    // ~90 ns, per-socket, 256 GB–1 TB
    Hbm,     // ~120 ns, on-package, 64–128 GB (Xeon Max, MI300A)
    Cxl,     // ~250 ns, rack-local, 1–8 TB (not yet available — OQ-02)
    Nvme,    // ~20 µs, rack-local, 10–100 TB
    NvmeOf,  // ~50 µs, cross-rack
    Network, // ~5 µs RTT, cross-rack
}
```

Each tier has characteristic latency, bandwidth, and energy (see
[`architecture/cpu-energy-kb.md`](./architecture/cpu-energy-kb.md) §2).

### 5.2 NUMA topology detection

**MUST**: At startup, `NumaTopology::detect()` reads
`/sys/devices/system/node/` and classifies each NUMA node:
- Node with CPUs → `Ddr5`
- Memory-only node → `Cxl`
- HBM node (Xeon Max) → `Hbm`

**MUST**: Thread pinning uses `pthread_setaffinity_np` (Linux) to pin
worker threads to the NUMA node of their data (ADR-008).

### 5.3 Region allocation (ADR-009)

**MUST**: Regions are allocated via `mmap` with `MAP_HUGETLB | MAP_PRIVATE
| MAP_ANONYMOUS`.

**SHOULD**: If `MAP_HUGETLB` fails (fragmentation), fall back to `mmap`
without `MAP_HUGETLB` + `madvise(MADV_HUGEPAGE)`.

### 5.4 Tier migration (ADR-010)

**MUST**: Migration policy is LRU (Least Recently Used):
- Each tier has an LRU list of resident regions
- On access, the region moves to the front
- On insertion into a full tier, evict the back (migrate to next tier down)

**MUST**: Migration uses `ptr::copy_nonoverlapping` (lowered to `REP MOVSB`
with ERMS on x86-64).

**GUARANTEE**: LRU is k-competitive (Sleator-Tarjan 1985) — cost ≤ k ×
offline optimal, where k = number of tiers.

### 5.5 Placement policy

**SHOULD**: New regions are placed using a hot-first policy:
1. If the region is an index or hash table → L3
2. If the region is hot working set → DDR5
3. If the region is cold → CXL (if available) or NVMe

**MAY**: An LP-based placement policy MAY be used for offline optimization
(not in v1).

---

## 6. Write-Ahead Log

### 6.1 WAL format (ADR-011)

**MUST**: The WAL is append-only. Each record:

```
[magic "TVW1" (4 bytes)]
[record length (4 bytes, LE u32)]
[txn_id (8 bytes, LE u64)]
[record_type (1 byte): 0=commit, 1=abort, 2=data]
[inserted_ns (8 bytes, LE u64)]
[checksum (8 bytes, LE u64, xxh3 of body)]
[body (variable)]
```

### 6.2 ZNS support (ADR-011)

**MUST**: If the storage device is ZNS (`ioctl(BLKGETZONESZ)` succeeds):
- Allocate zones explicitly via `ioctl(BLKOPENZONE)`
- Write sequentially within a zone
- Finish a zone when full via `ioctl(BLKFINISHZONE)`
- Never overwrite — old zones are reset in bulk after checkpoint

**MUST**: If the storage device is not ZNS, fall back to `io_uring` with
`O_DIRECT`.

### 6.3 Group commit

**SHOULD**: The WAL batches commits and flushes every N milliseconds or
when the buffer is full, whichever comes first. N is configurable
(default: 1 ms).

### 6.4 Recovery

**MUST**: On startup, `Wal::open()` scans the WAL forward, validates each
record's checksum, and truncates at the first corrupt/truncated record.

---

## 7. Executor

### 7.1 Morsel-driven pipeline (ADR-018)

**MUST**: The executor uses data-centric morsel-driven parallelism:
- A **morsel** = 1024 cells (8 KB, fits in L1)
- Each worker thread is NUMA-pinned (ADR-008)
- A morsel is dispatched to the worker on the data's NUMA node
- The worker runs the full pipeline (scan → filter → aggregate) on one
  morsel, keeping intermediate data in L1/L2

**MUST**: Pipeline breakers (hash join build, sort) materialize to DRAM.

### 7.2 Plan lowering

```rust
pub enum PlanNode {
    Scan { region_id: RegionId, operator: Operator, params: KernelParams },
    Aggregate { child: Box<PlanNode>, operator: Operator },
    Join { left: Box<PlanNode>, right: Box<PlanNode>, operator: Operator },
    Materialize { child: Box<PlanNode>, target_region: RegionId },
}
```

**MUST**: `lower_to_kernels(plan)` converts a `LogicalPlan` to a list of
`KernelInvocation`s.

### 7.3 Join ordering (ADR-019)

**MUST**: Join ordering uses:
- DPccp (exact) for n ≤ 15 joins
- IDP (block size k=8) for 16 ≤ n ≤ 40
- Greedy GOO for n > 40

**MUST**: The cost model (§11) provides per-join cost estimates.

### 7.4 Admission control (ADR-020)

**MUST**: Admission control uses a two-layer policy:
1. **Kingman ρ-guard**: reject new queries if predicted ρ > 0.8
2. **Token bucket**: capacity = 2 × max concurrent queries, refill = 0.7 × μ

**MUST**: Rejected queries return HTTP 503 with `Retry-After` header.

---

## 8. Protocol Coordinator

### 8.1 Protocol boundaries

```
Within a rack (CXL 3.0 fabric)     → hardware coherence, ~250 ns commit
Across racks (RoCEv2 / IB)         → software coherence (Raft), ~10 µs commit
Across regions (internet)          → async replication, ms-class
```

### 8.2 Linear-typed handles (ADR-013)

**MUST**: Two wrapper types enforce protocol safety at compile time:

```rust
/// Linear reference to CXL-resident data. Cannot be duplicated.
/// Cannot escape the rack scope (!Send + !Sync).
pub struct CxlRef<T> { /* no Clone, no Copy */ }

/// Affine reference to cross-rack data. Can be dropped, not duplicated.
pub struct RaftRef<T> { /* no Clone, no Copy */ }
```

**MUST**: Code that tries to send a `CxlRef` across a Raft boundary fails
to compile.

### 8.3 Clock (ADR-014)

**MUST**: Timestamps are HLC (Hybrid Logical Clocks) over PTP:
- Physical component: PTP-synced nanosecond clock (~100 µs accuracy)
- Logical component: Lamport counter for tie-breaking
- Timestamp: `(physical_ns: u64, logical: u64)`

**MUST**: No commit-wait (unlike Spanner's TrueTime).

### 8.4 CXL coordinator (OQ-02 — blocked)

**MAY**: If CXL hardware is available, single-rack transactions commit via
CXL.mem shared commit record (`cmpxchg16b`). ~200–500 ns per commit.

**MUST**: If CXL is unavailable, single-rack transactions commit via local
NVMe WAL (fallback, ~20 µs per commit).

### 8.5 Raft coordinator (OQ-04 — partially specified)

**SHOULD**: Cross-rack transactions use Raft over RoCEv2.
- Leader replicates log entries via RDMA writes
- Quorum = floor(N/2) + 1
- Commit latency: ~5–15 µs (RDMA RTT + local NVMe log)

**MUST**: If RDMA is unavailable, fall back to Raft over TCP (~50–100 µs).

---

## 9. Schema Layer

### 9.1 MDL schema selection

**MUST**: For schema-on-read columns, the type interpretation is chosen by
minimizing description length:

```
L(τ) = L_model(τ) + L_data(data | τ)
```

Where:
- `L_model(τ)` = 16 bits (type tag) + metadata
- `L_data(data | τ)` = n × value_bits(τ) (0 if mismatch → infinite)

The candidate types are: F64 (64 bits), I32 (32 bits), Bool (8 bits),
Null (0 bits), Variant (80 bits = 16 tag + 64 payload).

**MUST**: `schema_select(column)` returns the `TypeInterpretation` with
minimum `L(τ)`.

### 9.2 Schema evolution

**MUST**: v1 uses SQL DDL (`ALTER TABLE ADD COLUMN`, `DROP COLUMN`).
Schema changes are metadata-only — no storage rewrite.

**MAY**: v2 MAY use functorial data migration (Spivak's Σ ⊣ Δ ⊣ Π) —
deferred (OQ-05, 35% confidence).

---

## 10. Query Language

### 10.1 Standard SQL

**MUST**: The engine accepts standard SQL `SELECT`, `FROM`, `WHERE`,
`GROUP BY`, `ORDER BY`, `JOIN`, `INSERT`, `UPDATE`, `DELETE`.

### 10.2 Extension: APPROXIMATE (ADR-015, ADR-024)

```sql
SELECT AVG(col) APPROXIMATE WITHIN <ε> CONFIDENCE <1-δ> FROM table;
```

**MUST**: The planner picks the minimal-cost sketch whose theorem guarantees
(ε, δ). Options:
- AVG/SUM: empirical Bernstein with sequential stopping
- COUNT DISTINCT: HyperLogLog (RSE = 1.04/√m)
- PERCENTILE: t-Digest
- Heavy hitters: Count-Min sketch

**MUST**: (ε, δ) propagates through the operator DAG via McDiarmid's
inequality (ADR-024): `ε_join = √(ε_R² + ε_S² · σ²)` where σ is join
selectivity.

### 10.3 Extension: TIER

```sql
SELECT * FROM table TIER <L3|DDR5|CXL|NVME> WHERE ...;
```

**MUST**: If the data is not in the requested tier, the engine either
migrates it or returns an error.

### 10.4 Extension: SIMILAR TO (ADR-017)

```sql
SELECT * FROM table
WHERE col SIMILAR TO <target> WITHIN HAMMING DISTANCE <k>;

SELECT a.id, b.id FROM t1 a JOIN t1 b
ON a.col SIMILAR TO b.col WITHIN HAMMING DISTANCE <k>;
```

**MUST**: For ≤ 10⁶ cells, use brute-force `VPOPCNTDQ` kernel.
**MUST**: For > 10⁶ cells, use LSH (Andoni-Indyk) with re-ranking.

### 10.5 Extension: CONSISTENCY

```sql
SELECT * FROM table CONSISTENCY <STRONG|READ_COMMITTED|EVENTUAL>;
```

**MUST**: STRONG → CXL or local DDR5; READ_COMMITTED → NVMe with flush;
EVENTUAL → cross-region async replica.

### 10.6 Extension: SCOPE

```sql
BEGIN TRANSACTION SCOPE <RACK|REGION|GLOBAL ASYNC>;
  -- statements
COMMIT;
```

**MUST**: SCOPE RACK → CXL coherence (or local fallback).
**MUST**: SCOPE REGION → Raft over RoCEv2.
**MUST**: SCOPE GLOBAL ASYNC → async replication.

### 10.7 Extension: USING

```sql
SELECT COUNT(DISTINCT col) USING <HYPERLOGLOG|COUNT_MIN|T_DIGEST> FROM table;
```

**MUST**: Forces the planner to use the specified sketch.

### 10.8 Extension: MEMORY BUDGET

```sql
SELECT * FROM table MEMORY BUDGET <n> <GB|MB> WHERE ...;
```

**MUST**: The plan must fit in the specified memory. If it can't, spill to
the next tier down.

### 10.9 Extension: ENERGY BUDGET

```sql
SELECT * FROM table ENERGY BUDGET <n> JOULES WHERE ...;
```

**SHOULD**: The plan must not exceed the specified energy. Enforced via
RAPL measurement (ADR-022) or analytical model.

---

## 11. Cost Model (ADR-023)

### 11.1 Formula

```
T_query = Σ_kernels ( T_compute + W_Kingman )
```

Where:
```
T_compute = n_cells / (throughput(kernel, tier) × f_cpu)
W_Kingman = (ρ / (1-ρ)) × ((c_a² + c_s²) / 2) × (1/μ)
```

### 11.2 Throughput model

For L3-resident data:
```
throughput = simd_lanes × f_cpu
```

For DRAM-resident data:
```
throughput = BW_memory / cell_size
```

### 11.3 Measured calibration (Zen 5, 2.0 GHz, 4 cores)

| Kernel | L3 throughput | DRAM throughput |
|--------|--------------|-----------------|
| scan_eq AVX-512 | 24.1 G cells/sec | 5.0 G cells/sec |
| scan_eq AVX2 | 15.4 G cells/sec | 5.0 G cells/sec |
| sum_f64 AVX-512 | 29.8 G cells/sec | 5.0 G cells/sec |
| hamming VPOPCNTDQ | 24.2 G cells/sec | 5.0 G cells/sec |

Validation: measured AVX-512 throughput matches theoretical (8 × 3 GHz = 24 G)
within 5%.

### 11.4 Kingman parameters

| Parameter | Source |
|-----------|--------|
| ρ (utilization) | λ/μ, measured at runtime |
| c_a (arrival CV) | measured from request inter-arrival times |
| c_s (service CV) | measured from kernel execution times |
| μ⁻¹ (mean service) | from the throughput model above |

---

## 12. Calibration

### 12.1 Calibration benchmark

**MUST**: Each new CPU is calibrated by running `examples/bench_kernel.rs`,
which measures:
- Per-kernel throughput (L3-resident and DRAM-resident)
- Memory read bandwidth
- REP MOVSB copy bandwidth

**MUST**: Results are stored in a calibration JSON:
```json
{
  "cpu": "amd-epyc-turin",
  "clock_ghz": 2.0,
  "cores": 4,
  "kernels": {
    "scan_eq_avx512_l3": { "throughput_mps": 24099 },
    "sum_f64_avx512_l3": { "throughput_mps": 29802 }
  },
  "memory_bw_gbps": 40.63,
  "copy_bw_gbps": 21.65
}
```

### 12.2 Energy measurement (ADR-022)

**MUST**: Energy is measured via:
1. Intel RAPL (`perf stat -e power/energy-pkg/`) — primary on Intel
2. Analytical model from `cpu-energy-kb.md` — fallback on AMD/ARM
3. External meter (Hioki) — calibration anchor, quarterly

**MUST**: Report joules per query, queries per joule, and tpmC per watt.

---

## Appendix A: ADR Cross-Reference

| ADR | Section | Decision |
|-----|---------|---------|
| 001 | §3.1 | 64-bit word |
| 002 | §3.2–3.4 | Page/region/tablet hierarchy |
| 003 | §4.2–4.3 | CPUID-guarded kernel dispatch |
| 004 | §4.5 | Branchless hot loops |
| 005 | §4.6 | Cache-line alignment for atomics |
| 006 | §5.4 | REP MOVSB for bulk copy |
| 007 | §7.1 | 1024-cell batch size |
| 008 | §5.2 | NUMA thread pinning |
| 009 | §5.3 | Huge pages for regions |
| 010 | §5.4 | LRU tier migration |
| 011 | §6.1–6.2 | ZNS-aware WAL |
| 012 | §3.2 | CRC32C + XOR parity |
| 013 | §8.2 | Linear-typed memory handles |
| 014 | §8.3 | HLC over PTP |
| 015 | §10.2 | Empirical Bernstein for (ε,δ) |
| 016 | §9.1 | Greedy submodular index selection |
| 017 | §10.4 | Brute VPOPCNTDQ then LSH |
| 018 | §7.1 | Data-centric morsel executor |
| 019 | §7.3 | DPccp join ordering |
| 020 | §7.4 | Kingman admission control |
| 021 | §1.4 | TPC-H accept loss |
| 022 | §12.2 | RAPL energy benchmarking |
| 023 | §11 | Calibrated analytic cost model |
| 024 | §10.2 | McDiarmid (ε,δ) propagation |
| 025 | §3.5 | rANS cold-tier compression |

## Appendix B: Open Questions Cross-Reference

| OQ | Section | Blocker |
|----|---------|---------|
| 02 | §8.4 | CXL commit — no CXL hardware |
| 04 | §8.5 | Raft implementation — openraft eval |
| 05 | §9.2 | Schema migration — research risk |
| 07 | §1.3 | ARM port — no Graviton 4 access |
| 08 | §7.1 | CXL spill — no CXL hardware |
| 09 | §7.1 | Trace JIT — Cranelift prototype |
| 10 | §8.5 | Distributed TX — Calvin constraints |
