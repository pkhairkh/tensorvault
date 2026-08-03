# SQL Feature Completeness Roadmap

**Status:** Active. Drives Waves 1–15.
**Goal:** Transform turboGP from a research prototype into an embeddable
relational database with a complete SQL surface — DDL, DML, transactions,
server mode, recursive CTEs, window functions, PIVOT/UNPIVOT, JSON,
MERGE, temporal tables, views, stored procedures, and durability.

## Architecture

Each wave adds one vertical slice of SQL functionality. Waves are strictly
serial unless noted. Every wave ends with: (1) `cargo test --lib` passes
with no regressions, (2) the wave's integration test passes, (3) commit
pushed to `origin/main`.

| Wave | Feature | Key modules | Integration test |
|------|---------|-------------|------------------|
| 0 | Provisioning (done) | `Cargo.toml` feature gates | — |
| 1 | This plan (done) | `docs/sql-completeness/` | — |
| 2 | Server mode (Postgres wire protocol) | `src/server/` | `tests/server_pgwire.rs` |
| 3 | DDL: CREATE/DROP TABLE, types, schemas | `src/sql/ddl.rs`, `src/catalog/ddl.rs` | `tests/ddl.rs` |
| 4 | DML: INSERT/UPDATE/DELETE, OUTPUT | `src/sql/dml.rs`, `src/engine/dml.rs` | `tests/dml.rs` |
| 5 | Transactions: BEGIN/COMMIT/ROLLBACK, SI | `src/txn/` | `tests/txn.rs` |
| 6 | Recursive CTEs | `src/sql/cte.rs`, `src/exec/recursive.rs` | `tests/cte_recursive.rs` |
| 7 | Window functions | `src/exec/window.rs` | `tests/window.rs` |
| 8 | PIVOT/UNPIVOT + GROUPING SETS | `src/exec/pivot.rs` | `tests/pivot_grouping.rs` |
| 9 | JSON | `src/types/json.rs`, `src/exec/json.rs` | `tests/json.rs` |
| 10 | MERGE + TRY_CONVERT | `src/exec/merge.rs` | `tests/merge.rs` |
| 11 | Temporal tables | `src/exec/temporal.rs` | `tests/temporal.rs` |
| 12 | Views | `src/catalog/views.rs` | `tests/views.rs` |
| 13 | Procedures / functions / TVPs / session | `src/exec/procedure.rs` | `tests/procedures.rs` |
| 14 | Durability: WAL replay + checkpoint | `src/storage/recovery.rs` | `tests/recovery.rs` |
| 15 | End-to-end smoke | `examples/feature_smoke.rs` | `tests/feature_smoke.rs` |

## Per-wave DoD protocol

After each wave:
1. `cargo test --lib` — zero regressions.
2. `cargo test --test <wave-test>` — the wave's integration test passes.
3. `cargo build --lib` — no new warnings beyond the existing 295 baseline.
4. Commit with prefix `wave-N: <summary>`.
5. `git push origin main`.

## Environment

- **Rust:** stable 1.97.1
- **OS:** Debian 13 trixie, x86_64
- **Runtime:** tokio multi-thread (server mode)
- **Heavy C++ deps** (DuckDB, ClickHouse) gated behind `bench-external` feature
