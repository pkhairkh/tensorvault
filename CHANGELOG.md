# Changelog

All notable changes to turboGP are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

## [1.0.0-remediated] — 2026-08-04

### Production readiness remediation (Waves 49–56)

#### Fixed (13 critical bugs)
- **LEFT JOIN** silently executed as INNER JOIN (Wave 49)
- **Multi-aggregate GROUP BY** dropped all but the first aggregate (Wave 49)
- **SelectMulti + ORDER BY** returned rows in scan order (Wave 49)
- **DML WHERE** only supported `=`; now supports `!=`, `<>`, `<`, `>`, `<=`, `>=` (Wave 50)
- **DML WHERE** broke on strings with spaces; now uses the SQL lexer (Wave 50)
- **UPDATE** didn't update the NULL bitmap; COUNT(col) now excludes NULLed rows (Wave 50)
- **Checkpoint** was type-destructive; now preserves FLOAT/VARCHAR/NULL (Wave 50)
- **WAL** had no commit markers; BEGIN/COMMIT/ROLLBACK now write proper records (Wave 51)
- **WAL** appended before execute; now appends after successful execute (Wave 51)
- **WAL** string escaping was ambiguous; now uses base64 (Wave 51)
- **pgwire** sent NULL values as "0"; now sends -1 length (Wave 52)
- **pgwire** Describe executed the query; now returns NoData without executing (Wave 52)
- **pgwire** max_rows was discarded; now honours cursor-style Execute (Wave 52)

#### Added (dead modules wired into execute())
- **Views**: CREATE VIEW / DROP VIEW / SELECT FROM view (materialization) (Wave 53)
- **Procedures**: CREATE PROCEDURE / EXEC with positional params (Wave 53)
- **MERGE**: MERGE INTO ... WHEN MATCHED THEN UPDATE/DELETE/INSERT (Wave 53)
- **JSON**: json_value, json_query, json_modify, is_json (module-level) (Wave 53)
- **Temporal**: FOR SYSTEM_TIME AS OF <timestamp> (Wave 53)
- **Window functions**: ROW_NUMBER, RANK, DENSE_RANK, SUM, COUNT with OVER (...) (Wave 53)
- **PIVOT**: pivot() function callable (SQL parsing deferred) (Wave 53)

#### Documentation
- README.md: fixed repo layout, updated research agenda, added "Current SQL Surface" and "Known Limitations" sections
- ARCHITECTURE.md: replaced DAG executor description with dispatch-based flow, added CXL/RoCEv2 stub warnings
- ORCHESTRATION.md: added waves 19-56, updated test count from 554 to 1100+
- ROADMAP.md: updated feature table with actual implementation status
- CHANGELOG.md: added v0.3.0 through v1.0.0-remediated entries
- CONTRIBUTING.md: fixed test count, updated build instructions
- ADRs: added status notes to ADR-011 (ZNS WAL is not production WAL), ADR-018 (morsel executor not used), ADR-019 (DPccp not wired)
- Module doc comments: marked 10 dead modules with "NOT WIRED INTO SQL EXECUTION" notices

## [0.9.0] — 2026-07-29

### Added (Waves 41-48)
- MVCC readonly select path (`try_readonly_select`)
- ORDER BY on string columns via StringSearchColumn sidecar
- Parquet loader populates NULL bitmaps
- Type OID threaded through ResultColumn to pgwire
- Dispatch-path arithmetic in aggregates (SUM(price * 2))
- Typed expression evaluator (mixed int/float)

## [0.8.0] — 2026-07-28

### Added (Waves 36-40)
- TableSchema preserving column types from DDL
- Expression evaluator for arithmetic in aggregate args
- NULL bitmap in dispatch path (COUNT(col) excludes NULLs)
- String range predicates on StringSearchColumn
- Parallel count for large tables

## [0.7.0] — 2026-07-27

### Added (Waves 29-35)
- Kernel-direct query dispatch (classify_query → QueryShape → kernel)
- StringSearchColumn sidecar for string columns
- NULL bitmap support in Table and dispatch
- Flat hash table for GROUP BY
- Vectorized filter / sum / avg / min / max / count_distinct kernels

## [0.6.0] — 2026-07-26

### Added (Waves 23-28)
- DDL parser (CREATE TABLE, DROP TABLE, CREATE SCHEMA)
- DML parser (INSERT, UPDATE, DELETE)
- CTE parser (WITH ... AS (...) SELECT ...)
- pgwire protocol server (simple query + extended query)
- TPC-H interpreter fallback (CASE WHEN, HAVING, subqueries, multi-table joins)

## [0.5.0] — 2026-07-25

### Added (Waves 19-22)
- JOIN support in the basic parser (INNER, LEFT)
- GROUP BY with single-key and multi-key paths
- ORDER BY with ascending/descending
- LIMIT clause

## [0.4.0] — 2026-07-24

### Added (Waves 13-18)
- WCOJ / Leapfrog triejoin
- Learned cardinality estimator
- MCTS plan search for n>15 joins
- Adaptive eddies
- Tensor-network contraction for join planning
- 3× proof benchmark

## [0.3.0] — 2026-07-23

### Added (Waves 7-12)
- SQL parser (SELECT, FROM, WHERE, GROUP BY, ORDER BY, LIMIT)
- WAL + checkpoint for durability
- Protocol coordinator stubs (HLC, CXL, Raft)
- Indexes + sketches (BSI, LSH, HLL, Count-Min, t-Digest)
- DPccp join ordering
- TPC-H and TPC-C benchmark harness

## [0.2.0] — 2025-07-30

### Added
- Instruction-first, memory-centric architecture (25 ADRs)
- Kernel table with 16 AVX-512/AVX2/scalar kernels
- Tier-aware memory manager (8 tiers, NUMA detection, LRU migration)
- Storage format: 4 KB page / 2 MB region / 2 GB tablet
- ZNS-aware WAL with CRC32C checksums
- Data-centric morsel-driven executor (ADR-018)
- DPccp join ordering (ADR-019)
- Approximate SQL with (ε,δ) guarantees (ADR-015, ADR-024)
- Similarity search via VPOPCNTDQ + LSH (ADR-017)
- rANS compression for cold-tier columns (ADR-025)
- Calibrated analytic cost model (ADR-023, measured on Zen 5)
- Formal specification (SPECIFICATION.md, 755 lines)
- Problem catalog: 99 problems across 10 files
- 5-wave research corpus with per-problem solution evaluations
- CCL-X 1.2 license

### Measured performance (AMD EPYC-Turin / Zen 5)
- scan_eq AVX-512: 24.1 G cells/sec
- sum_f64 AVX-512: 29.8 G cells/sec
- hamming VPOPCNTDQ: 24.2 G cells/sec
- Memory bandwidth: 40.6 GB/s

## [0.1.0] — 2025-07-28

### Added
- Initial NaN-boxed Cell prototype (superseded by instruction-first architecture)
- Basic encoders: TF-IDF, char n-gram, color histogram, DCT, FFT, feature hashing, random projection
- Non-ML tensor storage with int8 quantization and sparse CSR
- LSM-style storage (WAL + SSTable)
- LSH and brute-force indexes
- axum HTTP server + clap CLI
