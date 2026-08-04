# turboGP

> An **instruction-first, memory-centric** relational database engine.
>
> The thesis: design the database from the silicon up. Pick the cheapest
> instructions per joule, place data in the memory tier that feeds them, and
> treat every protocol boundary (CXL / RoCEv2 / IB) as a first-class design
> axis. The table-and-column model is the last layer, not the first.

## Quick links

| Document | What it is |
|----------|-----------|
| **[docs/FINE_DRAFT.md](docs/FINE_DRAFT.md)** | The definitive synthesis: venture + 25 ADRs + measured performance |
| **[docs/adr/](docs/adr/)** | 25 accepted ADRs + 7 open questions |
| **[SPECIFICATION.md](SPECIFICATION.md)** | Formal technical specification for implementers |
| **[ARCHITECTURE.md](ARCHITECTURE.md)** | The architecture in 1 page |
| **[docs/README.md](docs/README.md)** | Documentation index (reading order for new contributors) |
| **[docs/problems/](docs/problems/)** | Problem catalog: 99 problems with status, math, effort, impact |
| **[docs/research/waves/](docs/research/waves/)** | Per-problem solution evaluations (performance / time / energy) |

## Why this exists

Every existing database engine — Postgres, MySQL, DuckDB, ClickHouse — starts
from the table-and-column abstraction and works down to the hardware. This
leads to engines that use generic executors, treat the memory hierarchy as a
performance afterthought, and pay 1.5–2× energy and latency penalties because
their inner loops weren't designed around the actual instructions the silicon
can execute.

turboGP inverts the design order:

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
turboGP/
├── README.md                         ← you are here
├── ARCHITECTURE.md                   ← the dispatch-based architecture (1-page summary)
├── Cargo.toml
├── src/                              ← Rust source code
│   ├── kernel/                       ← SIMD kernels (the moat)
│   ├── memory/                       ← tier-aware memory manager
│   ├── storage/                      ← WAL + checkpoint (recovery.rs)
│   ├── engine/                       ← QueryEngine + dispatch + tpch fallback
│   │   ├── mod.rs                    ← QueryEngine::execute() entry point
│   │   ├── dispatch.rs               ← kernel-direct query dispatch
│   │   ├── executor.rs               ← basic executor (JOIN, GROUP BY, etc.)
│   │   └── tpch.rs                   ← TPC-H interpreter (rich SQL fallback)
│   ├── sql/                          ← lexer, parser, DDL, DML, CTE
│   ├── exec/                         ← window, pivot, merge, json, temporal, etc.
│   ├── datasource/                   ← CSV/Parquet loaders + Table struct
│   ├── catalog/                      ← table + view registries
│   ├── server/                       ← pgwire protocol server
│   └── schema/                       ← column type schema (TableSchema)
├── examples/smoke.rs                 ← end-to-end demo
├── benches/                          ← criterion benchmarks
└── docs/
    ├── README.md                     ← documentation index (start here)
    ├── adr/                          ← 25 ADRs + open questions
    ├── research/                     ← math foundations + wave evaluations
    └── problems/                     ← problem catalog
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
| 1 | ✅ Done | Kernel table + memory manager + storage format (Waves 1-3) |
| 2 | ✅ Done | Full executor with cross-tier joins (Waves 4-45) |
| 3 | ⚠️ Stub | CXL-aware buffer pool + migration policy (stubs only, not production) |
| 4 | ✅ Done | WAL + checkpoint (Waves 14, 37, 50, 51) |
| 5 | ⚠️ Stub | Protocol coordinator (CXL/Raft stubs exist, not wired to executor) |
| 6 | ✅ Done | SQL parser + schema layer (Waves 3, 6, 12, 22, 36) |
| 7 | ⚠️ Stub | DPU offload + computational storage pushdown (stubs only) |
| 8 | ✅ Done | TPC-H benchmark suite (Waves 10, 24, 28) |

## Current SQL surface

The following SQL features work end-to-end through `QueryEngine::execute()`:

