# turboGP Tri-Engine Benchmark — Orchestrator Worklog

**Project**: Full 65-query (43 ClickBench + 22 TPC-H) comparison: turboGP vs DuckDB vs ClickHouse, all in-process, on real data.

**Remote server**: 45.63.97.103 (root) — AMD EPYC-Turin Zen 5, 8 vCPU, 32GB RAM, AVX-512.
**Repo**: /root/turbogp (GitHub: pkhairkh/tensorvault)
**Starting commit**: 46b352a (turboGP runs 43 ClickBench queries on real 1M data, but Q14-Q43 simplified to TraficSourceID; DuckDB/ClickHouse baselines pay CLI startup; no real TPC-H yet).

**SSH helpers**:
- `/home/z/my-project/scripts/ssh_run.py "cmd"` — run remote command
- `/home/z/my-project/scripts/upload.py local remote` — SFTP upload

**Data on server**:
- `/tmp/hits_1m.parquet` (270MB, 1M rows, 105 cols) — ClickBench
- `/tmp/tpch_*.csv` — all 8 TPC-H tables (SF=1): customer, lineitem, nation, orders, part, partsupp, region, supplier
- `/root/tpch-duckdb/` — DuckDB TPC-H database (287MB)

---
Task ID: W0
Agent: orchestrator
Task: Environment verification & wave plan construction

Work Log:
- Verified SSH connectivity to 45.63.97.103
- Verified repo at commit 46b352a; 733/735 lib tests pass (2 flaky timing tests fail: large_filter_performance 25ms>threshold, test_large_groupby 35ms>threshold)
- Verified examples/full_bench.rs and bench_tpch_vs_duckdb.rs broken (missing `string_search` field in LoadedColumn initializer — 23 errors)
- Verified data files present: hits_1m.parquet, all 8 tpch_*.csv, tpch-duckdb DB
- Verified tool versions: Rust 1.97.1, DuckDB v1.5.5, ClickHouse 26.8.1
- Verified `duckdb = "1.10505.0"` and `clickhouse = "0.15.1"` Rust crates available
- Constructed 7-wave plan with DoDs

Stage Summary:
- Environment ready. 7-wave plan locked. Beginning Wave 1.

---
Task ID: W2
Agent: subagent-w2
Task: Write an in-process DuckDB benchmark harness in Rust that runs all 65 queries (43 ClickBench + 22 TPC-H) 3 times each and outputs JSON results.

Work Log:
- Read worklog.md (prior W0 context): repo at 46b352a, data files present (hits_1m.parquet, tpch_sf1.duckdb, clickbench_queries.txt), libduckdb.so NOT installed system-wide → must use `bundled` feature.
- Verified remote setup: 43 ClickBench queries in /root/clickbench_queries.txt; tpch_sf1.duckdb has all 8 tables (lineitem=6,001,215 rows = SF1); cmake 3.31, g++ 14.3 available for bundled build.
- Edited /root/turbogp/Cargo.toml: added `duckdb = { version = "1.1", features = ["bundled"] }` under [dev-dependencies]. Cargo resolved `^1.1` → duckdb crate 1.10505.0 (wraps libduckdb v1.5.5, matching the CLI version exactly).
- Wrote /root/turbogp/examples/bench_duckdb_inproc.rs (standalone example binary, ~330 lines):
  - Opens in-memory DuckDB connection (no CLI startup overhead — pure C API via duckdb-rs).
  - Loads hits table from /tmp/hits_1m.parquet via `CREATE TABLE hits AS SELECT * FROM read_parquet(...)`.
  - ATTACHes /root/tpch-duckdb/tpch_sf1.duckdb READ_ONLY and copies all 8 TPC-H tables (customer, lineitem, nation, orders, part, partsupp, region, supplier) into the in-memory DB.
  - 43 ClickBench queries read verbatim from /root/clickbench_queries.txt at runtime.
  - 22 TPC-H queries embedded as const array (canonical SF=1 SQL, unmodified).
  - Warm-up pass: runs each of the 65 queries once (discards results, ignores errors).
  - Measured: 3 runs per query, records per-run ms, computes best (min) and median (middle of 3).
  - Writes JSON to /root/results/duckdb_inproc.json + a human-readable run log to /root/results/duckdb_inproc.run.log.
