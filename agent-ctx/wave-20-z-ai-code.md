# Wave 20 — End-to-End SQL Execution Engine

**Agent**: z-ai-code
**Date**: 2026-07-30
**Status**: Complete
**Baseline**: 583 tests (564 lib + 7 integration + 12 doc-tests, 1 ignored)
**After Wave 20**: 646 tests (627 lib + 7 integration + 12 doc-tests, 2 ignored)
**Net new**: +63 lib tests (22 engine/mod + 10 engine/result + 27 engine/executor + 4 sql/parser)
**New ignored**: +1 doc-test (the engine module-level example uses `ignore` because it requires a `.parquet` file).

## Goal

Before Wave 20 the SQL parser produced a `SelectQuery` but nothing
connected it to the catalog → planner → executor → result. Wave 20
closes that gap with a top-level `QueryEngine` that takes a SQL string
and returns results.

## Tasks Completed

### 20-1: `src/engine/mod.rs` — `QueryEngine`

The top-level engine struct:

```rust
pub struct QueryEngine {
    catalog: Catalog,
    kernel_table: Arc<KernelTable>,
    cost_model: CostModel,
}

impl QueryEngine {
    pub fn new() -> Self
    pub fn with_cost_model(cost_model: CostModel) -> Self
    pub fn catalog(&self) -> &Catalog
    pub fn kernel_table(&self) -> &KernelTable
    pub fn cost_model(&self) -> &CostModel
    pub fn register_table(&mut self, table: Table)
    pub fn load_parquet(&mut self, path: &str, table_name: &str) -> Result<usize>
    pub fn load_csv(&mut self, path: &str, table_name: &str, has_header: bool) -> Result<usize>
    pub fn execute(&self, sql: &str) -> Result<QueryResult>
}
```

The `execute` pipeline is the heart of Wave 20:

1. `Instant::now()` to start the wall-clock timer.
2. `crate::sql::parse_with_extensions(sql)` — tokenises, strips the
   seven turboGP extensions, parses the remainder as `SELECT`.
   Errors become `Error::Parse(String)`.
3. `execute_select(&query, &extensions, &self.catalog, &self.kernel_table, &self.cost_model)`
   — the core execution path (see 20-3).
4. Stamp `result.elapsed_us = start.elapsed().as_micros() as u64`.

`load_parquet` and `load_csv` wrap the Wave 19 readers
([`crate::datasource::read_parquet`] / [`crate::datasource::read_csv`])
and register the resulting `Table` in the catalog under the user-
supplied name. The original `LoadedTable::name` (derived from the
file stem) is overwritten with the caller's `table_name` so
`SELECT * FROM <name>` works regardless of the file path.

`register_table` is a thin pass-through to `Catalog::register`. The
catalog overwrites same-name entries, which matches the "load file →
register → query" usage pattern.

`with_cost_model` is a bonus constructor for callers that want to
attach a learned cardinality estimator
([`CostModel::with_learned`]) without losing the default kernel
table.

22 tests live alongside the engine (`src/engine/mod.rs::tests`),
including the 9 DoD tests (`dod_*`) that mirror the task brief
exactly.

### 20-2: `src/engine/result.rs` — `QueryResult`

```rust
pub struct ResultColumn {
    pub name: String,
    pub values: Vec<u64>,
}

pub struct QueryResult {
    pub columns: Vec<ResultColumn>,
    pub row_count: usize,
    pub elapsed_us: u64,
}

impl QueryResult {
    pub fn empty() -> Self
    pub fn from_scalar_u64(name, value: u64) -> Self
    pub fn from_scalar_f64(name, value: f64) -> Self
    pub fn push_column(&mut self, column: ResultColumn) -> Result<(), String>
    pub fn column_count(&self) -> usize
    pub fn column(&self, name: &str) -> Option<&[u64]>
    pub fn scalar_u64(&self) -> Option<u64>
    pub fn scalar_f64(&self) -> Option<f64>
    pub fn print(&self)
}
```

Naming note: the constructors are `from_scalar_u64` / `from_scalar_f64`
(not `scalar_u64` / `scalar_f64`) because the spec also defines
getters with those names — duplicate method names would not compile.
The constructors build a single-cell result; the getters read the
first cell of the first column.