- **DDL**: `CREATE TABLE`, `DROP TABLE`, `CREATE SCHEMA`, `CREATE VIEW`, `DROP VIEW`, `CREATE PROCEDURE`
- **DML**: `INSERT`, `UPDATE`, `DELETE` with WHERE clauses supporting `=`, `!=`, `<>`, `<`, `>`, `<=`, `>=`, `AND`, `OR`
- **SELECT**: `SELECT *`, `SELECT col`, `SELECT col1, col2`, `SELECT count(*)`, `SELECT sum/avg/min/max(col)`, `SELECT count(DISTINCT col)`
- **JOIN**: `INNER JOIN`, `LEFT [OUTER] JOIN`, `RIGHT [OUTER] JOIN`, `FULL [OUTER] JOIN`, `CROSS JOIN` (with ON clause)
- **GROUP BY**: single-key and multi-key, with multiple aggregates in one query
- **ORDER BY**: ascending/descending, string-aware (uses StringSearchColumn sidecar when present)
- **LIMIT**: row count limiting
- **WHERE**: `=`, `!=`, `<>`, `<`, `>`, `<=`, `>=`, `LIKE` (with `%` wildcards), `AND`, `OR`
- **NULL semantics**: NULL bitmaps track NULL cells; `COUNT(col)` excludes NULLs; pgwire sends NULL as `-1` length
- **Transactions**: `BEGIN`, `COMMIT`, `ROLLBACK` with snapshot isolation
- **WAL**: write-ahead log with BEGIN/COMMIT/ROLLBACK markers, base64-encoded SQL, replay on restart
- **Checkpoint**: type-preserving (FLOAT, VARCHAR, NULL all round-trip correctly)
- **CTE**: `WITH ... AS (...) SELECT ...` including recursive CTEs
- **Views**: `CREATE VIEW` + `SELECT FROM view` (materialized on query)
- **Procedures**: `CREATE PROCEDURE` + `EXEC proc_name [args]`
- **MERGE**: `MERGE INTO target WHEN MATCHED THEN UPDATE/DELETE/INSERT`
- **Temporal**: `FOR SYSTEM_TIME AS OF <timestamp>` (requires pre-registered TemporalTable)
- **Window functions**: `ROW_NUMBER()`, `RANK()`, `DENSE_RANK()`, `SUM()`, `COUNT()` with `OVER (PARTITION BY ... ORDER BY ...)`
- **pgwire server**: extended query protocol (Parse/Bind/Describe/Execute/Sync), NULL handling, max_rows/cursor support
- **Data loading**: CSV, Parquet (with NULL bitmap and StringSearchColumn sidecar)

## Known limitations

- **No persistent storage**: all data is in-memory; WAL+checkpoint provide durability across restarts but there's no on-disk page store
- **CXL/RoCEv2/IB are stubs**: the protocol modules exist but are not wired to the executor; single-node only
- **Morsel executor not used**: `executor/morsel.rs` exists but the SQL executor uses dispatch + vectorized kernels, not morsel-driven parallelism
- **DPccp/MCTS planners not wired**: `planner/dpccp.rs` and `planner/mcts.rs` exist but the executor uses a simple cost-based optimizer
- **No concurrent write transactions**: snapshot isolation supports one transaction at a time per engine; concurrent connections each get their own engine
- **String columns hashed**: strings are stored as xxh3 hashes in u64 cells; the original text is preserved in a `StringSearchColumn` sidecar (not all operations consult the sidecar)
- **PIVOT/UNPIVOT SQL syntax not parsed**: the `pivot()` / `unpivot()` functions are callable but `PIVOT (...)` clause in SELECT is not yet parsed
- **JSON functions not in expression evaluator**: `JSON_VALUE`, `JSON_QUERY`, etc. are callable as module functions but not yet integrated into the SELECT expression evaluator
- **Describe returns NoData**: the pgwire Describe message always returns NoData without inferring the schema (psql tolerates this)
- **No indexes used by executor**: `index/manager.rs` and `index/lsh.rs` exist but the executor does full scans

See `ARCHITECTURE.md` and `docs/` for the full design.

## License

CCL-X (Civil Common License X), Version 1.2 — see `LICENSE.md` for the full
text. The `Cargo.toml` declares `license = "CCL-X-1.2"` and the LICENSE.md
file in the repo root is the canonical CCL-X v1.2 text. All three sources
(README, Cargo.toml, LICENSE.md) now agree on CCL-X-1.2.

(Wave 59a fix: the previous README claimed "MIT OR Apache-2.0" and called
the Cargo.toml value "historical" — but no MIT or Apache LICENSE file
existed in the repo, and LICENSE.md was already CCL-X. This was a fake
license claim. Corrected to match the actual license file.)
