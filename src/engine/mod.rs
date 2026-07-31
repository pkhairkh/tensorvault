//! # Query engine — the end-to-end SQL surface.
//!
//! [`QueryEngine`] is the top-level type that ties the SQL parser, the
//! catalog, the kernel table, and the cost model together. It is the
//! DuckDB-style entry point: hand it a SQL string, get back a
//! [`QueryResult`].
//!
//! ## Pipeline
//!
//! ```text
//!   SQL string
//!       │
//!       ▼  sql::parse_with_extensions
//!   (SelectQuery, QueryExtensions)
//!       │
//!       ▼  engine::execute_select
//!   QueryResult
//! ```
//!
//! The parse step lives in [`crate::sql`]; the execute step lives in
//! [`crate::engine::executor`]. This module's [`QueryEngine`] is the
//! glue that owns the catalog and the kernel table, captures the wall-
//! clock time around the pipeline, and returns the result.
//!
//! ## Why a struct (not free functions)
//!
//! Free functions would force every caller to construct a `Catalog`,
//! `KernelTable`, and `CostModel` themselves and pass them to every
//! call. The struct bundles them once and exposes a single `execute`
//! method, which is the shape callers actually want:
//!
//! ```ignore
//! let mut engine = QueryEngine::new();
//! engine.load_parquet("hits.parquet", "hits")?;
//! let result = engine.execute("SELECT count(*) FROM hits")?;
//! println!("{}", result.scalar_u64().unwrap());
//! ```
//!
//! ## Concurrency
//!
//! `QueryEngine` is `Send` but not `Sync` (the catalog is a plain
//! `HashMap`, not a `RwLock<HashMap>`). Callers that want to share an
//! engine across threads should wrap it in an `Arc<Mutex<QueryEngine>>`
//! themselves — the same pattern the catalog module recommends.
//!
//! ## Loading data
//!
//! Two convenience constructors wrap the Parquet and CSV readers from
//! [`crate::datasource`]:
//!
//! - [`QueryEngine::load_parquet`] reads a `.parquet` file and registers
//!   it in the catalog under the given name (or, if no name is given,
//!   under the file's stem).
//! - [`QueryEngine::load_csv`] does the same for `.csv` files.
//!
//! Both return the row count so the caller can sanity-check the load.

pub mod executor;
pub mod tpch;
pub mod result;

pub use executor::execute_select;
pub use result::{QueryResult, ResultColumn};

use crate::catalog::Catalog;
use crate::datasource::table::Table;
use crate::datasource::{read_csv, read_parquet};
use crate::error::{Error, Result};
use crate::kernel::KernelTable;
use crate::planner::CostModel;
use std::sync::Arc;
use std::time::Instant;

/// The top-level engine: catalog + kernel table + cost model, plus a
/// single `execute` method that runs a SQL query end-to-end.
///
/// See the module docs for the pipeline and usage examples.
pub struct QueryEngine {
    /// The table catalog (name → [`Table`]).
    catalog: Catalog,
    /// The kernel table: maps `(Operator, CpuTarget, MemoryTier)` to the
    /// best kernel for that combination on the running CPU.
    kernel_table: Arc<KernelTable>,
    /// The cost model: per-tier throughput estimates. Currently unused
    /// by the executor (it picks L3 by default), but retained in the
    /// struct so a future wave that wires in the `PlanLowerer` doesn't
    /// have to change every call site.
    cost_model: CostModel,
}

impl QueryEngine {
    /// Construct an empty engine with the default kernel table and cost
    /// model. The catalog starts empty — register tables via
    /// [`QueryEngine::register_table`], [`QueryEngine::load_parquet`],
    /// or [`QueryEngine::load_csv`].
    pub fn new() -> Self {
        Self {
            catalog: Catalog::new(),
            kernel_table: Arc::new(KernelTable::new()),
            cost_model: CostModel::default(),
        }
    }

    /// Construct an engine with a custom cost model (e.g., one with a
    /// learned cardinality estimator attached — see
    /// [`CostModel::with_learned`]). The kernel table is still the
    /// default.
    pub fn with_cost_model(cost_model: CostModel) -> Self {
        Self { catalog: Catalog::new(), kernel_table: Arc::new(KernelTable::new()), cost_model }
    }

