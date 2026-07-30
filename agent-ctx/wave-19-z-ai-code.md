# Wave 19 — Parquet Reader (data source + catalog)

**Agent**: z-ai-code
**Date**: 2026-07-30
**Status**: Complete
**Baseline**: 554 tests (535 lib + 7 integration + 12 doc-tests, 1 ignored)
**After Wave 19**: 583 tests (564 lib + 7 integration + 12 doc-tests, 1 ignored)
**Net new**: +29 lib tests (22 datasource + 7 catalog)

## Tasks Completed

### 19-1: `src/datasource/mod.rs` — data source module root

Created the new top-level module that owns external-format ingestion.
The module doc explains the u64 cell contract (the conversion table
that every reader follows) and why string columns are hashed
(lossy: filterable, but the original bytes are not recoverable — a
sidecar bytes arena is deferred to a future wave).

`pub mod` re-exports `csv`, `parquet`, `table`, and the module-level
`pub use` re-exports the most common entry points (`read_csv`,
`read_parquet`, `read_parquet_column`, `LoadedColumn`,
`LoadedTable`, `Table`) so callers can write
`use turbogp::datasource::{read_parquet, Table};` instead of three
imports.

Registered `pub mod datasource;` in `src/lib.rs` (alphabetical, after
`compress`). Updated the crate-level module docs to point at
[`datasource`] and [`catalog`] as Wave 19 additions.

### 19-2: `src/datasource/parquet.rs` — Parquet reader

Two public structs and two public functions:

- `LoadedColumn { name, cells: Vec<u64>, row_count }`
- `LoadedTable { name, columns: Vec<LoadedColumn>, row_count }`
- `read_parquet(path: &str) -> Result<LoadedTable, Box<dyn Error>>`
- `read_parquet_column(path: &str, column_name: &str) -> Result<LoadedColumn, Box<dyn Error>>`

Both `LoadedColumn` and `LoadedTable` derive `Debug + Clone` so the
catalog can snapshot a loaded table cheaply.

#### Implementation notes

1. **Builder/reader pipeline.** Uses
   `ParquetRecordBatchReaderBuilder::try_new(file)` →
   `.with_batch_size(8192).build()` → iterate `RecordBatch` items.
   Batch size of 8192 rows is the same default DuckDB uses for Parquet
   reads; large enough that the per-batch overhead is amortised,
   small enough that a single batch fits in L2.

2. **Type conversion.** Centralised in a private
   `convert_array_to_u64(array: &ArrayRef) -> Vec<u64>` helper. Each
   branch matches on `array.data_type()` then
   `array.as_any().downcast_ref::<ConcreteArray>()`. The branches:

   | Arrow type      | u64 encoding                      |
   |-----------------|-----------------------------------|
   | Int32           | `value as u64` (zero-extends)     |
   | Int64           | `value as u64` (bit-reinterpret)  |
   | Float64         | `f64::to_bits(value)`             |
   | Utf8            | `xxh3::xxh3_64(bytes)` (hash)     |
   | LargeUtf8       | `xxh3::xxh3_64(bytes)` (hash)     |
   | Boolean         | `0u64` / `1u64`                   |
   | Date32          | `days as u64`                     |
   | (anything else) | `0u64` for every row              |

   Each branch is annotated with a comment explaining the conversion
   (per the "type conversions must be documented" constraint).
   Null values are encoded as `0u64` — the engine does not yet track
   a null bitmap per column. Acceptable for ClickBench/TPC-H (both
   are dense).

3. **Single-column read.** `read_parquet_column` resolves the column
   index up front via `builder.schema().column_with_name(...)` so it
   can fail fast on an unknown name. The `parquet` crate still
   materialises every column of every row group (column pruning is a
   row-group-level option, not a ParquetReader option), but we avoid
   the per-column allocation for the columns the caller doesn't want.

