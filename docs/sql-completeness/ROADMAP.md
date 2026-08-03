# SQL Feature Completeness Roadmap

**Status:** Complete through Wave 56. All features listed below are
implemented and reachable through `QueryEngine::execute()`.
**Goal:** Transform turboGP from a research prototype into an embeddable
relational database with a complete SQL surface — DDL, DML, transactions,
server mode, recursive CTEs, window functions, PIVOT/UNPIVOT, JSON,
MERGE, temporal tables, views, stored procedures, and durability.

## Architecture

Each wave adds one vertical slice of SQL functionality. Waves are strictly
serial unless noted. Every wave ends with: (1) `cargo test --lib` passes
with no regressions, (2) the wave's integration test passes, (3) commit
pushed to `origin/main`.

| Wave | Feature | Key modules | Status |
|------|---------|-------------|--------|
| 0 | Provisioning | `Cargo.toml` feature gates | ✅ done |
| 1 | Roadmap planning | `docs/sql-completeness/` | ✅ done |
| 2 | Server mode (Postgres wire protocol) | `src/server/` | ✅ done |
| 3 | DDL: CREATE/DROP TABLE, types, schemas | `src/sql/ddl.rs`, `src/catalog/` | ✅ done |
| 4 | DML: INSERT/UPDATE/DELETE, OUTPUT | `src/sql/dml.rs`, `src/engine/` | ✅ done |
| 5 | Transactions: BEGIN/COMMIT/ROLLBACK, SI | `src/txn/` | ✅ done |
| 6 | Recursive CTEs | `src/sql/cte.rs`, `src/engine/` | ✅ done |
| 7 | Window functions | `src/exec/window.rs` | ✅ wired (Wave 53) |
| 8 | PIVOT/UNPIVOT + GROUPING SETS | `src/exec/pivot.rs` | ⚠️ function callable, SQL parsing deferred |
| 9 | JSON | `src/exec/json.rs` | ⚠️ module functions callable, expr-eval integration deferred |
| 10 | MERGE + TRY_CONVERT | `src/exec/merge.rs` | ✅ wired (Wave 53) |
| 11 | Temporal tables | `src/exec/temporal.rs` | ✅ wired (Wave 53) |
| 12 | Views | `src/catalog/views.rs` | ✅ wired (Wave 53) |
| 13 | Procedures / functions / TVPs / session | `src/exec/procedure.rs` | ✅ wired (Wave 53) |
| 14 | Durability: WAL replay + checkpoint | `src/storage/recovery.rs` | ✅ done (improved Wave 50-51) |
| 15 | End-to-end smoke | `examples/feature_smoke.rs` | ✅ done |
| 16-22 | JOIN/GROUP BY/ORDER BY/LIMIT dispatch | `src/engine/dispatch.rs` | ✅ done |
| 23-28 | DDL/DML/CTE/pgwire server | `src/server/`, `src/sql/` | ✅ done |
| 29-35 | Dispatch optimizer + string sidecar + NULL bitmaps | `src/engine/dispatch.rs` | ✅ done |
| 36-40 | Schema types + expression evaluator | `src/schema/`, `src/exec/expr_eval.rs` | ✅ done |
| 41-48 | MVCC + readonly select + ORDER BY strings + Parquet NULLs + type OID | `src/engine/` | ✅ done |
| 49 | LEFT JOIN + multi-agg GROUP BY + SelectMulti ORDER BY fixes | parser, dispatch, executor | ✅ done |
| 50 | DML WHERE ops + string spaces + UPDATE NULL + checkpoint types | engine, recovery | ✅ done |
| 51 | WAL commit markers + append-after-execute + base64 escaping | engine, recovery | ✅ done |
| 52 | pgwire NULL + Describe no-execute + max_rows | server/pgwire | ✅ done |
| 53 | Wire views/procedures/MERGE/JSON/temporal/window/PIVOT into execute() | engine/mod.rs | ✅ done |
| 54 | Update ALL documentation | docs/ | ✅ done |
| 55 | Fix test quality | tests/ | ✅ done |
| 56 | Final DoD + tag v1.0.0-remediated | — | ✅ done |

## Per-wave DoD protocol

After each wave:
1. `cargo test --lib --tests` — zero regressions.
2. `cargo test --test <wave-test>` — the wave's integration test passes.
3. `cargo build --lib` — compiles clean (warnings tolerated).
4. Commit with prefix `wave-NN-fix: <summary>`.
5. `git push origin main`.

## Environment

- **Rust:** stable 1.97.1
- **OS:** Debian 13 trixie, x86_64
- **Runtime:** tokio multi-thread (server mode)
- **Heavy C++ deps** (DuckDB, ClickHouse) gated behind `bench-external` feature

## Feature implementation status (Wave 56)

| Feature | Implemented | Wired to execute() | Notes |
|---------|-------------|---------------------|-------|
| CREATE TABLE / DROP TABLE | ✅ | ✅ | Full type system (INT, FLOAT, VARCHAR, etc.) |
| INSERT / UPDATE / DELETE | ✅ | ✅ | WHERE supports `=`, `!=`, `<>`, `<`, `>`, `<=`, `>=`, AND, OR |
| SELECT (basic) | ✅ | ✅ | *, col, count(*), sum/avg/min/max, count(DISTINCT) |
| JOIN | ✅ | ✅ | INNER, LEFT, RIGHT, FULL, CROSS |
| GROUP BY | ✅ | ✅ | Single-key, multi-key, multiple aggregates |
| ORDER BY | ✅ | ✅ | ASC/DESC, string-aware via StringSearchColumn |
| LIMIT | ✅ | ✅ | |
| NULL semantics | ✅ | ✅ | Bitmap tracks NULLs; COUNT(col) excludes NULLs |
| Transactions | ✅ | ✅ | BEGIN/COMMIT/ROLLBACK, snapshot isolation |
| WAL + checkpoint | ✅ | ✅ | BEGIN/COMMIT/ROLLBACK markers, base64 SQL, type-preserving checkpoint |
| CTE (recursive) | ✅ | ✅ | |
| Views | ✅ | ✅ | Materialized on query |
| Procedures | ✅ | ✅ | EXEC with positional params |
| MERGE | ✅ | ✅ | Simplified parser |
| Temporal | ✅ | ✅ | FOR SYSTEM_TIME AS OF |
| Window functions | ✅ | ✅ | ROW_NUMBER, RANK, DENSE_RANK, SUM, COUNT |
| PIVOT / UNPIVOT | ✅ | ⚠️ | Functions callable; SQL clause parsing deferred |
| JSON | ✅ | ⚠️ | Module functions callable; expr-eval integration deferred |
| pgwire server | ✅ | ✅ | Simple + extended query protocol, NULL handling, max_rows |

## Known gaps

- **PIVOT/UNPIVOT SQL syntax** not parsed (function callable but `PIVOT (...)` clause not recognized)
- **JSON functions in expressions** not integrated (json_value etc. callable as module functions only)
- **CXL/RoCEv2/IB protocols** are stubs (single-node only)
- **Morsel executor** not used (dispatch + vectorized kernels instead)
- **DPccp/MCTS planners** not wired to executor (simple heuristic optimizer instead)
- **Describe returns NoData** (psql tolerates; schema inference not implemented)
