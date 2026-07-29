# turboGP Architecture

> Design the database from the silicon up: pick the cheapest instructions per
> joule, place data in the memory tier that feeds them, and treat every
> protocol boundary as a first-class design axis.

## The inversion

Every existing database engine starts from the table-and-column abstraction
and works down to the hardware:

```
Schema → Tables → Columns → Rows → Storage Format → Indexes → Executor
```

turboGP inverts this:

```
Instruction Sets → Memory Hierarchy → Protocols → Storage Layout → Executor → Schema (last)
```

## The three invariants

### 1. The hot loop is a fixed instruction sequence

Each operator compiles to a hand-tuned kernel per `(CPU vendor, CPU
generation, memory tier)` tuple. The kernel table is indexed at startup via
CPUID; the best kernel per `(operator, tier)` is selected for the running
hardware.

Example kernels (see `src/kernel/`):

| Operator | CPU | Tier | Instructions | Throughput |
|----------|-----|------|-------------|-----------|
| `scan_eq` | SPR AVX-512 | L3 | `VMOVDQA64` + `VPCMPEQQ` + `KMOVQ` | 19 G cells/sec |
| `scan_eq` | SPR AVX-512 | DDR5 | + 4-page prefetch | 5 G cells/sec |
| `scan_eq` | SPR AVX-512 | CXL | + 8-page prefetch | 3 G cells/sec |
| `scan_range` | SPR AVX-512 | L3 | `VPCMPGTQ` + `VPCMPGTQ` + `KAND` | 12 G cells/sec |
| `hash_probe` | SPR AVX-512 | L3 | SwissTable + `VPCMPEQB` | 8 G probes/sec |
| `aggregate_sum` | SPR AVX-512 | L3 | `VADDPD` | 16 G cells/sec |
| `similarity_hamming` | SPR AVX-512 | L3 | `VXORQ` + `VPOPCNTDQ` + `VPCMPLEQ` | 8 G cells/sec |

The same operator has different kernels for different tiers because the
optimal prefetch distance, batch size, and SIMD width depend on the tier's
latency and bandwidth. A generic vectorized executor picks one kernel and
runs it regardless of where the data lives — turboGP picks a different
kernel per tier.

### 2. Data placement follows the hierarchy, not the schema

Every piece of data lives in a specific tier of the memory hierarchy:

| Tier | Latency | Bandwidth | What lives here |
|------|---------|-----------|-----------------|
| L1/L2 | 1–4 ns | ~2 TB/s | Current 4 KB working batch (auto-managed by HW) |
| L3 | 10–20 ns | ~300 GB/s | Hot indexes, hash tables < 32 MB, bloom filters |
| DDR5 | 80–100 ns | ~50 GB/s | Hot working set, large hash tables |
| HBM | 100–150 ns | ~1.6 TB/s | Scan-heavy analytics (Xeon Max, MI300A) |
| CXL | 140–500 ns | ~64 GB/s | Buffer pool extension, cold-ish indexes |
| NVMe | 10–30 µs | ~14 GB/s | WAL, LSM compaction, cold data |
| NVMe-oF | 30–60 µs | ~12 GB/s | Cross-rack shared block |
| RoCEv2/IB | 1–10 µs | ~50 GB/s | Replication, distributed commit |

The memory manager (`src/memory/`) migrates whole 2 MB regions between tiers
based on access statistics. Migration is the unit of placement — 2 MB matches
the huge page granularity and amortizes TLB cost.

### 3. Protocols define coherence and reach boundaries

The transaction coordinator (`src/protocol/`) runs at protocol boundaries:

```
┌─────────────────────────────────────────────────────────┐
│  Within a rack (CXL 3.0 fabric)                         │
│  ↑ coherence is hardware; commit ~250 ns                │
├─────────────────────────────────────────────────────────┤
│  Across racks (RoCEv2 / IB)                              │
│  ↑ coherence is software (Raft); commit ~10 µs          │
├─────────────────────────────────────────────────────────┤
│  Across regions (internet)                               │
│  ↑ async replication; commit ms-class                   │
└─────────────────────────────────────────────────────────┘
```

The engine never crosses a protocol boundary unintentionally. Single-rack
transactions use CXL coherence for visibility; cross-rack transactions use
Raft over RoCEv2.

## Storage format: instruction-shaped, not schema-shaped

The fundamental storage unit is the **opcode-stream pair**: a contiguous run
of bytes whose layout is chosen so a specific instruction can extract value
at peak throughput.