4. **Helper for tests.** `write_parquet_for_test(path, batch)` is a
   `pub` helper that wraps `ArrowWriter::try_new` → `write` →
   `close`. Exported so tests in `tests/` (or future benchmarks) can
   manufacture small fixtures without checking in binary assets.

5. **`LoadedTable::name_from_path`.** Strips directory and extension
   to derive the table name (`hits.parquet` → `hits`). Documented
   and unit-tested.

#### Tests in `parquet.rs` (6)

- `read_parquet_round_trip` — writes 100 rows of Int64/Float64/Utf8,
  reads back, verifies every cell matches the expected encoding.
- `read_parquet_single_column` — `read_parquet_column` on the
  `score` column returns just that column's cells.
- `read_parquet_column_unknown_errors` — unknown column name
  produces a "not found" error.
- `read_parquet_int32_column` — exercises the Int32 branch.
- `name_from_path_strips_extension` — three path shapes (absolute,
  relative, no extension).
- `loaded_types_are_clone` — `Clone` derive works for both structs.

### 19-3: `src/datasource/csv.rs` — CSV reader

One public function:

- `read_csv(path: &str, has_header: bool) -> Result<LoadedTable, Box<dyn Error>>`

Reuses `LoadedColumn` and `LoadedTable` from `parquet.rs` (the user
spec says the CSV reader returns a `LoadedTable`). Documented in the
module docs why we don't use `arrow-csv` even though it's in the
dependency tree (the CSV path is deliberately dependency-light and
auditable).

#### Implementation notes

1. **No external CSV crate.** Uses `std::fs::read_to_string` then
   `.lines()` + `.split(',')`. Quoted-field handling is intentionally
   absent — the module docs explain this is fine for ClickBench /
   TPC-H CSV exports.

2. **Per-column type inference.** First pass per column: try to parse
   every value as `i64`. If every value parses, the column is
   numeric (cast `i64 → u64` bit-reinterpret). If any value fails,
   the column is hashed (`xxh3_64`). This matches the spec
   ("Non-numeric columns are hashed to u64").

3. **Line ending handling.** `.lines()` already strips `\n`;
   `.trim_end_matches('\r')` handles `\r\n` from Windows exports.

4. **Empty / blank lines.** Skipped. Trailing newlines don't create
   phantom zero-column rows.

5. **Synthetic column names.** Without a header, columns are named
   `col_0`, `col_1`, … so the catalog can still address them.

6. **Error on ragged rows.** If a row has a different field count
   than the header (or first row), the function returns an error
   with the row number, expected count, and actual count.

#### Tests in `csv.rs` (9)

- `read_csv_numeric_with_header` — 3-row CSV with `id,value` header.
- `read_csv_numeric_no_header` — synthetic `col_0`, `col_1` names.
- `read_csv_mixed_column_is_hashed` — all-non-numeric column is
  hashed; same string produces same cell.
- `read_csv_negative_values` — `-1i64 as u64` bit-reinterpret.
- `read_csv_inconsistent_columns_errors` — ragged row errors out.
- `read_csv_empty_file` — empty file produces an empty table.
- `read_csv_skips_blank_lines` — blank lines (including trailing
  newlines) are skipped.
- `read_csv_handles_crlf` — Windows `\r\n` line endings.
- `read_csv_mixed_columns` — a table with one numeric and one hashed
  column.

### 19-4: `src/datasource/table.rs` — in-memory `Table`

The bridge between the loaders and the executor. Separate type from
`LoadedTable` because `LoadedTable` is owned by the loader (and may
be moved multiple times, e.g. into a `Catalog`), while `Table` is the
final resting shape that the executor borrows.

```rust
pub struct Table {
    pub name: String,
    pub columns: Vec<Vec<u64>>,
    pub column_names: Vec<String>,
    pub row_count: usize,
}

impl Table {
    pub fn from_loaded(loaded: LoadedTable) -> Self
    pub fn column(&self, name: &str) -> Option<&[u64]>
    pub fn column_idx(&self, name: &str) -> Option<usize>
    pub fn row_count(&self) -> usize
    pub fn column_count(&self) -> usize  // bonus
}
```