    /// Borrow the catalog. Read-only access for callers that want to
    /// introspect registered tables without going through SQL.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Borrow the kernel table. Used by callers that want to inspect
    /// the registered kernels or override the auto-detected CPU.
    pub fn kernel_table(&self) -> &KernelTable {
        &self.kernel_table
    }

    /// Borrow the cost model. Used by callers that want to inspect the
    /// hardware parameters (`cpu_freq_hz`, `simd_lanes`, etc.) or the
    /// attached learned estimator.
    pub fn cost_model(&self) -> &CostModel {
        &self.cost_model
    }

    /// Register a table in the catalog. The table's `name` field is
    /// used as the catalog key (so `SELECT * FROM <name>` works after
    /// registration).
    ///
    /// If a table with the same name is already registered, the new
    /// table replaces it (matching [`Catalog::register`]'s overwrite
    /// semantics).
    pub fn register_table(&mut self, table: Table) {
        self.catalog.register(table);
    }

    /// Load a Parquet file into the catalog under the given table name.
    ///
    /// Reads every column of every row group via
    /// [`crate::datasource::read_parquet`], converts each column to the
    /// engine's `Vec<u64>` cell format, and registers the resulting
    /// [`Table`] in the catalog. Returns the row count.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Other`] wrapping the underlying Parquet error
    /// if the file cannot be opened, parsed, or has mismatched column
    /// lengths.
    pub fn load_parquet(&mut self, path: &str, table_name: &str) -> Result<usize> {
        let loaded = read_parquet(path).map_err(|e| Error::Other(e.to_string()))?;
        let row_count = loaded.row_count;
        let mut table = Table::from_loaded(loaded);
        table.name = table_name.to_string();
        self.catalog.register(table);
        Ok(row_count)
    }

    /// Load a CSV file into the catalog under the given table name.
    ///
    /// Reads the file via [`crate::datasource::read_csv`], infers
    /// per-column types (numeric → `i64` as `u64`; non-numeric →
    /// `xxh3_64` hash), and registers the resulting [`Table`] in the
    /// catalog. Returns the row count.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Other`] wrapping the underlying CSV error if
    /// the file cannot be read or has inconsistent column counts.
    pub fn load_csv(&mut self, path: &str, table_name: &str, has_header: bool) -> Result<usize> {
        let loaded = read_csv(path, has_header).map_err(|e| Error::Other(e.to_string()))?;
        let row_count = loaded.row_count;
        let mut table = Table::from_loaded(loaded);
        table.name = table_name.to_string();
        self.catalog.register(table);
        Ok(row_count)
    }

    /// Execute a SQL query and return the result.
    ///
    /// The pipeline is:
    ///
    /// 1. Parse the SQL via [`crate::sql::parse_with_extensions`],
    ///    producing a `SelectQuery` and its turboGP extensions.
    /// 2. Execute via [`execute_select`], which looks up the source
    ///    table in the catalog, picks a kernel, runs it, and packages
    ///    the result.
    /// 3. Capture the wall-clock time and stamp it on the result.
    ///
    /// # Errors
    ///
    /// - [`Error::Parse`] if the SQL is malformed.
    /// - [`Error::NotFound`] if the source table or a referenced column
    ///   does not exist in the catalog.
    /// - [`Error::Other`] for unsupported SQL features (multi-column
    ///   SELECT, range WHERE, etc.).
    pub fn execute(&self, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();

        // Parse the SQL.
        let (query, extensions) = crate::sql::parse_with_extensions(sql).map_err(Error::Parse)?;

        // Execute the parsed query.
        let mut result = execute_select(
            &query,
            &extensions,
            &self.catalog,
            &self.kernel_table,
            &self.cost_model,
        )?;

        // Stamp the elapsed time.
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }

    /// Execute a TPC-H SQL query using the dedicated TPC-H interpreter.
    ///
    /// This path uses [] which
    /// has a richer parser (arithmetic in aggregates, CASE WHEN, EXTRACT,
    /// BETWEEN, IN, subqueries, derived tables, multi-table implicit
    /// joins, HAVING, LEFT JOIN) and a type-aware row-based evaluator
    /// (correctly interprets Float64 columns stored as f64::to_bits).
    pub fn execute_tpch(&self, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();
        let mut result = crate::engine::tpch::parse_and_execute(sql, &self.catalog)?;
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }
}