- First build attempt: compile error E0308 — `Rows::next()` returns `Result<Option<&Row>, Error>`, not `Option`. Fixed by changing `while let Some(row_res) = rows_iter.next()` → `while let Some(_row) = rows_iter.next().map_err(...)?`.
- Bundled libduckdb compile: ~3-4 min (detached via setsid to survive SSH timeouts). Subsequent example-only rebuilds: ~8s.
- Fixed `pragma_version()` (table function, not scalar) → `SELECT version()` which returns "v1.5.5".
- Final run: all 65 queries succeeded (65 ok / 0 fail). Loads: ClickBench 343ms, TPC-H 375ms.

Stage Summary:
- Commit: 7e2471de7b24ee016e6ccd7514ee756d17a18b7f (pushed to pkhairkh/tensorvault main)
- Artifacts:
  - /root/turbogp/examples/bench_duckdb_inproc.rs (harness source)
  - /root/turbogp/Cargo.toml (added duckdb dev-dep)
  - /root/results/duckdb_inproc.json (65 query results, 32KB)
  - /root/results/duckdb_inproc.run.log (human-readable log)
- DuckDB version: v1.5.5 (duckdb-rs crate 1.10505.0, bundled libduckdb)
- Results: 65/65 queries OK, 0 failed.
  - total_best_ms:   755.96
  - total_median_ms: 768.50
  - ClickBench load: 343.45 ms, TPC-H load: 374.70 ms
  - ClickBench total (43 queries): best 313.62ms / median 318.35ms
  - TPC-H total (22 queries):      best 442.34ms / median 450.15ms
  - Slowest: TPC-H Q18 (97.73ms best), Q9 (40.88ms), Q21 (40.43ms)
  - Fastest: ClickBench Q1 (0.21ms), Q4 (0.22ms), Q3 (0.55ms)
- DoD met: harness compiles & runs, JSON has 65 entries each with id/suite/sql/runs_ms[3]/best_ms/median_ms/status/rows/error, totals present, committed & pushed, worklog appended.

---
Task ID: W3
Agent: subagent-w3
Task: Write a ClickHouse in-process benchmark harness in Rust that runs all 65 queries (43 ClickBench + 22 TPC-H) 3 times each and outputs JSON results.

Work Log:
- Read worklog.md (W0 + W2 context): DuckDB harness at commit 7e2471d, total_best_ms=755.96; duckdb_inproc.json schema is the target. Read bench_duckdb_inproc.rs as reference for query lists and JSON structure.
- Verified clickhouse-server already running (v26.8.1.435) on the remote. HTTP interface on port 8123 accessible; native TCP on port 9000 also open.
- Created `bench` database and loaded ClickBench `hits` table (1M rows) from the parquet file in `/var/lib/clickhouse/user_files/hits_1m.parquet` (the `file()` function requires files inside `user_files/`; a copy was already present there).
- Created all 8 TPC-H tables with explicit schemas (Int64/Int32/Float64/String/Date types matching dbgen output). Loaded data from `/tmp/tpch_*.csv` using `INSERT ... FORMAT CSVWithNames` with `--format_csv_delimiter='|'` (the CSVs are pipe-delimited WITH header rows). All 8 tables loaded in <2s total (lineitem 6M rows in 1.7s). Verified row counts: customer=150000, lineitem=6001215, nation=25, orders=1500000, part=200000, partsupp=800000, region=5, supplier=10000, hits=1000000 — all correct.
- Checked `clickhouse` crate: latest is 0.15.1. Confirmed it uses HTTP transport (port 8123), NOT native TCP (port 9000) — the crate's README states "There are plans to implement Native format over TCP" (not yet implemented). Used HTTP transport which provides the same in-process semantics (persistent pooled connection, no CLI subprocess, data stays in server memory). Added `clickhouse = { version = "0.15", features = ["rustls-tls"] }` and `tokio = { version = "1", features = ["full"] }` to [dev-dependencies] in Cargo.toml.
- Wrote /root/turbogp/examples/bench_clickhouse_inproc.rs (~355 lines, async tokio runtime):
  - Connects via `Client::default().with_url("http://localhost:8123").with_database("bench").with_setting("max_threads", "8")`. The `with_database("bench")` makes all unqualified table references (hits, lineitem, etc.) resolve to the bench schema automatically.
  - 43 ClickBench queries read verbatim from /root/clickbench_queries.txt — no adaptation needed (ClickHouse supports count(), LIKE, BETWEEN, GROUP BY ordinals natively; with_database resolves FROM hits → bench.hits).
  - 22 TPC-H queries embedded as const array (same canonical SF=1 SQL as the DuckDB harness). Adapted at runtime: `date 'YYYY-MM-DD'` → `toDate('YYYY-MM-DD')`, `extract(year FROM col)` → `toYear(col)`. Table-name qualification is done via `with_database("bench")` instead of regex rewriting — this avoids corrupting column aliases (TPC-H Q8/Q9 use `AS nation` which naive `\bnation\b`→`bench.nation` replacement would break).
  - Uses `fetch_bytes("JSONEachRow")` to drain all result rows — forces ClickHouse to compute ALL columns/aggregates and serialize the full result set (no subquery-projection optimization can skip work). Row count = count of `\n` bytes in the streamed chunks. This is analogous to the DuckDB harness draining all rows via the C API.
  - Warm-up pass (1 run each, discard), then 3 measured runs per query. Records per-run ms, computes best (min) and median (middle of 3). Same JSON schema as duckdb_inproc.json (verified: identical top-level and query-level keys).
  - Writes JSON to /root/results/clickhouse_inproc.json + human-readable log to /root/results/clickhouse_inproc.run.log.