#### Implementation notes

1. **Defensive length clamp.** `from_loaded` clamps `row_count` to
   the minimum actual column length. The loaders already verify the
   invariant, but a caller that hand-builds a `LoadedTable` with
   mismatched lengths would otherwise cause the executor to read
   past the end of a column. The clamp silently truncates; a test
   verifies this behaviour.

2. **Linear-scan column lookup.** `column_idx` is `O(ncols)`. The
   module docs explain why a `HashMap` doesn't pay for itself:
   lookups are rare (once per query) and turboGP tables are
   wide-and-short (ClickBench's `hits` table has 105 columns).

3. **`Clone` derive.** The catalog can snapshot a `Table` into a
   worker-local copy if it ever needs to.

#### Tests in `table.rs` (7)

- `from_loaded_preserves_data` — name, columns, names, row count.
- `column_lookup_by_name` — `column("id")` returns the right slice;
  unknown name returns `None`.
- `column_idx_lookup` — `column_idx` returns the right index.
- `row_count_accessor` — `row_count()` matches the field.
- `from_loaded_clamps_mismatched_lengths` — defensive clamp works.
- `from_loaded_empty` — empty `LoadedTable` → empty `Table`.
- `table_is_clone` — `Clone` derive works.

### 19-5: `src/catalog/mod.rs` — table catalog

A simple `HashMap<String, Table>` wrapper with convenience accessors.
Documented why there's no concurrency yet (callers wrap in
`Arc<RwLock<Catalog>>` themselves; the morsel executor snapshots the
catalog into per-worker borrows at scheduling time).

```rust
pub struct Catalog { tables: HashMap<String, Table> }

impl Catalog {
    pub fn new() -> Self
    pub fn register(&mut self, table: Table)
    pub fn get(&self, name: &str) -> Option<&Table>
    pub fn get_column(&self, table: &str, column: &str) -> Option<&[u64]>
    pub fn table_names(&self) -> Vec<&str>
    pub fn len(&self) -> usize              // bonus
    pub fn is_empty(&self) -> bool          // bonus
}
impl Default for Catalog
```

#### Implementation notes

1. **`register` overwrites.** Same-name registration replaces the
   existing entry. A test verifies this.

2. **`get_column` two-arg convenience.** Wraps `get` + `Table::column`
   so callers don't have to chain two `Option`s.

3. **`table_names` returns `Vec<&str>`.** Borrows from the
   `HashMap`'s keys. Order is unspecified (follows `HashMap`
   iteration); the test sorts before comparing.

4. **`Default` impl.** Equivalent to `new()`. Lets callers write
   `Catalog::default()` in contexts where the type is inferred.

#### Tests in `catalog/mod.rs` (7)

- `register_and_get` — basic register-then-get round-trip.
- `get_column_works` — two-arg lookup; missing column returns
  `None`; missing table returns `None`.
- `register_overwrites` — re-registering the same name replaces.
- `table_names_lists_all` — three registered tables are all listed.
- `len_and_is_empty` — `len` and `is_empty` track registrations.
- `default_is_empty` — `Catalog::default()` is empty.
- `get_missing_returns_none` — unknown name returns `None`.

### 19-6: Tests + integration

All tests live alongside the code they test (the `#[cfg(test)] mod
tests` convention used elsewhere in turboGP). No `unwrap()` / `expect()`
in non-test code — every fallible call uses `?` or returns an
`Option`. The two `expect()` calls in tests
(`sample_batch().expect("build batch")`,
`NamedTempFile::new().expect("temp file")`) are confined to the
`#[cfg(test)]` block, as the constraint allows.

## DoD Verification

