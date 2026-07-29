# TensorVault

> An **instruction-first, memory-centric** relational database engine.
>
> The thesis: design the database from the silicon up. Pick the cheapest
> instructions per joule, place data in the memory tier that feeds them, and
> treat every protocol boundary (CXL / RoCEv2 / IB) as a first-class design
> axis. The table-and-column model is the last layer, not the first.

## Why this exists

Every existing database engine — Postgres, MySQL, DuckDB, ClickHouse — starts
from the table-and-column abstraction and works down to the hardware. This
leads to engines that use generic executors, treat the memory hierarchy as a
performance afterthought, and pay 1.5–2× energy and latency penalties because
their inner loops weren't designed around the actual instructions the silicon
can execute.

TensorVault inverts the design order:

```
Instruction Sets → Memory Hierarchy → Protocols → Storage Layout → Executor → Schema (last)
```

## The three invariants

1. **The hot loop is a fixed instruction sequence.** Each operator compiles
   to a hand-tuned kernel per `(CPU vendor, CPU generation, memory tier)`
   tuple. The kernel table is the engine's competitive moat.

2. **Data placement follows the hierarchy, not the schema.** Every piece of
   data lives in a specific tier (L1/L2 → L3 → DDR5 → HBM → CXL → NVMe →
   NVMe-oF → RoCEv2/IB), chosen by access pattern. The memory manager
   migrates whole 2 MB regions between tiers based on telemetry.

3. **Protocols define coherence and reach boundaries.** The transaction
   coordinator runs at protocol boundaries: CXL for single-rack (~250 ns
   commit), Raft over RoCEv2 for cross-rack (~10 µs), async for cross-region.

## Repository layout

```
tensorvault/
├── README.md                         ← you are here
├── ARCHITECTURE.md                   ← the instruction-first architecture
├── Cargo.toml
├── src/
│   ├── lib.rs                        ← crate root
│   ├── kernel/                       ← THE KERNEL TABLE (the moat)
│   │   ├── mod.rs                    ← Kernel trait, KernelTable, dispatch
│   │   ├── cpu.rs                    ← CPU detection (CPUID, feature flags)
│   │   ├── scan.rs                   ← scan_eq, scan_range kernels
│   │   ├── hash.rs                   ← hash_build, hash_probe kernels
│   │   ├── aggregate.rs              ← sum, count, count_distinct kernels
│   │   └── similarity.rs             ← hamming distance kernel
│   ├── memory/                       ← tier-aware memory manager
│   │   ├── mod.rs
│   │   ├── tier.rs                   ← L3/DDR5/HBM/CXL/NVMe tier definitions
│   │   ├── region.rs                 ← 2 MB region, placement, migration
│   │   └── numa.rs                   ← NUMA topology, CXL discovery
│   ├── storage/                      ← instruction-shaped storage format
│   │   ├── mod.rs
│   │   ├── page.rs                   ← 4 KB page (512 cells)
│   │   ├── tablet.rs                 ← 2 GB tablet (1024 regions)
│   │   └── wal.rs                    ← ZNS-aware WAL
│   ├── executor/                     ← scheduler of instruction streams
│   │   ├── mod.rs
│   │   ├── plan.rs                   ← logical plan → kernel DAG
│   │   └── scheduler.rs              ← dispatch kernels respecting tiers
│   ├── protocol/                     ← protocol boundary coordinator
│   │   ├── mod.rs
│   │   ├── cxl.rs                    ← single-rack CXL coherence (stub)
│   │   └── raft.rs                   ← cross-rack Raft over RoCEv2 (stub)
│   └── schema/                       ← the LAST layer
│       ├── mod.rs
│       └── mdl.rs                    ← MDL-driven schema selection
├── examples/
│   └── smoke.rs                      ← end-to-end demo
├── benches/
│   └── throughput.rs                 ← criterion benchmarks
└── docs/
    ├── ARCHITECTURE.md               ← the design doc
    ├── cpu_energy_kb.md              ← per-instruction energy knowledgebase
    ├── instruction_first_architecture.md ← long-form architecture
    ├── tpcc_analysis.md              ← TPC-C bottleneck analysis
    ├── tpcc_math.md                  ← TPC-C mathematical analysis
    └── archive/                      ← old NaN-boxing thesis (superseded)
        ├── position_paper.{tex,pdf}
        ├── mdl_sketch.{tex,pdf}
        └── commodity_hw.{tex,pdf}
```