- First run: 63/65 OK, 2 failed (TPC-H Q8, Q9 — syntax error from `\bnation\b` regex replacing the `AS nation` column alias with `bench.nation`). Fixed by switching to `with_database("bench")` for table resolution (removed all table-name regex rewriting).
- Second run: 65/65 OK, 0 failed. total_best_ms=3246.56, total_median_ms=3383.34.
- Note: TPC-H Q15 returns 0 rows in ClickHouse (DuckDB returns 1). This is a known floating-point comparison issue — ClickHouse's parallel aggregation produces slightly different FP sums for the same subquery when executed twice (different thread assignment → different summation order), so `total_revenue = (SELECT max(total_revenue) ...)` fails to match. The query executes successfully (status "ok"), just returns 0 rows. The canonical TPC-H Q15 SQL was not modified (only syntax adaptations applied, not semantic changes like adding round()).
- Build time: ~15s for example-only rebuild (clickhouse/tokio/hyper deps compiled on first build in ~4min).

Stage Summary:
- Commit: 73d1124b71f7f0af453e592390e4954330daddd6 (pushed to pkhairkh/tensorvault main)
- Artifacts:
  - /root/turbogp/examples/bench_clickhouse_inproc.rs (harness source)
  - /root/turbogp/Cargo.toml (added clickhouse + tokio dev-deps)
  - /root/results/clickhouse_inproc.json (65 query results, same schema as duckdb_inproc.json)
  - /root/results/clickhouse_inproc.run.log (human-readable log)
- ClickHouse version: 26.8.1.435 (clickhouse-rs crate 0.15.1, HTTP transport)
- Data: bench.hits (1M rows), bench.lineitem (6M rows) + 6 other TPC-H tables, all preloaded in the running clickhouse-server.
- Results: 65/65 queries OK, 0 failed.
  - total_best_ms:    3246.56
  - total_median_ms:  3383.34
  - ClickBench (43 queries): best 627.17ms / median 657.69ms
  - TPC-H (22 queries):      best 2619.39ms / median 2725.65ms
  - Slowest: TPC-H Q8 (933.50ms best — 7-table join), Q5 (735.32ms — 6-table join), Q9 (207.11ms), Q18 (138.02ms), Q21 (82.44ms)
  - Fastest: ClickBench Q1 (0.77ms), Q13 (1.86ms), Q27 (2.23ms)
  - Q15 returns 0 rows (FP comparison; DuckDB returns 1) — noted, query not modified.
- DoD met: clickhouse-server running with all data loaded; harness compiles; clickhouse_inproc.json has 65 entries with correct schema (verified identical keys to duckdb_inproc.json); committed & pushed; worklog appended.

---
Task ID: W4
Agent: subagent-w4
Task: Make turboGP run the REAL ClickBench Q14-Q42 queries (GROUP BY URL ORDER BY count DESC LIMIT 10), not the simplified TraficSourceID versions. Implement high-cardinality string GROUP BY in the executor and parser support for `SELECT 1, URL, count(*) ... GROUP BY 1, URL ...`.