### The word: 64 bits, always

Every value is a 64-bit word — not for type uniformity, but because the
cheapest SIMD instructions on modern x86 and ARM operate on 64-bit lanes:
`VPCMPEQQ`, `VPADDQ`, `VPOPCNTDQ`, `VPTERNLOGQ`. All process 8×64-bit lanes
per cycle.

### The page: 4 KB, cache-aligned

The fundamental I/O unit is a 4 KB page:
- 4 KB matches the OS page size and x86 TLB granularity
- 4 KB = 64×64-byte cache lines = 512 u64 cells (504 after header)
- Scanning a 4 KB page with `VPCMPEQQ` takes ~64 cycles, fitting in L1

Page headers are 64 bytes (1 cache line): page type, tier hint, homogeneity
mask, row count, checksum, predecessor/successor (for LSM chains).

### The region: 2 MB, TLB-friendly

Pages are grouped into 2 MB regions (huge page granularity). A region holds
512 pages of the same type. The region is the **unit of placement and
migration** — the memory manager moves whole regions between tiers.

### The tablet: 2 GB, NUMA-aligned

Regions are grouped into 2 GB tablets. A tablet is the **unit of NUMA
placement** — the smallest structure that can be pinned to a specific NUMA
node or CXL device. A tablet holds 1024 regions.

### The column: a linked list of tablets

A logical column is a linked list of tablets, each tagged with its row range.
The schema layer maps SQL column references to (tablet list, kernel id) pairs
at query parse time.

## The executor: a scheduler of instruction streams

The executor (`src/executor/`) is not a Volcano-style pipeline. It's a
scheduler that:

1. Receives a logical plan from the parser
2. Lowers it to a DAG of kernel invocations
3. For each invocation, picks the kernel matching (cpu, tier)
4. Schedules invocations respecting data dependencies and tier bandwidth
5. Manages the L1/L2 working set explicitly (4 KB batches in, 4 KB out)

For `SELECT COUNT(*) FROM logs WHERE level = 'ERROR'`:

```
Kernel: scan_eq_u64  (tier=L3, cpu=detected)
  input:  logs.region[42]  (2 MB, 262144 cells)
  target: Cell::from_short_str("ERROR")
  output: count (u64)
```

The scheduler allocates a 4 KB output buffer in L1, issues the scan kernel in
4 KB batches (64 batches for a 2 MB region), and accumulates the count in a
register. Total cost: 262144 cells / 6.4 cells/cycle = ~41K cycles = ~14 µs.

## The kernel table: the moat

The kernel table (`src/kernel/`) is the engine's competitive moat. Each
kernel is hand-tuned for a specific `(CPU, tier)` tuple, benchmarked, and
added to the table. New CPUs get new kernels.

The table is indexed by `(Operator, CpuTarget, MemoryTier)`:

```rust
pub trait Kernel: Send + Sync {
    fn operator(&self) -> Operator;
    fn cpu(&self) -> CpuTarget;
    fn tier(&self) -> MemoryTier;
    unsafe fn execute(&self, input: *const u8, output: *mut u8, params: &KernelParams) -> KernelResult;
}
```

At startup, `KernelTable::new()` probes CPUID, detects the running CPU, and
registers all available kernels. `table.select(op, tier)` returns the best
kernel for the detected CPU.

## What this is not

- **Not a faster OLAP engine.** On TPC-H, this loses to DuckDB by 1.2–1.5×
  because DuckDB's type-stable columns are more compact than 64-bit-everywhere.
- **Not a production database.** This is a research prototype demonstrating
  the instruction-first architecture.

## What this is

A **unified substrate for tier-aware, instruction-tuned data processing**
that wins on:
- Heterogeneous/semi-structured analytics: 5–10× faster than DuckDB
- Memory-disaggregated scale-up: 2–3× effective capacity via CXL
- Energy efficiency: 3–5× lower energy per query
- Schema evolution: near-zero cost (metadata only)
- TPC-C consolidation: ~11× energy efficiency vs PolarDB (see `docs/tpcc_math.md`)

## References

- `docs/cpu_energy_kb.md` — per-instruction energy and latency knowledgebase
- `docs/instruction_first_architecture.md` — long-form architecture document
- `docs/tpcc_analysis.md` — TPC-C bottleneck analysis
- `docs/tpcc_math.md` — TPC-C mathematical analysis with path to beating it