impl Default for QueryEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::{LoadedColumn, LoadedTable};
    use crate::datasource::Table as DataSourceTable;
    use arrow::array::Int64Array;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc as ArrowArc;
    use tempfile::NamedTempFile;

    /// Build a `Table` with two columns: `id` (0..n) and `x` (cycling 0..7).
    fn make_table(n: usize) -> DataSourceTable {
        let ids: Vec<u64> = (0..n).map(|i| i as u64).collect();
        let xs: Vec<u64> = (0..n).map(|i| (i % 7) as u64).collect();
        DataSourceTable::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![
                LoadedColumn { name: "id".into(), cells: ids, row_count: n, string_search: None },
                LoadedColumn { name: "x".into(), cells: xs, row_count: n, string_search: None },
            ],
            row_count: n,
        })
    }

    /// Build a `Table` with a single integer-encoded column `v`.
    fn make_int_table(values: &[u64]) -> DataSourceTable {
        let n = values.len();
        DataSourceTable::from_loaded(LoadedTable {
            name: "ft".into(),
            columns: vec![LoadedColumn { name: "v".into(), cells: values.to_vec(), row_count: n, string_search: None }],
            row_count: n,
        })
    }

    // -----------------------------------------------------------------
    // DoD tests (the 9 cases from the Wave 20 task brief)
    // -----------------------------------------------------------------

    /// DoD 1: `SELECT count(*) FROM t` returns the table's row count.
    #[test]
    fn dod_count_star_returns_row_count() {
        let mut engine = QueryEngine::new();
        engine.register_table(make_table(1000));
        let r = engine.execute("SELECT count(*) FROM t").expect("query");
        assert_eq!(r.scalar_u64(), Some(1000));
    }

    /// DoD 2: `SELECT count(*) FROM t WHERE x = 42` returns the right count.
    #[test]
    fn dod_count_star_with_where() {
        let mut engine = QueryEngine::new();
        // Make a table where x = 42 appears exactly 7 times.
        let mut xs: Vec<u64> = (0..1000).map(|i| (i % 7) as u64).collect();
        // Make some entries equal to 42.
        for i in 0..7 {
            xs[i * 100] = 42;
        }
        let table = DataSourceTable::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![
                LoadedColumn {
                    name: "id".into(),
                    cells: (0..1000).map(|i| i as u64).collect(),
                    row_count: 1000, string_search: None,
                },
                LoadedColumn { name: "x".into(), cells: xs, row_count: 1000, string_search: None },
            ],
            row_count: 1000,
        });
        engine.register_table(table);

        let r = engine.execute("SELECT count(*) FROM t WHERE x = 42").expect("query");
        assert_eq!(r.scalar_u64(), Some(7));
    }

    /// DoD 3: `SELECT sum(col) FROM t` returns the right sum.
    #[test]
    fn dod_sum_returns_correct_sum() {
        let mut engine = QueryEngine::new();
        engine.register_table(make_table(1000));
        let r = engine.execute("SELECT sum(id) FROM t").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 499_500.0).abs() < 1e-3, "got {s}");
    }

    /// DoD 4: `SELECT * FROM t WHERE id = 5` returns the matching row.
    #[test]
    fn dod_select_star_with_where() {
        let mut engine = QueryEngine::new();
        engine.register_table(make_table(1000));
        let r = engine.execute("SELECT * FROM t WHERE id = 5").expect("query");
        assert_eq!(r.row_count, 1);
        assert_eq!(r.column("id"), Some(&[5u64][..]));
        assert_eq!(r.column("x"), Some(&[5u64][..])); // 5 % 7 = 5
    }

    /// DoD 5: APPROXIMATE extension parses and runs.
    #[test]
    fn dod_count_distinct_with_approximate() {
        let mut engine = QueryEngine::new();
        engine.register_table(make_table(1000));
        let r = engine
            .execute("SELECT count(DISTINCT x) APPROXIMATE WITHIN 0.05 CONFIDENCE 0.95 FROM t")
            .expect("query");
        assert_eq!(r.scalar_u64(), Some(7));
    }

    /// DoD 6: TIER extension parses and runs.
    #[test]
    fn dod_count_star_with_tier_l3() {
        let mut engine = QueryEngine::new();
        engine.register_table(make_table(1000));
        let r = engine.execute("SELECT count(*) FROM t TIER L3").expect("query");
        assert_eq!(r.scalar_u64(), Some(1000));
    }

    /// DoD 7: Invalid SQL returns `Error::Parse`.
    #[test]
    fn dod_invalid_sql_returns_parse_error() {
        let engine = QueryEngine::new();
        let r = engine.execute("SELECT FROM WHERE");
        assert!(matches!(r, Err(Error::Parse(_))), "got {r:?}");
    }

    /// DoD 8: Non-existent table returns `Error::NotFound`.
    #[test]
    fn dod_non_existent_table_returns_not_found() {
        let engine = QueryEngine::new();
        let r = engine.execute("SELECT count(*) FROM missing");
        assert!(matches!(r, Err(Error::NotFound(_))), "got {r:?}");
    }

    /// DoD 9: Load a Parquet file, query it.
    #[test]
    fn dod_load_parquet_and_query() {
        // Build a small Parquet file with one Int64 column `id` of 100 rows.
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        let ids: Vec<i64> = (0..100).collect();
        let arr = ArrowArc::new(Int64Array::from(ids));
        let schema = ArrowArc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(schema, vec![arr]).expect("batch");
        crate::datasource::parquet::write_parquet_for_test(&path, &batch).expect("write");

        let mut engine = QueryEngine::new();
        let n = engine.load_parquet(&path, "loaded").expect("load");
        assert_eq!(n, 100);

        let r = engine.execute("SELECT count(*) FROM loaded").expect("query");
        assert_eq!(r.scalar_u64(), Some(100));

        let r = engine.execute("SELECT sum(id) FROM loaded").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 4950.0).abs() < 1e-3, "got {s}"); // 0+1+...+99 = 4950
    }

    // -----------------------------------------------------------------
    // Additional integration tests
    // -----------------------------------------------------------------

    /// Load a CSV file and query it.
    #[test]
    fn load_csv_and_query() {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        std::fs::write(&path, "id,value\n1,10\n2,20\n3,30\n4,40\n5,50\n").expect("write");

        let mut engine = QueryEngine::new();
        let n = engine.load_csv(&path, "csvt", true).expect("load");
        assert_eq!(n, 5);

        let r = engine.execute("SELECT count(*) FROM csvt").expect("query");
        assert_eq!(r.scalar_u64(), Some(5));

        let r = engine.execute("SELECT count(*) FROM csvt WHERE value = 30").expect("query");
        assert_eq!(r.scalar_u64(), Some(1));

        let r = engine.execute("SELECT sum(value) FROM csvt").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 150.0).abs() < 1e-9, "got {s}"); // 10+20+30+40+50 = 150

        let r = engine.execute("SELECT * FROM csvt WHERE id = 3").expect("query");
        assert_eq!(r.row_count, 1);
        assert_eq!(r.column("id"), Some(&[3u64][..]));
        assert_eq!(r.column("value"), Some(&[30u64][..]));
    }

    /// Sum of an integer-encoded column through the engine API.
    #[test]
    fn engine_sum_integer_column() {
        let mut engine = QueryEngine::new();
        // Integer-encoded column: 1, 2, 3, 4 → sum = 10.
        engine.register_table(make_int_table(&[1, 2, 3, 4]));
        let r = engine.execute("SELECT sum(v) FROM ft").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 10.0).abs() < 1e-9, "got {s}");
    }

    /// The elapsed_us field is populated after `execute`.
    #[test]
    fn execute_populates_elapsed_us() {
        let mut engine = QueryEngine::new();
        engine.register_table(make_table(100));
        let r = engine.execute("SELECT count(*) FROM t").expect("query");
        // elapsed_us should be non-negative (and almost certainly > 0,
        // but we don't assert that to avoid flakes on very fast machines).
        assert!(r.elapsed_us < 1_000_000, "elapsed_us unreasonably large: {}", r.elapsed_us);
    }

    /// Re-registering a table replaces the old one.
    #[test]
    fn register_table_overwrites() {
        let mut engine = QueryEngine::new();
        engine.register_table(make_table(100));
        engine.register_table(make_table(200));
        let r = engine.execute("SELECT count(*) FROM t").expect("query");
        assert_eq!(r.scalar_u64(), Some(200));
    }

    /// `with_cost_model` constructs an engine with a non-default cost model.
    #[test]
    fn with_cost_model_constructs_engine() {
        let cm = CostModel { cpu_freq_hz: 4.0e9, simd_lanes: 16, ..CostModel::default() };
        let engine = QueryEngine::with_cost_model(cm);
        assert_eq!(engine.cost_model().cpu_freq_hz, 4.0e9);
        assert_eq!(engine.cost_model().simd_lanes, 16);
    }

    /// `QueryEngine::default()` is equivalent to `new()`.
    #[test]
    fn default_is_empty() {
        let engine = QueryEngine::default();
        assert!(engine.catalog().is_empty());
    }

    /// Accessors return the right types.
    #[test]
    fn accessors_work() {
        let engine = QueryEngine::new();
        let _cat: &Catalog = engine.catalog();
        let _kt: &KernelTable = engine.kernel_table();
        let _cm: &CostModel = engine.cost_model();
    }

    /// A query against a table with zero rows returns 0 for count(*).
    #[test]
    fn count_star_on_empty_table_returns_zero() {
        let mut engine = QueryEngine::new();
        engine.register_table(make_table(0));
        let r = engine.execute("SELECT count(*) FROM t").expect("query");
        assert_eq!(r.scalar_u64(), Some(0));
    }

    /// A sum against a table with zero rows returns 0.0.
    #[test]
    fn sum_on_empty_table_returns_zero() {
        let mut engine = QueryEngine::new();
        engine.register_table(make_table(0));
        let r = engine.execute("SELECT sum(id) FROM t").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!(s.abs() < 1e-9, "got {s}");
    }

    /// Print does not panic on a real result.
    #[test]
    fn print_does_not_panic() {
        let mut engine = QueryEngine::new();
        engine.register_table(make_table(10));
        let r = engine.execute("SELECT * FROM t").expect("query");
        r.print();
        // No assertion — the test just verifies print doesn't panic.
    }

    /// Extensions other than TIER/APPROXIMATE are accepted (no-ops).
    #[test]
    fn other_extensions_accepted() {
        let mut engine = QueryEngine::new();
        engine.register_table(make_table(100));
        let r = engine
            .execute("SELECT count(*) FROM t USING HYPERLOGLOG MEMORY BUDGET 1048576 ENERGY BUDGET 100 JOULES CONSISTENCY STRONG")
            .expect("query");
        assert_eq!(r.scalar_u64(), Some(100));
    }

    /// Loading a Parquet file under a custom name works.
    #[test]
    fn load_parquet_under_custom_name() {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        let arr = ArrowArc::new(Int64Array::from(vec![1i64, 2, 3]));
        let schema = ArrowArc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(schema, vec![arr]).expect("batch");
        crate::datasource::parquet::write_parquet_for_test(&path, &batch).expect("write");

        let mut engine = QueryEngine::new();
        let n = engine.load_parquet(&path, "custom_name").expect("load");
        assert_eq!(n, 3);

        // The table is registered under "custom_name", not the file stem.
        let r = engine.execute("SELECT count(*) FROM custom_name").expect("query");
        assert_eq!(r.scalar_u64(), Some(3));

        // The file stem is NOT registered.
        let r = engine.execute("SELECT count(*) FROM tempfile");
        assert!(matches!(r, Err(Error::NotFound(_))), "got {r:?}");
    }

    /// Parquet Int64 column round-trips through a load + count + sum.
    #[test]
    fn parquet_int_column_count_and_sum() {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        // Int64 column 1..=5 → integer-encoded as 1u64..=5.
        let arr = ArrowArc::new(Int64Array::from(vec![1i64, 2, 3, 4, 5]));
        let schema = ArrowArc::new(Schema::new(vec![Field::new("v", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(schema, vec![arr]).expect("batch");
        crate::datasource::parquet::write_parquet_for_test(&path, &batch).expect("write");

        let mut engine = QueryEngine::new();
        engine.load_parquet(&path, "ft").expect("load");

        // Count.
        let r = engine.execute("SELECT count(*) FROM ft").expect("query");
        assert_eq!(r.scalar_u64(), Some(5));

        // Sum (integer-encoded: 1+2+3+4+5 = 15).
        let r = engine.execute("SELECT sum(v) FROM ft").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 15.0).abs() < 1e-9, "got {s}");

        // Count distinct.
        let r = engine.execute("SELECT count(DISTINCT v) FROM ft").expect("query");
        assert_eq!(r.scalar_u64(), Some(5));

        // SELECT * with filter.
        let r = engine.execute("SELECT * FROM ft WHERE v = 3").expect("query");
        assert_eq!(r.row_count, 1);
        assert_eq!(r.column("v"), Some(&[3u64][..]));
    }
}
pub mod dispatch;