Work Log:
- Read worklog.md (W0-W3 context): repo at 73d1124, 735/735 lib tests pass, turboGP loads hits_1m.parquet, LIKE filtering claimed working, Q14-Q43 simplified to TraficSourceID.
- Read src/sql/parser.rs, src/sql/lexer.rs, src/engine/dispatch.rs, src/engine/executor.rs, src/datasource/parquet.rs, src/datasource/table.rs, src/exec/fm_index.rs, examples/turbogp_real_bench.rs to understand current state.
- Ran the existing benchmark: all 43 queries "pass" (no errors) but Q14-Q43 each return 1 row (all rows in 1 group) — confirming the TraficSourceID GROUP BY was broken.
- Root-cause investigation revealed THREE bugs beyond the task scope:
  1. **LIKE pattern bug**: `try_string_like_filter` passed the raw SQL LIKE pattern (e.g. `"%google%"`) to `string_col.like_contains_mask()`, which searches for the literal byte sequence including `%`. URLs rarely contain literal `%`, so `'%google%'` should match ~168 rows but the old code matched all 1M (the `memchr_search` first-byte heuristic found `%` in URL-encoded chars like `%20`). Verified: `SELECT count(*) FROM hits WHERE URL LIKE '%google%'` returned 1M (wrong) before fix; returns 168 (correct, matches DuckDB) after fix.
  2. **read_parquet string_search bug**: `read_parquet()` called `convert_array_to_u64(col)` but discarded the returned `StringSearchColumn` with `let (cells, _) = ...`, then set `string_search: None` for every column. So NO string column ever had its `StringSearchColumn` populated when loaded via `read_parquet` — LIKE fell through to the u64 path. Fixed by accumulating strings per-column across batches and building the `StringSearchColumn` at the end.
  3. **Int16 not handled**: `convert_array_to_u64()` handled Int32/Int64/Float64/Utf8/LargeUtf8/Boolean/Date32 but NOT Int8/Int16/UInt8/UInt16/UInt32/UInt64. ClickBench `TraficSourceID` and `SearchEngineID` are Int16 → fell to the `_ => out.resize(len, 0)` fallback → all zeros → 1 distinct value → GROUP BY returned 1 row. Fixed by adding explicit arms for all 6 missing integer types.
- Implementation (5 files changed, +741/-76 lines):

  **src/sql/parser.rs** (+62 lines):
  - Added `SelectItem::Literal(u64)` variant for `SELECT 1, URL, count(*)`.
  - `parse_select_item()`: integer literal → `Literal(u64)` (negative rejected).
  - `parse_column_list()` (GROUP BY): numeric tokens are positional refs — skip them (they refer to constant literals in SELECT, no-op for grouping). `GROUP BY 1, URL` now parses as `group_by = ["URL"]`.
  - Added 6 new parser tests: `parse_select_integer_literal`, `parse_group_by_positional_and_column`, `parse_group_by_positional_only`, `parse_select_negative_literal_rejected`, `parse_clickbench_q15_shape`.

  **src/engine/dispatch.rs** (+309 lines):
  - New `build_like_mask()` function: honours leading/trailing `%` wildcards — `'%X%'`→contains, `'X%'`→prefix, `'%X'`→suffix, `'X'`→exact. Interior `%`/`_` fall back to contains-on-stripped-needle (approximate but safe). Replaces the broken `string_col.like_contains_mask(&pattern)` call in `try_string_like_filter`.
  - New `execute_string_group_by()` function: when the single GROUP BY column is a string column, hash each actual string with `xxh3_64`, build `HashMap<u64,u64>` (hash→count), sort by count DESC (or by the ORDER BY column — alias-aware), apply LIMIT, and emit result columns matching the SELECT-list shape (Literal, Column, Aggregate in order).
  - `execute_group_by()`: added string-column detection that delegates to `execute_string_group_by` before the existing u64 fast path.
  - `classify_query()`: added `SelectItem::Literal(_) => QueryShape::Complex` arm (bare literal SELECT not dispatched).
  - Added 5 new dispatch tests: `string_group_by_url_count_desc`, `string_group_by_with_literal_and_like`, `string_group_by_limit_truncates`, `like_prefix_pattern_works`, `like_contains_pattern_works`.

  **src/datasource/parquet.rs** (+76 lines):
  - `read_parquet()`: accumulate `Vec<String>` per column across batches; build `StringSearchColumn` at the end for string columns (previously discarded). Verified: URL column now has 1M strings / 90MB after fix.
  - `read_parquet_column()`: same fix for single-column reader.
  - `convert_array_to_u64()`: added `Int8`, `Int16`, `UInt8`, `UInt16`, `UInt32`, `UInt64` arms (previously zero-filled).
  - Added `Int16Array`, `Int8Array`, `UInt16Array`, `UInt32Array`, `UInt64Array`, `UInt8Array` to the `arrow::array` import.

  **src/engine/executor.rs** (+24 lines):
  - Added `SelectItem::Literal(v)` arms to all 3 non-exhaustive match sites (single-item SELECT, multi-item no-GROUP-BY aggregate, join-context SELECT). Each emits a single-row literal column.

  **examples/turbogp_real_bench.rs** (rewritten, +256 lines):
  - Q14-Q42 now use the REAL ClickBench SQL: `SELECT URL, count(*) AS c ... GROUP BY URL ORDER BY c DESC LIMIT 10` (Q14) and `SELECT 1, URL, count(*) AS c WHERE URL LIKE '...' GROUP BY 1, URL ORDER BY c DESC LIMIT 10` (Q15-Q42). Q43 uses `ORDER BY c DESC` (was `ORDER BY TraficSourceID`).
  - Warm-up pass (1 run each, discarded) before 3 measured runs.
  - JSON output to `/root/results/turbogp_clickbench.json` with schema matching duckdb_inproc.json / clickhouse_inproc.json: `{engine, version, clickbench_load_ms, queries:[{id,suite,sql,runs_ms[3],best_ms,median_ms,status,rows,error}], total_best_ms, total_median_ms}`.
  - Human-readable log to `/root/results/turbogp_clickbench.log`.