`print` renders a column-aligned text table with a `│` separator,
a `─` underline, and a `(N rows in M µs)` footer. Cell values are
printed as `u64` (the engine's universal cell format); for `f64`
aggregates the caller should bit-reinterpret via
`scalar_f64()` before printing.

`push_column` enforces the row-count invariant: the first push sets
`row_count`; subsequent pushes must match. Length-mismatch returns
`Err(String)` so callers don't silently construct a malformed result.

10 tests cover the result type (`scalar_u64_round_trip`,
`scalar_f64_round_trip`, `empty_result_prints_summary`,
`push_column_sets_row_count`, `push_column_rejects_length_mismatch`,
`column_lookup_by_name`, `scalar_returns_none_for_empty_result`,
`print_does_not_panic_on_multi_column`,
`result_column_scalar_constructors`, `default_is_empty`).

### 20-3: `src/engine/executor.rs` — `execute_select`

The core execution path:

```rust
pub fn execute_select(
    query: &SelectQuery,
    extensions: &QueryExtensions,
    catalog: &Catalog,
    kernel_table: &KernelTable,
    cost_model: &CostModel,
) -> Result<QueryResult>
```

The dispatch logic:

| SQL form | Path |
|----------|------|
| `SELECT count(*) FROM t` | `table.row_count` (no kernel — the catalog already knows) |
| `SELECT count(*) FROM t WHERE col = N` | `ScanEqU64` kernel → `result.count` |
| `SELECT count(DISTINCT col) FROM t` | `AggregateCountDistinct` kernel → `result.count` |
| `SELECT sum(col) FROM t` | in-engine integer-as-f64 sum (see note below) |
| `SELECT avg(col) FROM t` | in-engine sum / count |
| `SELECT * FROM t [WHERE col = N] [LIMIT k]` | in-engine filter + gather (no kernel) |
| `SELECT col FROM t [WHERE col = N]` | in-engine projection |

The WHERE extractor (`parse_where`) handles `col = <literal>` and
`<literal> = col` in either order. Literals can be `Int`, `Float`,
`String`, or `Hex` — the `literal_to_u64` helper converts each to the
`u64` the kernel expects (Int → `as u64`, Float → `to_bits`, String →
`xxh3_64` hash matching the loader's string encoding, Hex → 8-byte
little-endian pack). Anything other than a single equality returns
`Error::Other` rather than silently producing wrong results.

The single unsafe boundary is `run_kernel`, which constructs the
`*const u8` / `*mut u8` pointers the `Kernel::execute` trait method
requires. The wrapper is safe: it always passes a properly-sized
slice (`cell_count: input.len()`) and a stack-allocated `[u8; 64]`
output buffer (more than enough for any `KernelResult`, which is 24
bytes).

The tier picker (`pick_tier`) maps the `TIER` extension string to a
`MemoryTier`. Recognised names: `L1`, `L2`, `L1L2`, `L3`, `DDR5`,
`DRAM`, `DDR`, `HBM`, `CXL`, `NVME`, `NVMEOF`, `NETWORK`.
Unrecognised names fall back to `L3` (no error — the engine still
has a kernel for L3 via the kernel table's scalar fallback).

27 tests cover the executor, including all the DoD patterns and the
error paths (`non_existent_table_returns_not_found`,
`unknown_aggregate_returns_error`, `range_where_returns_error`,
`and_where_returns_error`, `invalid_sql_returns_parse_error`).

### 20-4: Integration with SQL parser

The `execute` method in `QueryEngine` calls
`crate::sql::parse_with_extensions(sql)` — the convenience entry
point added in Wave 18 that does tokenize → strip extensions → parse
SELECT in one call. Parse errors are wrapped in `Error::Parse(String)`.

### 20-5: Parser change — `COUNT(DISTINCT col)`

The existing parser handled `COUNT_DISTINCT(col)` (a single function
name) but not the standard SQL `COUNT(DISTINCT col)` syntax that the
Wave 20 DoD requires.

Modified `parse_select_item` in `src/sql/parser.rs`: after consuming
the `(` of an aggregate call, check whether the next token is
`Ident("DISTINCT")` (case-insensitive, via the existing
`match_ident` helper). If so, consume `DISTINCT`, expect a column
name, and produce `Aggregate { func: format!("{}_DISTINCT", name.to_uppercase()), arg: col }`.
Otherwise, fall through to the existing `parse_agg_arg` path.

This preserves the existing `COUNT_DISTINCT(col)` convention (the
test `build_plan_count_distinct_uses_count_distinct_operator` still
passes) while adding the standard SQL syntax.

4 new parser tests:
`parse_count_distinct_keyword`, `parse_count_distinct_case_insensitive`,
`parse_count_distinct_requires_column`, `parse_sum_distinct_keyword`.

## Design decisions worth recording

### 1. Why `SUM` does not use the `AggregateSumF64` kernel

The `AggregateSumF64` kernel bit-reinterprets each `u64` cell as
`f64` and sums them. This is correct for Float64-encoded columns
(where cells are `f64::to_bits(value)`), but WRONG for integer-
encoded columns (where cells are `value as u64`).

The engine does not yet track per-column types (the loaders encode
every column as `Vec<u64>`; the `Table` struct has no type
metadata). Without type info, calling the kernel on an integer
column produces nonsense — e.g. `sum(id)` over `0,1,...,999` returns
~0.0 instead of `499500.0`, because each integer's bit pattern
reinterprets as a tiny denormal float.

Two options:

1. **Track column types** (invasive change to Wave 19's `Table`
   struct — would touch the loaders, the catalog, and every caller).
2. **Sum as integers** (cast each `u64` to `f64` and accumulate;
   correct for integer-encoded columns up to `f64`'s 53-bit
   mantissa, ≈9 × 10¹⁵).

Wave 20 chose option 2 because the DoD test (`SELECT sum(id) FROM t`
returning `499500.0`) explicitly exercises an integer-encoded
column, and option 1 is a separate concern that should be its own
wave. The `AggregateSumF64` kernel is still used by direct kernel
tests and is the right choice for Float64 columns once type metadata
exists.

The decision is documented in `execute_sum`'s doc comment, along
with the note that a future wave should route Float64 columns to the
kernel.

### 2. Why `SELECT *` filters in-engine, not via the scan kernel

The `ScanEqU64` kernel returns a `count` and a 64-bit `mask` covering
the first 64 cells. That's enough to answer `SELECT count(*) FROM t
WHERE col = N` (use `count`), but not enough to gather matching rows
for `SELECT * FROM t WHERE col = N` on tables larger than 64 rows.

A production engine would extend the kernel interface to return a
vector of matching indices (or materialise the matching rows
directly). For Wave 20 the simpler path is to filter in-engine:
iterate the column, collect matching row indices, then gather every
column at those indices. This is `O(n)` per column, which is fine
for the test sizes (≤1000 rows); a future wave would replace it
with a real scan kernel that returns positions.

### 3. Why `WHERE` only supports a single equality

The `parse_where` extractor handles `col = <literal>` (in either
order) and rejects everything else: range predicates (`col < N`),
multi-predicate WHERE (`a = 1 AND b = 2`), and non-equality
comparisons (`col != N`).

The engine *could* route range predicates to `ScanRangeU64` and
multi-predicate WHERE to `ScanMultiPredicate` — both kernels exist.
But that requires more planner work than Wave 20 is doing (the
existing `src/sql/plan.rs::extract_eq_target` only handles single
equalities too). Rather than silently producing wrong answers, the
executor returns `Error::Other` with a descriptive message so the
caller knows the query shape is not yet supported.

### 4. Why `TIER L3` is mostly a no-op

The TIER extension selects the kernel tier via `pick_tier`, but for
`SELECT count(*) FROM t TIER L3` (no WHERE), the executor returns
`table.row_count` directly without running any kernel — so the tier
selection has no observable effect. For `SELECT count(*) FROM t
WHERE x = 0 TIER L3`, the L3-tier `ScanEqU64` kernel is selected
(the default anyway).

For `TIER CXL`, the kernel table's `select` falls back to the
scalar-L3 kernel (no AVX-512 CXL kernel is registered on most
platforms). The query still produces the right answer; only the
throughput differs. The `tier_cxl_selects_cxl_tier` test verifies
this fallback path.

### 5. Why `APPROXIMATE` is accepted but ignored

The `APPROXIMATE WITHIN ε CONFIDENCE δ` extension is parsed and
stored in `QueryExtensions::approximate` as `(epsilon,
failure_probability)`. The Wave 20 executor does not use these
values — it runs the exact `AggregateCountDistinct` kernel (HashSet-
backed prototype). A future wave would route to a HyperLogLog-based
approximate count distinct that consumes `(epsilon,
failure_probability)` to size the HLL registers.

The DoD test `dod_count_distinct_with_approximate` verifies the
query parses and runs without error; the result is the exact count
(7), which is also the correct approximate answer.

### 6. Why `COUNT(DISTINCT col)` required a parser change

The existing parser's `parse_agg_arg` consumes a single `Ident` and
returns it as the argument string. For `COUNT(DISTINCT x)`, the
first token is `Ident("DISTINCT")` (DISTINCT is not in `KEYWORDS`),
so `parse_agg_arg` would consume it and return `"DISTINCT"` — then
the parser would see `Ident("x")` where it expected `)` and error.

The fix detects `DISTINCT` *before* calling `parse_agg_arg` and
folds it into the function name: `COUNT(DISTINCT x)` becomes
`Aggregate { func: "COUNT_DISTINCT", arg: "x" }`. This preserves the
existing `COUNT_DISTINCT(x)` convention (which `pick_aggregate_operator`
in `src/sql/plan.rs` already recognises) while adding the standard
SQL syntax.

## DoD Verification

| DoD | Status |
|-----|--------|
| `cargo test` passes (583 existing + new) | ✅ 627 lib + 7 integration + 12 doc-tests, 2 ignored (646 total; +63 new lib tests) |
| `cargo clippy -- -D warnings` passes | ✅ `cargo clippy --all-targets -- -D warnings` clean |
| `cargo fmt` clean | ✅ `cargo fmt --check` returns no diffs (only the unstable-feature warnings on stable rustc) |
| Can execute `SELECT count(*) FROM table WHERE col = value` end-to-end | ✅ `dod_count_star_with_where` (1000-row table, 7 matches for x = 42) |
| Can execute `SELECT sum(col) FROM table` end-to-end | ✅ `dod_sum_returns_correct_sum` (sum(id) over 0..1000 = 499500.0) |
| Can load a Parquet file and query it | ✅ `dod_load_parquet_and_query` + `parquet_int_column_count_and_sum` |
| Can load a CSV file and query it | ✅ `load_csv_and_query` (5-row CSV with `id,value` header, count/sum/filter) |
| Invalid SQL returns typed errors | ✅ `dod_invalid_sql_returns_parse_error` → `Error::Parse`; `dod_non_existent_table_returns_not_found` → `Error::NotFound` |
| APPROXIMATE extension runs end-to-end | ✅ `dod_count_distinct_with_approximate` (parses, runs, returns 7) |
| TIER extension runs end-to-end | ✅ `dod_count_star_with_tier_l3` (parses, runs, returns 1000) |
| `SELECT * FROM t WHERE id = 5` returns matching rows | ✅ `dod_select_star_with_where` (1 row, id=5, x=5) |

## Files Created / Modified

| File | Action | Lines | Purpose |
|------|--------|------:|---------|
| `src/engine/mod.rs` | created | 567 | `QueryEngine` struct + 22 tests (incl. 9 DoD tests) |
| `src/engine/result.rs` | created | 319 | `ResultColumn`, `QueryResult` + 10 tests |
| `src/engine/executor.rs` | created | 815 | `execute_select` + 27 tests |
| `src/sql/parser.rs` | modified | +47 | `COUNT(DISTINCT col)` support in `parse_select_item` + 4 tests |
| `src/lib.rs` | modified | +4 | registered `pub mod engine;`; added module-doc bullet |

**Net new**: ~1700 lines across 3 new files, +63 tests
(22 engine/mod + 10 engine/result + 27 engine/executor + 4 sql/parser).

## Test inventory

### `src/engine/mod.rs` (22 tests)

DoD tests (9):
- `dod_count_star_returns_row_count` — DoD 1
- `dod_count_star_with_where` — DoD 2
- `dod_sum_returns_correct_sum` — DoD 3
- `dod_select_star_with_where` — DoD 4
- `dod_count_distinct_with_approximate` — DoD 5
- `dod_count_star_with_tier_l3` — DoD 6
- `dod_invalid_sql_returns_parse_error` — DoD 7
- `dod_non_existent_table_returns_not_found` — DoD 8
- `dod_load_parquet_and_query` — DoD 9

Additional integration tests (13):
- `load_csv_and_query` — CSV load + count + filter + sum + select
- `engine_sum_integer_column` — integer sum through the engine API
- `execute_populates_elapsed_us` — wall-clock timing
- `register_table_overwrites` — same-name registration replaces
- `with_cost_model_constructs_engine` — custom cost model
- `default_is_empty` — `QueryEngine::default()` is empty
- `accessors_work` — `catalog()`, `kernel_table()`, `cost_model()`
- `count_star_on_empty_table_returns_zero` — empty-table edge case
- `sum_on_empty_table_returns_zero` — empty-table edge case
- `print_does_not_panic` — `print()` on a real result
- `other_extensions_accepted` — `USING`, `MEMORY BUDGET`, etc. as no-ops
- `load_parquet_under_custom_name` — file stem ≠ registered name
- `parquet_int_column_count_and_sum` — Int64 round-trip + count + sum + distinct + filter

### `src/engine/result.rs` (10 tests)

- `scalar_u64_round_trip`
- `scalar_f64_round_trip`
- `empty_result_prints_summary`
- `push_column_sets_row_count`
- `push_column_rejects_length_mismatch`
- `column_lookup_by_name`
- `scalar_returns_none_for_empty_result`
- `print_does_not_panic_on_multi_column`
- `result_column_scalar_constructors`
- `default_is_empty`

### `src/engine/executor.rs` (27 tests)

- `count_star_no_where_returns_row_count`
- `count_star_with_where_returns_match_count`
- `count_star_with_where_no_matches_returns_zero`
- `sum_col_returns_correct_sum`
- `sum_float_col_returns_correct_sum_for_integer_encoding`
- `count_distinct_returns_distinct_count`
- `count_distinct_with_approximate_extension_runs`
- `count_star_with_tier_extension_runs`
- `select_star_with_where_returns_matching_rows`
- `select_star_no_where_returns_all_rows`
- `select_star_with_limit_truncates`
- `select_column_returns_single_column`
- `select_column_with_where_on_same_column_filters`
- `select_column_with_where_on_other_column_gathers`
- `avg_col_returns_correct_average`
- `non_existent_table_returns_not_found`
- `non_existent_column_returns_not_found`
- `unknown_aggregate_returns_error`
- `min_max_return_unsupported_error`
- `multi_item_select_returns_error`
- `range_where_returns_error`
- `and_where_returns_error`
- `invalid_sql_returns_parse_error`
- `count_with_column_arg_returns_error`
- `tier_cxl_selects_cxl_tier`
- `literal_on_left_of_equality_works`
- `sum_with_where_filters_then_sums`

### `src/sql/parser.rs` (4 new tests, 21 total)

- `parse_count_distinct_keyword` — `COUNT(DISTINCT user_id)` → `func="COUNT_DISTINCT"`
- `parse_count_distinct_case_insensitive` — `count(distinct x)` normalises the same way
- `parse_count_distinct_requires_column` — `COUNT(DISTINCT)` errors
- `parse_sum_distinct_keyword` — `SUM(DISTINCT price)` → `func="SUM_DISTINCT"`

## Notes for future waves

1. **Column type metadata.** The biggest gap exposed by Wave 20 is
   that the engine doesn't know whether a `Vec<u64>` column is
   integer-encoded (`value as u64`) or Float64-encoded
   (`f64::to_bits(value)`). This forces `execute_sum` to sum as
   integers, which is wrong for Float64 columns. A future wave
   should add a `column_types: Vec<ColumnType>` field to `Table`
   (populated by the loaders from the Arrow schema) and have
   `execute_sum` dispatch on the type.

2. **Real scan-and-gather kernel.** `SELECT * FROM t WHERE col = N`
   currently filters in-engine because the `ScanEqU64` kernel only
   returns a count + 64-bit mask. A future wave should extend the
   kernel interface to return a vector of matching indices (or
   materialise the matching rows directly), so the gather path
   uses the kernel's SIMD scan instead of a scalar loop.

3. **Range and multi-predicate WHERE.** The `ScanRangeU64` and
   `ScanMultiPredicate` kernels exist but are not wired into the
   executor. The natural next step is to extend `parse_where` to
   return an `enum Filter { Eq(col, u64), Range(col, u64, u64),
   Multi(Vec<(col, op, u64)>) }` and have `execute_count` /
   `execute_select_star` dispatch on the filter kind.

4. **Planner integration.** The `cost_model` parameter to
   `execute_select` is currently unused — the executor picks the L3
   tier by default. A future wave should pass it to the
   `PlanLowerer` (from `src/planner/lowerer.rs`) to pick the
   cheapest tier per operator, then run the kernel that the lowerer
   selected. This is the path to making the TIER extension actually
   change which kernel runs, rather than just being acknowledged.

5. **Approximate aggregation.** The `APPROXIMATE WITHIN ε
   CONFIDENCE δ` extension is parsed but ignored. The HLL and
   Count-Min sketches in `src/sketch/` are the right primitive for
   `COUNT(DISTINCT x) APPROXIMATE` and `SUM(x) APPROXIMATE` — a
   future wave would route to them, sizing the sketch from
   `(epsilon, failure_probability)`.