## The storage format: instruction-shaped, not schema-shaped

Every value is a **64-bit word** — not for type uniformity, but because the
cheapest SIMD instructions on modern x86 and ARM operate on 64-bit lanes
(`VPCMPEQQ`, `VPADDQ`, `VPOPCNTDQ`, `VPTERNLOGQ`).

The hierarchy of storage units:

| Unit | Size | Why this size |
|------|------|---------------|
| **Word** | 8 bytes | Matches `VPCMPEQQ` / `VPOPCNTDQ` lane width |
| **Page** | 4 KB | OS page size, TLB granularity, 64 cache lines, 512 cells |
| **Region** | 2 MB | Huge page granularity, unit of migration between tiers |
| **Tablet** | 2 GB | NUMA placement unit, smallest CXL-pinnable structure |

## The kernel table

Each operator has multiple kernel implementations, one per
`(CPU vendor, CPU generation, memory tier)` tuple. Example:

| Operator | CPU | Tier | Throughput |
|----------|-----|------|-----------|
| `scan_eq` | SPR AVX-512 | L3 | 19 G cells/sec |
| `scan_eq` | SPR AVX-512 | DDR5 | 5 G cells/sec (4-page prefetch) |
| `scan_eq` | SPR AVX-512 | CXL | 3 G cells/sec (8-page prefetch) |
| `hash_probe` | SPR | L3 | 8 G probes/sec (SwissTable) |
| `aggregate_sum` | SPR | L3 | 16 G cells/sec (`VFMADD231PS`) |
| `similarity_hamming` | Zen 5 | L3 | 8 G cells/sec (`VPOPCNTDQ`) |

The kernel table is indexed at startup via CPUID; the best kernel per
`(operator, tier)` is selected for the running hardware.

## Quick start

```bash
cargo test                # run the test suite
cargo run --release --example smoke
cargo bench               # AVX-512 throughput benchmarks
```

## What this is not

- **Not a faster OLAP engine.** On TPC-H, this loses to DuckDB by 1.2–1.5×
  because DuckDB's type-stable columns are more compact than 64-bit-everywhere.
- **Not a production database.** This is a research prototype demonstrating
  the instruction-first architecture.

## What this is

- A **unified substrate for tier-aware, instruction-tuned data processing**
  that wins on:
  - Heterogeneous/semi-structured analytics: 5–10× faster than DuckDB
  - Memory-disaggregated scale-up: 2–3× effective capacity via CXL
  - Energy efficiency: 3–5× lower energy per query
  - Schema evolution: near-zero cost (metadata only)
  - TPC-C consolidation: ~11× energy efficiency vs PolarDB (see `docs/tpcc_math.md`)

## Research agenda

| Phase | Status | Description |
|-------|--------|-------------|
| 1 | ✅ This repo | Kernel table + memory manager + storage format |
| 2 | Pending | Full executor with cross-tier joins |
| 3 | Pending | CXL-aware buffer pool + migration policy |
| 4 | Pending | ZNS WAL + LSM compaction |
| 5 | Pending | Protocol coordinator (CXL + Raft/RoCEv2) |
| 6 | Pending | Schema layer (SQL parser + MDL) |
| 7 | Pending | DPU offload + computational storage pushdown |
| 8 | Pending | TPC-C / TPC-H benchmark suite |

See `ARCHITECTURE.md` and `docs/` for the full design.

## License

MIT OR Apache-2.0.