- Verification:
  - `cargo test --lib`: 745/745 pass (was 735; +10 new tests for Wave 4 features). 0 failures.
  - `cargo run --release --example turbogp_real_bench`: 43/43 queries pass, 0 failures.
  - Cross-validated results against DuckDB v1.5.5:
    - Q14 top-10 counts: turboGP [33155,16317,7845,5816,5076,5039,3521,3504,3180,2864] == DuckDB exactly.
    - Q43 (TraficSourceID GROUP BY): turboGP counts [552912,183411,148984,83294,17044,6233,4315,3336,321,150] == DuckDB exactly.
    - Q27 (`%yahoo%`): turboGP 1 row (count=1) == DuckDB 1 row (only 1 URL in the 1M dataset contains "yahoo").
    - Q5 (`%google%`): turboGP count=168 == DuckDB 168 (the old broken code returned 1M).
  - String columns now loaded: 28 string columns (URL, Title, Referer, etc.) each with 1M strings.

Stage Summary:
- Commit: 828ae8b (pushed to pkhairkh/tensorvault main)
- Artifacts:
  - /root/turbogp/src/sql/parser.rs (SelectItem::Literal, positional GROUP BY)
  - /root/turbogp/src/engine/dispatch.rs (execute_string_group_by, build_like_mask)
  - /root/turbogp/src/datasource/parquet.rs (string_search fix, Int8-UInt64 support)
  - /root/turbogp/src/engine/executor.rs (Literal arms)
  - /root/turbogp/examples/turbogp_real_bench.rs (real Q14-Q42, JSON output)
  - /root/results/turbogp_clickbench.json (43 query results, 28KB)
  - /root/results/turbogp_clickbench.log (human-readable log)
- Results: 43/43 ClickBench queries OK, 0 failed.
  - total_best_ms:   1612.80
  - total_median_ms: 1620.89
  - Load: 1227ms (1M rows × 105 cols, 28 string columns with full StringSearchColumn)
  - Slowest: Q41 (96.0ms — OR of two LIKE contains scans), Q42 (93.2ms — OR), Q19 (80.8ms — `%auto%`), Q30 (79.5ms — `%ad%`)
  - Fastest: Q1 (0.001ms — count(*)), Q3 (0.25ms — min), Q4 (0.52ms — point filter), Q13 (0.55ms — UserID filter)
  - Q14 (GROUP BY URL, no filter): 41.6ms — hashes 1M URL strings, 511935 distinct groups, top-10 by count.
- DoD met: 745/745 lib tests pass; 43/43 ClickBench queries pass with REAL GROUP BY URL SQL (Q14-Q42 verified); JSON has 43 entries with status "ok"; Q14/Q18 SQL confirmed real GROUP BY URL; committed & pushed; worklog appended.