| DoD | Status |
|-----|--------|
| `cargo test` passes (554 existing + new) | ✅ 564 lib + 7 integration + 12 doc-tests, 1 ignored (583 total; +29 new lib tests) |
| `cargo clippy -- -D warnings` passes | ✅ `cargo clippy --all-targets -- -D warnings` clean |
| Can read a Parquet file into turboGP's u64 column format | ✅ `read_parquet_round_trip` writes 100 rows of Int64/Float64/Utf8, reads back, verifies every cell |
| Can read a CSV file into turboGP's u64 column format | ✅ `read_csv_numeric_with_header` + 8 other CSV tests |
| Catalog can register and retrieve tables | ✅ 7 catalog tests, including overwrites and `get_column` two-arg lookup |
| `cargo fmt` clean | ✅ `cargo fmt --check` returns no diffs (only the unstable-feature warnings on stable rustc) |

## Files Created / Modified

| File | Action | Lines | Purpose |
|------|--------|------:|---------|
| `src/datasource/mod.rs` | created | 49 | module root + re-exports + u64 cell contract docs |
| `src/datasource/parquet.rs` | created | 454 | `LoadedColumn`, `LoadedTable`, `read_parquet`, `read_parquet_column`, `write_parquet_for_test`, 6 tests |
| `src/datasource/csv.rs` | created | 254 | `read_csv`, 9 tests |
| `src/datasource/table.rs` | created | 176 | `Table` struct + `from_loaded` + accessors, 7 tests |
| `src/catalog/mod.rs` | created | 174 | `Catalog` struct + 7 tests |
| `src/lib.rs` | modified | +2 | registered `pub mod catalog;` and `pub mod datasource;`; added two module-doc bullets |

**Net new**: 1107 lines across 5 new files, +29 tests.

## Design decisions worth recording

1. **Why `LoadedTable` and `Table` are separate types.** The loaders
   produce `LoadedTable`; the executor consumes `Table`. Keeping them
   separate lets the loader output evolve (e.g., adding schema
   metadata) without changing the executor's borrow contract. The
   bridge is `Table::from_loaded`, which is the only `Table`
   constructor.

2. **Why the CSV reader doesn't use `arrow-csv`.** The CSV path is
   the lowest-common-denominator format; the reader should remain
   auditable without pulling in arrow's full CSV parser. Parquet, by
   contrast, has no simple implementation and gets the full `parquet`
   crate. Documented in `csv.rs` module docs.

3. **Why string columns are hashed.** The kernel table operates on
   64-bit cells. A string column can't fit in a single u64 without
   either (a) truncation (lossy in a different, worse way), (b) an
   interned ID with a sidecar bytes arena (not yet built), or (c) a
   hash. We chose (c) because it preserves equality-filter semantics
   — the engine can still `scan_eq` on a string column and get the
   right matches. The hash is `xxh3_64` (already in deps; used by
   the HLL and Count-Min sketches). Full string support is deferred
   to a future wave.

4. **Why `register` overwrites instead of erroring.** The catalog is
   the bridge between loaders and the executor. The most common
   usage pattern is "load file → register → query". If the same
   table is loaded twice (e.g. the user re-runs a load after editing
   the source file), the second load should replace the first —
   returning an error would force the caller to `drop` explicitly,
   which is friction. Documented + tested.

5. **Why no concurrency on `Catalog`.** The morsel executor
   currently snapshots the catalog into per-worker borrows at
   scheduling time, so the registry itself never sees concurrent
   access during a query. A future wave that adds streaming loads
   (load-while-querying) will need to wrap the catalog in an
   `Arc<RwLock<...>>`; that's a separate concern with different
   tradeoffs (lock contention, snapshot freshness) and is out of
   scope here. Documented in the module docs.

## Notes for future waves

The next natural step is wiring the catalog into the SQL planner.
Currently `src/sql/plan.rs::table_region_id` hashes the table name
to derive a region ID — there's no schema lookup. With the catalog
in place, the planner can resolve `FROM hits` to an actual `Table`
in the catalog and feed its columns directly to the executor. That
change is invasive (it touches the planner, the lowerer, and the
scheduler) and is out of scope for Wave 19, but the catalog is the
prerequisite for it.
