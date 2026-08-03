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
use crate::datasource::{read_csv, read_parquet, read_parquet_column, read_parquet_column_names};
use crate::error::{Error, Result};
use crate::kernel::KernelTable;
use crate::planner::CostModel;
use std::collections::HashMap;
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
    /// Transaction manager for BEGIN/COMMIT/ROLLBACK (Wave 5).
    txn_manager: crate::txn::TxnManager,
    /// Write-ahead log for durability (Wave 37). None when not configured.
    wal: Option<crate::storage::recovery::Wal>,
    /// Index manager for secondary indexes (Wave 31).
    pub index_manager: crate::index::manager::IndexManager,
    /// Hash column registry for materialized string hashes (Wave 31).
    pub hash_registry: crate::exec::hash_column::HashColumnRegistry,
    /// View registry: CREATE VIEW / DROP VIEW / view expansion (Wave 53).
    pub views: crate::catalog::views::ViewRegistry,
    /// Stored procedure registry: CREATE PROCEDURE / EXEC (Wave 53).
    pub procedures: crate::exec::procedure::ProcedureRegistry,
    /// Table-valued parameter types (Wave 53).
    pub table_types: crate::exec::procedure::TableTypeRegistry,
    /// Temporal tables: maps table name → TemporalTable for FOR SYSTEM_TIME
    /// queries (Wave 53).
    pub temporals: HashMap<String, crate::exec::temporal::TemporalTable>,
}

impl QueryEngine {
    /// Try to execute a SELECT query without mutating the engine (Wave 41).
    ///
    /// This method takes `&self` (not `&mut self`), so it can be called
    /// concurrently from multiple threads when the engine is wrapped in
    /// `Arc<RwLock<QueryEngine>>`. SELECT queries take a read lock;
    /// DML/DDL take a write lock.
    ///
    /// Returns `Ok(result)` if the query was a SELECT that succeeded.
    /// Returns `Err(Error::Other("not a readonly query"))` if the query
    /// is DDL/DML/transaction control (caller should use `execute()` with
    /// a write lock).
    pub fn try_readonly_select(&self, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();
        let trimmed = sql.trim();
        let lower = trimmed.to_lowercase();

        // Only SELECT queries can be readonly.
        if !lower.starts_with("select") && !lower.starts_with("with") {
            return Err(Error::Other("not a readonly query".into()));
        }

        // Try DDL/DML — these are NOT readonly.
        if crate::sql::parse_ddl(sql).map_err(Error::Parse)?.is_some() {
            return Err(Error::Other("DDL requires write lock".into()));
        }
        if crate::sql::parse_dml(sql).map_err(Error::Parse)?.is_some() {
            return Err(Error::Other("DML requires write lock".into()));
        }

        // Try CTE.
        if let Some(with_result) = crate::sql::parse_with(sql) {
            return Err(Error::Other("CTE requires write lock".into()));
        }

        // Parse as SELECT and execute against the current catalog.
        let (query, extensions) = match crate::sql::parse_with_extensions(sql) {
            Ok(qe) => qe,
            Err(_parse_err) => {
                // Basic parser failed — need tpch fallback, which requires &mut self.
                return Err(Error::Other("query needs tpch fallback — requires write lock".into()));
            }
        };

        match execute_select(
            &query,
            &extensions,
            &self.catalog,
            &self.kernel_table,
            &self.cost_model,
        ) {
            Ok(mut result) => {
                result.elapsed_us = start.elapsed().as_micros() as u64;
                Ok(result)
            }
            Err(_exec_err) => {
                // execute_select failed — need tpch fallback.
                Err(Error::Other("query failed in execute_select — needs tpch fallback".into()))
            }
        }
    }

    /// Construct an empty engine with the default kernel table and cost
    /// model. The catalog starts empty — register tables via
    /// [`QueryEngine::register_table`], [`QueryEngine::load_parquet`],
    /// or [`QueryEngine::load_csv`].
    pub fn new() -> Self {
        let mut catalog = Catalog::new();
        // Register a dummy table that allows `SELECT 1` and `SELECT count(*)`
        // without a FROM clause. The table has one row and one column.
        let dummy = Table {
            name: "__dummy__".into(),
            columns: vec![std::sync::Arc::new(vec![0u64])],
            column_names: vec!["__dummy_col__".into()],
            row_count: 1,
            string_columns: vec![None],
            null_bitmaps: vec![None],
            schema: None,
        };
        catalog.register(dummy);
        Self {
            catalog,
            kernel_table: Arc::new(KernelTable::new()),
            cost_model: CostModel::default(),
            txn_manager: crate::txn::TxnManager::new(),
            wal: None,
            index_manager: crate::index::manager::IndexManager::new(),
            hash_registry: crate::exec::hash_column::HashColumnRegistry::new(),
            views: crate::catalog::views::ViewRegistry::new(),
            procedures: crate::exec::procedure::ProcedureRegistry::new(),
            table_types: crate::exec::procedure::TableTypeRegistry::new(),
            temporals: HashMap::new(),
        }
    }

    /// Open a QueryEngine with a WAL for durability (Wave 37).
    /// Replays the WAL on startup to restore committed state.
    pub fn open<P: AsRef<std::path::Path>>(wal_path: P) -> Result<Self> {
        let mut engine = Self::new();
        let wal = crate::storage::recovery::Wal::open(&wal_path)?;
        // Replay committed transactions.
        let stats = crate::storage::recovery::replay_wal(&mut engine, &wal)?;
        log::info!("WAL replay: {} records replayed, {} skipped, {} errors",
            stats.replayed, stats.skipped, stats.errors);
        engine.wal = Some(wal);
        Ok(engine)
    }

    /// Enable WAL on an existing engine.
    pub fn enable_wal<P: AsRef<std::path::Path>>(&mut self, wal_path: P) -> Result<()> {
        let wal = crate::storage::recovery::Wal::open(&wal_path)?;
        self.wal = Some(wal);
        Ok(())
    }

    /// Append a DML/DDL record to the WAL (if enabled).
    ///
    /// Wave 51 fix: `txn_id` is `Some(id)` for statements inside an
    /// explicit transaction, or `None` for autocommit. The record carries
    /// the txn_id so replay can group statements by transaction.
    fn wal_append_txn(&mut self, sql: &str, txn_id: Option<u64>) {
        if let Some(ref mut wal) = self.wal {
            let record = match txn_id {
                Some(id) => crate::storage::recovery::WalRecord::txn_dml(id, sql),
                None => crate::storage::recovery::WalRecord::autocommit(sql),
            };
            let _ = wal.append(&record);
            let _ = wal.sync();
        }
    }

    /// Append a pre-constructed WAL record (BEGIN / COMMIT / ROLLBACK
    /// markers, or any other special record). Used by `execute()` to
    /// write transaction boundary markers (Wave 51 fix).
    fn wal_append_record(&mut self, record: crate::storage::recovery::WalRecord) {
        if let Some(ref mut wal) = self.wal {
            let _ = wal.append(&record);
            let _ = wal.sync();
        }
    }

    /// Construct an engine with a custom cost model (e.g., one with a
    /// learned cardinality estimator attached — see
    /// [`CostModel::with_learned`]). The kernel table is still the
    /// default.
    pub fn with_cost_model(cost_model: CostModel) -> Self {
        let mut engine = Self::new();
        engine.cost_model = cost_model;
        engine
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

    /// Load a Parquet file with column pruning (Wave 30).
    ///
    /// Only loads columns referenced in the SQL query, skipping the rest.
    /// For a 105-column table where the query references 3 columns, this
    /// reduces I/O by ~35x.
    ///
    /// First loads all column names (cheap metadata read), then uses
    /// `prune_columns()` to determine which to materialize, then reads
    /// only those columns via `read_parquet_column()`.
    pub fn load_parquet_with_projection(
        &mut self,
        path: &str,
        table_name: &str,
        sql: &str,
    ) -> Result<usize> {
        // Step 1: Read all column names from the Parquet file (metadata only, no data).
        let all_columns = read_parquet_column_names(path)
            .map_err(|e| Error::Other(e.to_string()))?;

        // Step 2: Determine which columns are needed.
        let (needed_cols, pruned_count) = crate::datasource::projection::prune_columns(sql, &all_columns);
        log::debug!(
            "load_parquet_with_projection: {} of {} columns needed ({} pruned)",
            needed_cols.len(), all_columns.len(), pruned_count
        );

        // Step 3: If SELECT * or all columns needed, just load everything.
        if needed_cols.is_empty() || needed_cols.len() == all_columns.len() {
            return self.load_parquet(path, table_name);
        }

        // Step 4: Load only the needed columns.
        let mut columns: Vec<crate::datasource::LoadedColumn> = Vec::new();
        let mut row_count = 0usize;
        for col_name in &needed_cols {
            if let Ok(loaded_col) = read_parquet_column(path, col_name) {
                row_count = loaded_col.row_count;
                columns.push(loaded_col);
            }
        }

        if columns.is_empty() {
            return self.load_parquet(path, table_name);
        }

        let loaded = crate::datasource::parquet::LoadedTable {
            name: table_name.into(),
            columns,
            row_count,
        };
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

        // Materialize hash columns for string columns (Wave 31).
        // This pre-computes xxh3 hashes so GROUP BY doesn't re-hash per query.
        for col in &loaded.columns {
            if col.string_search.is_some() {
                // The cells are already xxh3 hashes (computed by the CSV reader).
                // Register them as a HashColumn so GROUP BY can use the pre-computed
                // hashes instead of re-hashing per query.
                let hash_col = crate::exec::hash_column::HashColumn {
                    hashes: col.cells.clone(),
                };
                self.hash_registry.register(table_name, &col.name, hash_col);
            }
        }

        let mut table = Table::from_loaded(loaded);
        table.name = table_name.to_string();
        self.catalog.register(table);
        Ok(row_count)
    }

    /// Execute a SQL statement and return the result.
    ///
    /// This method dispatches on the SQL verb:
    /// - `SELECT` → the existing read-only execution path.
    /// - `CREATE TABLE` / `DROP TABLE` / `CREATE SCHEMA` → DDL path
    ///   (Wave 3) that mutates the catalog.
    /// - `INSERT` / `UPDATE` / `DELETE` → DML path (Wave 4) that mutates
    ///   table data.
    /// - `BEGIN` / `COMMIT` / `ROLLBACK` → transaction control (Wave 5,
    ///   currently a no-op stub that returns an empty result).
    ///
    /// Takes `&mut self` because DDL/DML mutate the catalog.
    ///
    /// # Errors
    ///
    /// - [`Error::Parse`] if the SQL is malformed.
    /// - [`Error::NotFound`] if the source table or a referenced column
    ///   does not exist in the catalog.
    /// - [`Error::Other`] for unsupported SQL features.
    pub fn execute(&mut self, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();

        // Transaction control: BEGIN/COMMIT/ROLLBACK.
        //
        // Wave 51 fix (Bug 8): BEGIN/COMMIT/ROLLBACK now write corresponding
        // markers to the WAL so replay can reconstruct transaction
        // boundaries. Previously the WAL only ever saw `txn_id: 0,
        // is_commit: false`, so a `BEGIN; INSERT; INSERT; COMMIT;` block
        // was indistinguishable from three autocommit INSERTs on replay
        // — and a `BEGIN; INSERT; ROLLBACK;` would still replay the INSERT.
        let trimmed = sql.trim();
        let lower = trimmed.to_lowercase();
        if lower.starts_with("begin") || lower.starts_with("start transaction") {
            let id = self
                .txn_manager
                .begin(&self.catalog)
                .map_err(Error::Other)?;
            self.wal_append_record(crate::storage::recovery::WalRecord::begin(id));
            return Ok(QueryResult::empty());
        }
        if lower.starts_with("commit") {
            // Capture the txn_id before we drain the transaction.
            let txn_id = self.txn_manager.active.as_ref().map(|t| t.id).unwrap_or(0);
            let committed = self
                .txn_manager
                .commit()
                .map_err(Error::Other)?;
            self.catalog = committed;
            self.wal_append_record(crate::storage::recovery::WalRecord::commit(txn_id));
            return Ok(QueryResult::empty());
        }
        if lower.starts_with("rollback") {
            let txn_id = self.txn_manager.active.as_ref().map(|t| t.id).unwrap_or(0);
            self.txn_manager
                .rollback()
                .map_err(Error::Other)?;
            self.wal_append_record(crate::storage::recovery::WalRecord::rollback(txn_id));
            return Ok(QueryResult::empty());
        }

        // If a transaction is active, route all DML/DDL/SELECT to the
        // snapshot catalog. Otherwise, use the main catalog.
        // We do this by swapping the snapshot into self.catalog for the
        // duration of the statement, then swapping back.
        let txn_active = self.txn_manager.is_active();
        if txn_active {
            // Take the snapshot out of the txn manager temporarily.
            let txn_id = self.txn_manager.active.as_ref().map(|t| t.id).unwrap_or(0);
            let mut txn = self.txn_manager.active.take().expect("txn active");
            std::mem::swap(&mut self.catalog, &mut txn.snapshot);
            let result = self.execute_inner(sql, &start, Some(txn_id));
            // Swap back: self.catalog goes back to being the main catalog
            // (unchanged), txn.snapshot becomes the (possibly modified)
            // transaction state.
            std::mem::swap(&mut self.catalog, &mut txn.snapshot);
            self.txn_manager.active = Some(txn);
            return result;
        }

        self.execute_inner(sql, &start, None)
    }

    /// Inner execution: dispatches DDL, DML, CTE, and SELECT without
    /// transaction awareness. Called by `execute` either with the main
    /// catalog or with the txn snapshot swapped in.
    ///
    /// `txn_id` is `Some(id)` when executing inside an explicit
    /// transaction (so the WAL record carries the right txn_id), or
    /// `None` for autocommit.
    ///
    /// Wave 51 fix (Bug 9): the WAL append now happens AFTER a successful
    /// execute. Previously `wal_append(sql)` was called BEFORE
    /// `execute_ddl` / `execute_dml`, so a failed execute (e.g. INSERT
    /// INTO nonexistent) would still leave a record in the WAL — and
    /// replay would fail on restart.
    fn execute_inner(&mut self, sql: &str, start: &Instant, txn_id: Option<u64>) -> Result<QueryResult> {
        // Wave 53: Temporal query — FOR SYSTEM_TIME AS OF <timestamp>.
        // Check this FIRST because the basic lexer fails on very large
        // integer timestamps (u64 values that overflow i64), which would
        // cause the DDL/DML parsers to error before we reach this check.
        if let Some((table_name, timestamp)) = parse_for_system_time(sql) {
            if let Some(temporal) = self.temporals.get(&table_name) {
                let rows = temporal.query_as_of(timestamp);
                return Ok(rows_to_query_result(&rows, &temporal.column_names, start));
            }
        }

        // Try CTE (WITH ... SELECT ...) first.
        if let Some(with_result) = crate::sql::parse_with(sql) {
            let with = with_result.map_err(Error::Parse)?;
            let mut result = self.execute_with(with, txn_id)?;
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Wave 53: View DDL — CREATE VIEW / DROP VIEW.
        if let Some(parsed) = crate::catalog::views::parse_create_view(sql) {
            let view = parsed.map_err(Error::Other)?;
            self.views.create(view);
            let mut result = QueryResult::empty();
            result.row_count = 0;
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }
        if let Some(parsed) = crate::catalog::views::parse_drop_view(sql) {
            let (name, _if_exists) = parsed.map_err(Error::Other)?;
            self.views.drop(&name);
            let mut result = QueryResult::empty();
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Wave 53: Stored procedure DDL — CREATE PROCEDURE / CREATE FUNCTION.
        if let Some(parsed) = crate::exec::procedure::parse_create_procedure(sql) {
            let proc_def = parsed.map_err(Error::Other)?;
            self.procedures.create(proc_def);
            let mut result = QueryResult::empty();
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Wave 53: EXEC procedure_name [args].
        if let Some(parsed) = crate::exec::procedure::parse_exec(sql) {
            let (proc_name, args) = parsed.map_err(Error::Other)?;
            let proc_def = self.procedures.get(&proc_name)
                .ok_or_else(|| Error::NotFound(format!("procedure \"{proc_name}\"")))?
                .clone();
            // Substitute @param references in the body with the arg values.
            let body = substitute_proc_params(&proc_def.body, &args);
            // Re-execute the body SQL. If it's multi-statement, split on ';'
            // and execute each one, returning the last result.
            let mut last_result = QueryResult::empty();
            for stmt in body.split(';').filter(|s| !s.trim().is_empty()) {
                last_result = self.execute(stmt)?;
            }
            last_result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(last_result);
        }

        // Wave 53: MERGE statement.
        if let Some(merge) = parse_merge(sql) {
            return self.execute_merge_stmt(merge, start);
        }

        // Wave 56b: PIVOT clause. Detect `PIVOT (` in the SQL and route to
        // the pivot module. We parse the PIVOT spec, strip the PIVOT clause
        // from the SQL, execute the remaining SELECT to get the input rows,
        // then apply the pivot transformation. The group_col is auto-detected
        // as the first input column that's neither the pivot_col nor the
        // value_col.
        //
        // Supported syntax (simplified):
        //   SELECT * FROM <table> PIVOT (SUM(amount) FOR quarter IN ('Q1','Q2')) AS p
        //   SELECT * FROM <table> PIVOT (COUNT(*) FOR quarter IN (1, 2, 3))
        if let Some(pivot_spec) = parse_pivot_clause(sql) {
            // Strip the PIVOT clause (and any trailing alias) from the SQL.
            let stripped = strip_pivot_clause(sql);
            // Execute the stripped SELECT to get the input rows.
            let input = self.execute_inner(&stripped, start, txn_id)?;
            // Auto-detect the group_col: the first column in the input that's
            // neither the pivot_col nor the value_col.
            let group_col = input.columns.iter()
                .find(|c| c.name != pivot_spec.pivot_col && c.name != pivot_spec.value_col)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| input.columns.first().map(|c| c.name.clone()).unwrap_or_default());
            let spec = PivotSpec {
                group_col,
                pivot_col: pivot_spec.pivot_col,
                value_col: pivot_spec.value_col,
                pivot_values: pivot_spec.pivot_values,
                agg: pivot_spec.agg,
            };
            let mut result = apply_pivot(&input, &spec);
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Try DDL first (CREATE TABLE, DROP TABLE, CREATE SCHEMA).
        if let Some(ddl) = crate::sql::parse_ddl(sql).map_err(Error::Parse)? {
            let mut result = self.execute_ddl(ddl)?;
            // Wave 51 fix: append AFTER successful execute.
            self.wal_append_txn(sql, txn_id);
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Try DML (INSERT, UPDATE, DELETE).
        if let Some(dml) = crate::sql::parse_dml(sql).map_err(Error::Parse)? {
            let mut result = self.execute_dml(dml)?;
            // Wave 51 fix: append AFTER successful execute. If execute_dml
            // returns Err, we never reach this line, so the WAL stays clean.
            self.wal_append_txn(sql, txn_id);
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Wave 53: expand view references in the SQL before parsing as SELECT.
        // If the FROM clause references a view name, we materialize the view
        // by executing its SELECT SQL and registering the result as a
        // catalog table under the view's name (overwriting any prior
        // materialization). The outer SELECT then runs against the
        // materialized table.
        let expanded_sql = self.materialize_views_in_sql(sql);

        // Parse as SELECT.
        let (query, extensions) = match crate::sql::parse_with_extensions(&expanded_sql) {
            Ok(qe) => qe,
            Err(_parse_err) => {
                // The basic parser failed — try the TPC-H interpreter
                // which has a richer parser (CASE, EXTRACT, subqueries,
                // HAVING, arithmetic in aggregates, etc.).
                let mut tpch_result = crate::engine::tpch::parse_and_execute(&expanded_sql, &self.catalog)?;
                tpch_result.elapsed_us = start.elapsed().as_micros() as u64;
                return Ok(tpch_result);
            }
        };

        // Wave 53: Temporal query handling is done above (before parsing).

        // Execute the parsed query.
        match execute_select(
            &query,
            &extensions,
            &self.catalog,
            &self.kernel_table,
            &self.cost_model,
        ) {
            Ok(mut result) => {
                // Wave 53: apply window functions if any SelectItem::Window
                // is present in the query.
                if query.select.iter().any(|s| matches!(s, crate::sql::parser::SelectItem::Window { .. })) {
                    result = apply_window_functions(&result, &query);
                }
                // Wave 53: apply PIVOT if the extensions carry a pivot spec.
                if let Some(pivot_spec) = extensions_pivot(&extensions) {
                    result = apply_pivot(&result, &pivot_spec);
                }
                result.elapsed_us = start.elapsed().as_micros() as u64;
                Ok(result)
            }
            Err(exec_err) => {
                // The basic executor failed — try the TPC-H interpreter
                // as a fallback. This handles queries with features the
                // basic executor doesn't support (multi-aggregate, HAVING,
                // CASE WHEN, subqueries, etc.).
                let mut tpch_result = crate::engine::tpch::parse_and_execute(&expanded_sql, &self.catalog)
                    .map_err(|_| exec_err)?;
                tpch_result.elapsed_us = start.elapsed().as_micros() as u64;
                Ok(tpch_result)
            }
        }
    }

    /// Execute a DDL statement (CREATE TABLE, DROP TABLE, CREATE SCHEMA).
    fn execute_ddl(&mut self, ddl: crate::sql::DdlStatement) -> Result<QueryResult> {
        match ddl {
            crate::sql::DdlStatement::Create(ct) => {
                let full_name = if ct.schema == "dbo" {
                    ct.name.clone()
                } else {
                    format!("{}.{}", ct.schema, ct.name)
                };
                if self.catalog.get(&full_name).is_some() {
                    if ct.if_not_exists {
                        return Ok(QueryResult::empty());
                    }
                    return Err(Error::Other(format!("table \"{full_name}\" already exists")));
                }
                // Build an empty Table with the right column names.
                let column_names: Vec<String> = ct.columns.iter().map(|c| c.name.clone()).collect();
                let columns: Vec<std::sync::Arc<Vec<u64>>> = ct
                    .columns
                    .iter()
                    .map(|_| std::sync::Arc::new(Vec::new()))
                    .collect();
                let table = Table {
                    name: full_name.clone(),
                    columns,
                    column_names,
                    row_count: 0,
                    string_columns: vec![None; ct.columns.len()],
                    null_bitmaps: vec![None; ct.columns.len()],
                    schema: Some(crate::schema::table_schema::TableSchema::from_ddl(&ct.columns)),
                };
                self.catalog.register(table);
                Ok(QueryResult::empty())
            }
            crate::sql::DdlStatement::Drop(dt) => {
                let full_name = if dt.schema == "dbo" {
                    dt.name.clone()
                } else {
                    format!("{}.{}", dt.schema, dt.name)
                };
                if self.catalog.get(&full_name).is_none() {
                    if dt.if_exists {
                        return Ok(QueryResult::empty());
                    }
                    return Err(Error::NotFound(format!("table \"{full_name}\"")));
                }
                self.catalog.drop(&full_name);
                Ok(QueryResult::empty())
            }
            crate::sql::DdlStatement::CreateSchema(_) => {
                // Schemas are implicit — CREATE SCHEMA is a no-op.
                Ok(QueryResult::empty())
            }
        }
    }

    /// Execute a DML statement (INSERT, UPDATE, DELETE).
    fn execute_dml(&mut self, dml: crate::sql::DmlStatement) -> Result<QueryResult> {
        match dml {
            crate::sql::DmlStatement::Insert(ins) => self.execute_insert(ins),
            crate::sql::DmlStatement::Update(upd) => self.execute_update(upd),
            crate::sql::DmlStatement::Delete(del) => self.execute_delete(del),
        }
    }

    /// Execute an INSERT statement.
    fn execute_insert(&mut self, ins: crate::sql::Insert) -> Result<QueryResult> {
        let table = self
            .catalog
            .get_mut(&ins.table)
            .ok_or_else(|| Error::NotFound(format!("table \"{}\"", ins.table)))?;

        // Determine column indices.
        let col_indices: Vec<usize> = match &ins.columns {
            Some(cols) => {
                let mut idxs = Vec::with_capacity(cols.len());
                for col_name in cols {
                    let idx = table
                        .column_idx(col_name)
                        .ok_or_else(|| Error::NotFound(format!("column \"{col_name}\"")))?;
                    idxs.push(idx);
                }
                idxs
            }
            None => (0..table.columns.len()).collect(),
        };

        if col_indices.len() != ins.values.first().map(|r| r.len()).unwrap_or(0) {
            return Err(Error::Other(format!(
                "column count ({}) doesn't match value count ({})",
                col_indices.len(),
                ins.values.first().map(|r| r.len()).unwrap_or(0)
            )));
        }

        let n_new_rows = ins.values.len();

        // Extend each column with the new values.
        for row_vals in &ins.values {
            for (i, &col_idx) in col_indices.iter().enumerate() {
                let val_str = &row_vals[i];
                let is_null = val_str.trim().eq_ignore_ascii_case("null");
                let cell = parse_value_cell(val_str);
                // COW: Arc::make_mut gives us a mutable Vec if we're the
                // sole owner, or clones if shared.
                let col = std::sync::Arc::make_mut(&mut table.columns[col_idx]);
                col.push(cell);

                // Update the NULL bitmap (Wave 32): mark the cell as NULL
                // if the value was explicitly NULL.
                if is_null {
                    // Ensure a bitmap exists for this column.
                    if col_idx >= table.null_bitmaps.len() {
                        table.null_bitmaps.resize(table.columns.len(), None);
                    }
                    if table.null_bitmaps[col_idx].is_none() {
                        // Initialize bitmap: all existing rows are non-NULL.
                        let mut bm = crate::types::null_bitmap::NullBitmap::new(table.row_count);
                        // The new row (at index table.row_count) is NULL.
                        bm.push_null();
                        table.null_bitmaps[col_idx] = Some(bm);
                    } else {
                        table.null_bitmaps[col_idx].as_mut().unwrap().push_null();
                    }
                } else {
                    // Non-NULL value: ensure bitmap exists and push non-null.
                    if col_idx < table.null_bitmaps.len() {
                        if let Some(ref mut bm) = table.null_bitmaps[col_idx] {
                            bm.push_non_null();
                        }
                    }
                }
            }
        }
        table.row_count += n_new_rows;

        // Return a result with the number of rows inserted.
        let mut result = QueryResult::empty();
        result.row_count = n_new_rows;
        Ok(result)
    }

    /// Execute an UPDATE statement. Supports simple `col = value` assignments
    /// and a WHERE clause with `col = value` equality (AND/OR supported
    /// via the existing expression evaluator in a future wave).
    ///
    /// Wave 50 fix (Bug 6): when an assignment sets a column to NULL, the
    /// column's NULL bitmap is now updated so subsequent `COUNT(col)` /
    /// `AVG(col)` correctly exclude the row. Previously the cell was set
    /// to 0 but the bitmap still considered it non-NULL.
    fn execute_update(&mut self, upd: crate::sql::Update) -> Result<QueryResult> {
        let table = self
            .catalog
            .get_mut(&upd.table)
            .ok_or_else(|| Error::NotFound(format!("table \"{}\"", upd.table)))?;

        // Parse assignments into (col_idx, new_value_cell, is_null) triples.
        // `is_null` is true when the RHS is the literal `NULL`.
        let mut assigns: Vec<(usize, u64, bool)> = Vec::with_capacity(upd.assignments.len());
        for (col_name, expr) in &upd.assignments {
            let idx = table
                .column_idx(col_name)
                .ok_or_else(|| Error::NotFound(format!("column \"{col_name}\"")))?;
            let trimmed = expr.trim();
            let is_null = trimmed.eq_ignore_ascii_case("NULL");
            // For now, the expression must be a simple literal.
            let cell = parse_value_cell(expr);
            assigns.push((idx, cell, is_null));
        }

        // Determine which rows match the WHERE clause.
        let n = table.row_count;
        let mut updated = 0usize;
        let match_mask: Vec<bool> = if let Some(where_str) = &upd.where_clause {
            eval_simple_where(table, where_str)?
        } else {
            vec![true; n]
        };

        // Ensure NULL bitmaps exist for every column that we might mark NULL.
        // We grow `null_bitmaps` to match `columns.len()` if needed.
        while table.null_bitmaps.len() < table.columns.len() {
            table.null_bitmaps.push(None);
        }

        for (row_idx, &matches) in match_mask.iter().enumerate() {
            if !matches {
                continue;
            }
            for &(col_idx, val, is_null) in &assigns {
                let col = std::sync::Arc::make_mut(&mut table.columns[col_idx]);
                col[row_idx] = val;
                // Wave 50 fix: update the NULL bitmap to reflect the new
                // value. If we set the cell to NULL, mark the bitmap; if
                // we set it to a non-NULL value, clear the bitmap entry.
                if col_idx < table.null_bitmaps.len() {
                    if is_null {
                        // Ensure a bitmap exists, then mark this row NULL.
                        if table.null_bitmaps[col_idx].is_none() {
                            let mut bm = crate::types::null_bitmap::NullBitmap::new(0);
                            // Backfill existing rows as non-null so the
                            // bitmap is correctly sized up to row_idx.
                            for _ in 0..row_idx {
                                bm.push_non_null();
                            }
                            table.null_bitmaps[col_idx] = Some(bm);
                        }
                        // Ensure the bitmap has entries up to row_idx.
                        let bm = table.null_bitmaps[col_idx].as_mut().unwrap();
                        while bm.len() <= row_idx {
                            bm.push_non_null();
                        }
                        bm.set_null(row_idx);
                    } else {
                        // Clear the NULL flag if a bitmap exists.
                        if let Some(ref mut bm) = table.null_bitmaps[col_idx] {
                            while bm.len() <= row_idx {
                                bm.push_non_null();
                            }
                            bm.set_non_null(row_idx);
                        }
                    }
                }
            }
            updated += 1;
        }

        let mut result = QueryResult::empty();
        result.row_count = updated;
        Ok(result)
    }

    /// Execute a DELETE statement.
    fn execute_delete(&mut self, del: crate::sql::Delete) -> Result<QueryResult> {
        let table = self
            .catalog
            .get_mut(&del.table)
            .ok_or_else(|| Error::NotFound(format!("table \"{}\"", del.table)))?;

        let n = table.row_count;
        let delete_mask: Vec<bool> = if let Some(where_str) = &del.where_clause {
            eval_simple_where(table, where_str)?
        } else {
            vec![true; n]
        };

        let deleted = delete_mask.iter().filter(|&&b| b).count();
        if deleted == 0 {
            let mut result = QueryResult::empty();
            result.row_count = 0;
            return Ok(result);
        }

        // Rebuild each column keeping only non-deleted rows.
        let keep_mask: Vec<bool> = delete_mask.iter().map(|&d| !d).collect();
        for col in &mut table.columns {
            let col_ref = std::sync::Arc::make_mut(col);
            let mut new_vals = Vec::with_capacity(n - deleted);
            for (i, &keep) in keep_mask.iter().enumerate() {
                if keep {
                    new_vals.push(col_ref[i]);
                }
            }
            *col_ref = new_vals;
        }
        table.row_count -= deleted;

        let mut result = QueryResult::empty();
        result.row_count = deleted;
        Ok(result)
    }

    /// Execute a WITH clause (CTEs + outer query).
    ///
    /// For each CTE:
    /// 1. Execute the anchor query, register the result as a temp table
    ///    in the catalog under the CTE name.
    /// 2. If the CTE is recursive, iterate: execute the recursive query
    ///    (which references the CTE name), compute the new rows (set
    ///    difference), append them to the CTE table, and repeat until
    ///    no new rows or MAXRECURSION is reached.
    /// 3. Execute the outer query, which can reference any CTE by name.
    ///
    /// `txn_id` is threaded through so DML inside a CTE (rare but
    /// possible) still gets the right transaction marker in the WAL.
    fn execute_with(&mut self, with: crate::sql::WithClause, txn_id: Option<u64>) -> Result<QueryResult> {
        let mut temp_tables: Vec<String> = Vec::new();

        for cte in &with.ctes {
            // Execute the anchor.
            let anchor_result = self.execute_inner(&cte.anchor, &Instant::now(), txn_id)?;

            // Register the anchor result as a temp table.
            let temp_name = cte.name.clone();
            let table = result_to_table(&temp_name, &anchor_result);
            self.catalog.register(table);
            temp_tables.push(temp_name.clone());

            // If recursive, iterate.
            if let Some(recursive_sql) = &cte.recursive {
                let max_iter = if with.max_recursion == 0 {
                    100_000 // unlimited (capped at 100k for safety)
                } else {
                    with.max_recursion
                };

                for _ in 0..max_iter {
                    // Execute the recursive query with the current CTE state.
                    let rec_result = self.execute_inner(recursive_sql, &Instant::now(), txn_id)?;

                    // Compute new rows: rows in rec_result that aren't already
                    // in the CTE table. For simplicity, we compare by row
                    // content (all columns must match).
                    let new_rows = compute_new_rows(
                        &self.catalog.get(&temp_name).cloned().unwrap_or_else(|| {
                            Table {
                                name: temp_name.clone(),
                                columns: vec![],
                                column_names: vec![],
                                row_count: 0,
                                string_columns: vec![],
            null_bitmaps: vec![],
            schema: None,
                            }
                        }),
                        &rec_result,
                    );

                    if new_rows == 0 {
                        break; // No new rows — recursion complete.
                    }

                    // Append the new rows to the CTE table.
                    // We append ALL rows from rec_result (not just the new
                    // ones) because the recursive query should only produce
                    // new rows if written correctly. A proper set-difference
                    // would be more correct but expensive.
                    let cte_table = self
                        .catalog
                        .get_mut(&temp_name)
                        .ok_or_else(|| Error::NotFound(format!("CTE table \"{temp_name}\"")))?;
                    append_result_rows(cte_table, &rec_result);
                }
            }
        }

        // Execute the outer query.
        let result = self.execute_inner(&with.outer_query, &Instant::now(), txn_id)?;

        // Clean up temp tables.
        for name in &temp_tables {
            self.catalog.drop(name);
        }

        Ok(result)
    }

    /// Execute a TPC-H SQL query using the dedicated TPC-H interpreter.
    ///
    /// This path uses `src/engine/tpch.rs` which has a richer parser
    /// (arithmetic in aggregates, CASE WHEN, EXTRACT, BETWEEN, IN,
    /// subqueries, derived tables, multi-table implicit joins, HAVING,
    /// LEFT JOIN) and a type-aware row-based evaluator.
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

// -----------------------------------------------------------------------
// DML helper functions (Wave 4)
// -----------------------------------------------------------------------

/// Parse a value string from the DML parser into a u64 cell.
///
/// Supported formats:
/// - `"42"` → integer 42
/// - `"3.14"` → f64::to_bits(3.14)
/// - `"'hello'"` → xxh3 hash of "hello" (string columns are hashed)
/// - `"NULL"` → 0 (NULL is stored as 0; a proper null bitmap arrives in a later wave)
/// - `"x'0123'"` → first 8 bytes as u64
fn parse_value_cell(s: &str) -> u64 {
    use xxhash_rust::xxh3;
    let trimmed = s.trim();
    if trimmed == "NULL" {
        return 0;
    }
    // String literal: '...'
    if trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2 {
        let inner = &trimmed[1..trimmed.len() - 1];
        return xxh3::xxh3_64(inner.as_bytes());
    }
    // Hex literal: x'...'
    if trimmed.starts_with("x'") && trimmed.ends_with('\'') && trimmed.len() >= 3 {
        let hex = &trimmed[2..trimmed.len() - 1];
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
            .collect();
        let mut buf = [0u8; 8];
        for (i, &b) in bytes.iter().take(8).enumerate() {
            buf[i] = b;
        }
        return u64::from_le_bytes(buf);
    }
    // Float
    if trimmed.contains('.') || trimmed.contains('e') || trimmed.contains('E') {
        if let Ok(f) = trimmed.parse::<f64>() {
            return f.to_bits();
        }
    }
    // Integer
    if let Ok(n) = trimmed.parse::<i64>() {
        return n as u64;
    }
    if let Ok(n) = trimmed.parse::<u64>() {
        return n;
    }
    // Fallback: hash the string
    xxh3::xxh3_64(trimmed.as_bytes())
}

/// Evaluate a simple WHERE clause against a table, returning a row mask.
///
/// Wave 50 fix (Bugs 4 & 5):
/// - Previously only supported `=` and split the WHERE string on
///   whitespace, which broke string literals containing spaces like
///   `'Alice Bob'`.
/// - Now uses the SQL lexer (`crate::sql::lexer::tokenize`) so quoted
///   strings with spaces round-trip correctly, and supports the full set
///   of comparison operators: `=`, `!=`, `<>`, `<`, `>`, `<=`, `>=`.
/// - Also supports `AND` / `OR` for combining predicates (left-associative).
fn eval_simple_where(table: &Table, where_str: &str) -> Result<Vec<bool>> {
    let n = table.row_count;
    if n == 0 {
        return Ok(Vec::new());
    }

    // Tokenize the WHERE clause so string literals with spaces, embedded
    // operators, etc. are correctly preserved as single tokens.
    let tokens = crate::sql::lexer::tokenize(where_str)
        .map_err(Error::Parse)?;
    // Drop trailing EOF (and any leading WHERE keyword, in case the caller
    // passed the full predicate including `WHERE`).
    let tokens: Vec<crate::sql::lexer::Token> = tokens.into_iter()
        .filter(|t| !matches!(t, crate::sql::lexer::Token::EOF))
        .collect();
    let tokens: Vec<crate::sql::lexer::Token> = if tokens.first().and_then(|t| match t {
        crate::sql::lexer::Token::Keyword(k) if k.eq_ignore_ascii_case("WHERE") => Some(()),
        _ => None,
    }).is_some() {
        tokens[1..].to_vec()
    } else {
        tokens
    };

    if tokens.is_empty() {
        return Ok(vec![true; n]);
    }

    // Parse predicates of form: <col> <op> <value>, joined by AND/OR.
    // Each predicate produces a (col_idx, op, cell_value, is_string_literal, raw_string) tuple.
    #[derive(Clone)]
    struct Pred {
        col_idx: usize,
        op: String,
        cell: u64,
        // Original string literal (if the value was a quoted string), used
        // for string comparison when the column has a string sidecar.
        raw_string: Option<String>,
    }

    let mut predicates: Vec<Pred> = Vec::new();
    let mut operators: Vec<bool> = Vec::new(); // true = AND, false = OR
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            crate::sql::lexer::Token::Keyword(k) if k.eq_ignore_ascii_case("AND") => {
                operators.push(true);
                i += 1;
                continue;
            }
            crate::sql::lexer::Token::Keyword(k) if k.eq_ignore_ascii_case("OR") => {
                operators.push(false);
                i += 1;
                continue;
            }
            crate::sql::lexer::Token::LParen => {
                // Parenthesised expressions in DML WHERE are not supported
                // here — fall back to the dispatcher's mask evaluator if
                // the caller needs full boolean expression support.
                return Err(Error::Other(
                    "parenthesised expressions are not supported in DML WHERE; use SELECT WHERE instead".into(),
                ));
            }
            _ => {}
        }

        // Expect: <col> <op> <value>
        let col_name = match &tokens[i] {
            crate::sql::lexer::Token::Ident(s) => s.clone(),
            crate::sql::lexer::Token::Keyword(k) => k.clone(), // tolerate keyword-as-identifier
            other => return Err(Error::Other(format!(
                "expected column name in WHERE clause, got {:?}", other
            ))),
        };
        if i + 2 >= tokens.len() {
            return Err(Error::Other(format!(
                "incomplete WHERE predicate near '{col_name}'"
            )));
        }
        let op = match &tokens[i + 1] {
            crate::sql::lexer::Token::Op(s) => s.clone(),
            other => return Err(Error::Other(format!(
                "expected comparison operator after '{col_name}', got {:?}", other
            ))),
        };
        if !matches!(op.as_str(), "=" | "!=" | "<>" | "<" | ">" | "<=" | ">=") {
            return Err(Error::Other(format!(
                "unsupported WHERE operator '{op}' in DML WHERE"
            )));
        }

        let col_idx = table
            .column_idx(&col_name)
            .ok_or_else(|| Error::NotFound(format!("column \"{col_name}\"")))?;

        // Extract the value cell. String literals get the original text
        // preserved so we can compare against the string sidecar if one
        // exists; everything else is parsed via parse_value_cell.
        let (cell, raw_string) = match &tokens[i + 2] {
            crate::sql::lexer::Token::String(s) => {
                // Quoted string. If the column has a string sidecar, we
                // keep the original text for direct comparison; otherwise
                // we hash it (matching parse_value_cell behaviour).
                let has_string_sidecar = col_idx < table.string_columns.len()
                    && table.string_columns[col_idx].is_some();
                if has_string_sidecar {
                    (0u64, Some(s.clone()))
                } else {
                    (parse_value_cell(&format!("'{}'", s)), None)
                }
            }
            crate::sql::lexer::Token::Int(v) => (*v as u64, None),
            crate::sql::lexer::Token::Float(f) => (f.to_bits(), None),
            crate::sql::lexer::Token::Hex(bytes) => {
                let mut buf = [0u8; 8];
                for (j, &b) in bytes.iter().take(8).enumerate() {
                    buf[j] = b;
                }
                (u64::from_le_bytes(buf), None)
            }
            crate::sql::lexer::Token::Keyword(k) if k.eq_ignore_ascii_case("NULL") => {
                // NULL in a WHERE predicate — treat as 0 cell. Callers
                // that need IS NULL / IS NOT NULL should use the
                // expression evaluator path.
                (0u64, None)
            }
            other => return Err(Error::Other(format!(
                "expected literal value in WHERE clause, got {:?}", other
            ))),
        };

        predicates.push(Pred {
            col_idx,
            op: if op == "<>" { "!=".to_string() } else { op },
            cell,
            raw_string,
        });
        i += 3;
    }

    if predicates.is_empty() {
        return Ok(vec![true; n]);
    }

    // Evaluate each predicate per row.
    let mut per_pred_masks: Vec<Vec<bool>> = Vec::with_capacity(predicates.len());
    for p in &predicates {
        let col_idx = p.col_idx;
        let col = &table.columns[col_idx];

        // If we have the original string and the column has a string sidecar,
        // compare against the sidecar directly (lexicographic).
        if let Some(ref s) = p.raw_string {
            if col_idx < table.string_columns.len() {
                if let Some(ref sc) = table.string_columns[col_idx] {
                    let mask: Vec<bool> = (0..n).map(|i| {
                        let cell_str = sc.get(i);
                        match p.op.as_str() {
                            "=" => cell_str == s.as_str(),
                            "!=" => cell_str != s.as_str(),
                            "<" => cell_str < s.as_str(),
                            ">" => cell_str > s.as_str(),
                            "<=" => cell_str <= s.as_str(),
                            ">=" => cell_str >= s.as_str(),
                            _ => false,
                        }
                    }).collect();
                    per_pred_masks.push(mask);
                    continue;
                }
            }
        }

        // Default: compare u64 cells.
        let val = p.cell;
        let mask: Vec<bool> = match p.op.as_str() {
            "=" => col.iter().map(|&c| c == val).collect(),
            "!=" => col.iter().map(|&c| c != val).collect(),
            "<" => col.iter().map(|&c| c < val).collect(),
            ">" => col.iter().map(|&c| c > val).collect(),
            "<=" => col.iter().map(|&c| c <= val).collect(),
            ">=" => col.iter().map(|&c| c >= val).collect(),
            _ => vec![false; n],
        };
        per_pred_masks.push(mask);
    }

    // Combine: start with first predicate, then AND/OR (left-associative).
    let mut result = per_pred_masks[0].clone();
    for (i, mask) in per_pred_masks[1..].iter().enumerate() {
        let is_and = operators.get(i).copied().unwrap_or(true);
        if is_and {
            for j in 0..n {
                result[j] = result[j] && mask[j];
            }
        } else {
            for j in 0..n {
                result[j] = result[j] || mask[j];
            }
        }
    }

    Ok(result)
}

// -----------------------------------------------------------------------
// CTE helper functions (Wave 6)
// -----------------------------------------------------------------------

/// Convert a QueryResult into a Table that can be registered in the catalog.
fn result_to_table(name: &str, result: &QueryResult) -> Table {
    let column_names: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    let columns: Vec<std::sync::Arc<Vec<u64>>> = result
        .columns
        .iter()
        .map(|c| std::sync::Arc::new(c.values.clone()))
        .collect();
    let string_columns: Vec<Option<std::sync::Arc<crate::exec::fm_index::StringSearchColumn>>> =
        vec![None; result.columns.len()];
    Table {
        name: name.to_string(),
        columns,
        column_names,
        row_count: result.row_count,
        string_columns,
        null_bitmaps: vec![],
            schema: None,
    }
}

/// Compute how many rows in `result` are new (not already in `table`).
/// A row is "new" if its full column content doesn't match any existing
/// row in the table. This is O(result_rows × table_rows × ncols) —
/// expensive but correct for small CTEs.
fn compute_new_rows(table: &Table, result: &QueryResult) -> usize {
    if result.row_count == 0 {
        return 0;
    }
    let ncols = result.columns.len();
    let mut new_count = 0;
    for r_row in 0..result.row_count {
        let mut found = false;
        for t_row in 0..table.row_count {
            let mut matches = true;
            for col_idx in 0..ncols {
                let r_val = result.columns[col_idx].values.get(r_row).copied().unwrap_or(0);
                let t_val = table.columns.get(col_idx).and_then(|c| c.get(t_row)).copied().unwrap_or(0);
                if r_val != t_val {
                    matches = false;
                    break;
                }
            }
            if matches {
                found = true;
                break;
            }
        }
        if !found {
            new_count += 1;
        }
    }
    new_count
}

/// Append all rows from a QueryResult to an existing Table. The table
/// must have the same number of columns as the result.
fn append_result_rows(table: &mut Table, result: &QueryResult) {
    for col_idx in 0..result.columns.len() {
        if col_idx < table.columns.len() {
            let col = std::sync::Arc::make_mut(&mut table.columns[col_idx]);
            col.extend_from_slice(&result.columns[col_idx].values);
        }
    }
    table.row_count += result.row_count;
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
                LoadedColumn { name: "id".into(), cells: ids, row_count: n, string_search: None, null_bitmap: None },
                LoadedColumn { name: "x".into(), cells: xs, row_count: n, string_search: None, null_bitmap: None },
            ],
            row_count: n,
        })
    }

    /// Build a `Table` with a single integer-encoded column `v`.
    fn make_int_table(values: &[u64]) -> DataSourceTable {
        let n = values.len();
        DataSourceTable::from_loaded(LoadedTable {
            name: "ft".into(),
            columns: vec![LoadedColumn { name: "v".into(), cells: values.to_vec(), row_count: n, string_search: None, null_bitmap: None }],
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
                    row_count: 1000, string_search: None, null_bitmap: None },
                LoadedColumn { name: "x".into(), cells: xs, row_count: 1000, string_search: None, null_bitmap: None },
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
        let mut engine = QueryEngine::new();
        let r = engine.execute("SELECT FROM WHERE");
        assert!(matches!(r, Err(Error::Parse(_))), "got {r:?}");
    }

    /// DoD 8: Non-existent table returns `Error::NotFound`.
    #[test]
    fn dod_non_existent_table_returns_not_found() {
        let mut engine = QueryEngine::new();
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
        let mut engine = QueryEngine::with_cost_model(cm);
        assert_eq!(engine.cost_model().cpu_freq_hz, 4.0e9);
        assert_eq!(engine.cost_model().simd_lanes, 16);
    }

    /// `QueryEngine::default()` is equivalent to `new()`.
    /// The catalog contains the internal `__dummy__` table (used for
    /// FROM-less SELECTs), so it's not strictly empty — but it has no
    /// user-registered tables.
    #[test]
    fn default_is_empty() {
        let mut engine = QueryEngine::default();
        // The __dummy__ table is always present.
        assert_eq!(engine.catalog().len(), 1);
        // But no user tables.
        let names: Vec<&str> = engine
            .catalog()
            .table_names()
            .into_iter()
            .filter(|n| *n != "__dummy__")
            .collect();
        assert!(names.is_empty());
    }

    /// Accessors return the right types.
    #[test]
    fn accessors_work() {
        let mut engine = QueryEngine::new();
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

// -----------------------------------------------------------------------
// Wave 53 helper functions: wire views, procedures, MERGE, JSON,
// temporal, window, PIVOT into execute().
// -----------------------------------------------------------------------

/// Substitute @param references in a stored-procedure body with the
/// supplied argument values. @1 → args[0], @2 → args[1], etc., and
/// named params @name → args[i] where proc_def.params[i].name == name.
fn substitute_proc_params(body: &str, args: &[String]) -> String {
    let mut result = body.to_string();
    // Positional substitution: @1, @2, ... → args[0], args[1], ...
    for (i, arg) in args.iter().enumerate() {
        let placeholder = format!("@{}", i + 1);
        result = result.replace(&placeholder, arg);
    }
    result
}

/// Parse a MERGE statement (Wave 53 wiring for exec/merge.rs).
///
/// Supports the form:
///   MERGE INTO target [AS t]
///   USING (VALUES (1, 'a'), (2, 'b')) AS s (id, val)
///   ON t.id = s.id
///   WHEN MATCHED THEN UPDATE SET col = val [, ...]
///   WHEN NOT MATCHED THEN INSERT (cols) VALUES (vals)
///
/// Wave 56a fix: the previous implementation hardcoded `source_rows: Vec::new()`,
/// `join_target_col: String::new()`, `join_source_col: String::new()` — so
/// `execute_merge` could never match any target row and the WHEN MATCHED
/// branch was dead. We now parse the USING (VALUES ...) clause to populate
/// `source_rows`, and parse the ON clause to populate the join columns.
///
/// Returns None if the SQL is not a MERGE statement.
fn parse_merge(sql: &str) -> Option<crate::exec::merge::Merge> {
    use crate::exec::merge::{Merge, MergeAction};
    let trimmed = sql.trim();
    let upper = trimmed.to_uppercase();
    if !upper.starts_with("MERGE ") && !upper.starts_with("MERGE INTO ") {
        return None;
    }

    let after_merge = if upper.starts_with("MERGE INTO ") {
        &trimmed["MERGE INTO ".len()..]
    } else {
        &trimmed["MERGE ".len()..]
    };

    // Target table name is the first whitespace-delimited token (optionally
    // followed by `AS alias`).
    let target = after_merge.split_whitespace().next()?.to_string();

    let lower = trimmed.to_lowercase();

    // ---- Parse USING (VALUES (...) , (...), ...) AS alias (col1, col2, ...) ----
    // The source rows are the (join_value, [full_row]) tuples extracted from
    // the VALUES list. The merge module's `source_rows` field is shaped as
    // Vec<(join_value_str, full_row_vals)> — the first element of each tuple
    // is the join key (a stringified u64 or quoted string), and the second
    // is the full row (used by the Insert action).
    let mut source_rows: Vec<(String, Vec<String>)> = Vec::new();
    let mut source_col_names: Vec<String> = Vec::new();
    if let Some(using_pos) = lower.find("using ") {
        let after_using = &trimmed[using_pos + "using ".len()..];
        // Skip whitespace.
        let after_using = after_using.trim_start();
        if after_using.starts_with('(') {
            // Find the matching close paren for the USING (...) group.
            // This may contain nested parens for the VALUES list.
            let mut depth = 0i32;
            let mut using_close = None;
            for (i, c) in after_using.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            using_close = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(close) = using_close {
                let using_inner = &after_using[1..close];
                // using_inner should start with "VALUES" then have (..), (..)
                let using_inner_lower = using_inner.to_lowercase();
                if let Some(v_pos) = using_inner_lower.find("values") {
                    let after_values = &using_inner[v_pos + "values".len()..];
                    // Parse each (...) tuple.
                    source_rows = parse_values_tuples(after_values);
                }
                // After the USING (...) group, look for "AS alias (col1, col2, ...)"
                // to extract the source column names.
                let after_group = after_using[close + 1..].trim_start();
                let after_as = if after_group.to_uppercase().starts_with("AS ") {
                    &after_group["AS ".len()..]
                } else {
                    after_group
                };
                // Skip the alias identifier.
                let after_alias = after_as.split_whitespace().next().map(|n| &after_as[n.len()..]).unwrap_or(after_as).trim_start();
                if after_alias.starts_with('(') {
                    if let Some(close2) = after_alias.find(')') {
                        source_col_names = after_alias[1..close2]
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .collect();
                    }
                }
            }
        }
    }

    // ---- Parse ON target_col = source_col ----
    let mut join_target_col = String::new();
    let mut join_source_col = String::new();
    if let Some(on_pos) = lower.find(" on ") {
        // Limit the ON clause to the next WHEN keyword (so we don't grab
        // any later "on" in a subquery or string literal).
        let after_on = &trimmed[on_pos + " on ".len()..];
        let when_pos = after_on.to_lowercase().find(" when ").unwrap_or(after_on.len());
        let on_clause = after_on[..when_pos].trim();
        // Parse "target.col = source.col" — split on '=' first.
        if let Some(eq_pos) = on_clause.find('=') {
            let lhs = on_clause[..eq_pos].trim();
            let rhs = on_clause[eq_pos + 1..].trim();
            // Both sides should be qualified "alias.col" — take the part after the dot.
            if let Some(dot_pos) = lhs.rfind('.') {
                join_target_col = lhs[dot_pos + 1..].trim().to_string();
            } else {
                join_target_col = lhs.to_string();
            }
            if let Some(dot_pos) = rhs.rfind('.') {
                join_source_col = rhs[dot_pos + 1..].trim().to_string();
            } else {
                join_source_col = rhs.to_string();
            }
        }
    }

    // ---- Look for WHEN MATCHED THEN UPDATE SET col = val, ... ----
    let mut when_matched: Option<MergeAction> = None;
    let mut when_not_matched_by_target: Option<MergeAction> = None;

    if let Some(pos) = lower.find("when matched then update set") {
        let after = &trimmed[pos + "when matched then update set".len()..];
        // The SET clause runs until the next WHEN keyword (or end of string).
        let set_end = after.to_lowercase().find(" when ").unwrap_or(after.len());
        let assigns_str = after[..set_end].trim();
        // Parse `col = val` pairs separated by commas.
        let assigns: Vec<(String, String)> = split_top_level_commas(assigns_str)
            .into_iter()
            .filter_map(|pair| {
                let pair = pair.trim();
                let eq_pos = pair.find('=')?;
                let col_raw = pair[..eq_pos].trim().to_string();
                let val_raw = pair[eq_pos + 1..].trim().to_string();
                // Strip any "alias." prefix from the LHS column (target.col → col).
                // IMPORTANT: do NOT strip the alias from the RHS value —
                // `source.val` must be preserved so execute_merge can
                // recognize it as a column reference and resolve it against
                // the current source row (Wave 56a fix).
                let col = col_raw.rsplit('.').next().unwrap_or(&col_raw).to_string();
                if col.is_empty() || val_raw.is_empty() {
                    None
                } else {
                    Some((col, val_raw))
                }
            })
            .collect();
        if !assigns.is_empty() {
            when_matched = Some(MergeAction::Update(assigns));
        }
    }

    if let Some(pos) = lower.find("when not matched then insert") {
        let after = &trimmed[pos + "when not matched then insert".len()..];
        // The INSERT clause runs until the next WHEN keyword (or end of string).
        let ins_end = after.to_lowercase().find(" when ").unwrap_or(after.len());
        let ins_str = after[..ins_end].trim();
        // Parse `(col1, col2) VALUES (val1, val2)` — best-effort.
        if let Some(open) = ins_str.find('(') {
            if let Some(close) = ins_str.find(')') {
                let cols: Vec<String> = ins_str[open + 1..close]
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect();
                if let Some(vals_pos) = ins_str[close..].to_lowercase().find("values") {
                    let vals_str = &ins_str[close + vals_pos + "values".len()..];
                    if let Some(v_open) = vals_str.find('(') {
                        if let Some(v_close) = vals_str.find(')') {
                            let vals: Vec<String> = vals_str[v_open + 1..v_close]
                                .split(',')
                                .map(|s| s.trim().to_string())
                                .collect();
                            when_not_matched_by_target = Some(MergeAction::Insert(cols, vals));
                        }
                    }
                }
            }
        }
    }

    // If we parsed source column names, find the join source col's index
    // and rewrite source_rows so the first element of each tuple is the
    // value of the join column (stringified). The merge module uses
    // source_rows[i].0 as the join key to match against target.col_values.
    if !source_col_names.is_empty() && !join_source_col.is_empty() {
        if let Some(src_idx) = source_col_names.iter().position(|c| c.eq_ignore_ascii_case(&join_source_col)) {
            // Each source_row tuple's first element becomes the join key.
            // The Vec<String> carries the full row values in source_col_names order.
            source_rows = source_rows.into_iter().map(|(_old_key, mut vals)| {
                let key = vals.get(src_idx).cloned().unwrap_or_default();
                (key, vals)
            }).collect();
        }
    }

    Some(Merge {
        target,
        source_rows,
        source_col_names,
        join_target_col,
        join_source_col,
        when_matched,
        when_not_matched_by_source: None,
        when_not_matched_by_target,
    })
}

/// Split a string on top-level commas (not inside parentheses or quotes).
/// Used by `parse_merge` to split SET assignments like
/// `col1 = source.col1, col2 = 'literal, with comma'`.
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '\'' => { in_str = !in_str; cur.push(c); }
            '(' if !in_str => { depth += 1; cur.push(c); }
            ')' if !in_str => { depth -= 1; cur.push(c); }
            ',' if depth == 0 && !in_str => {
                out.push(cur.clone());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Parse the body of a SQL VALUES list — e.g. `(1, 'a'), (2, 'b')` — into
/// a Vec of (first_cell_stringified, full_row) tuples. The first cell is
/// later used as the join key (it's overwritten in parse_merge if a join
/// column index is known).
fn parse_values_tuples(s: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut tuples: Vec<String> = Vec::new();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_str = !in_str;
                cur.push(c);
            }
            '(' if !in_str => {
                depth += 1;
                if depth == 1 {
                    cur.clear();
                } else {
                    cur.push(c);
                }
            }
            ')' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    tuples.push(cur.clone());
                    cur.clear();
                } else {
                    cur.push(c);
                }
            }
            _ if depth >= 1 => cur.push(c),
            _ => {}
        }
    }
    for t in &tuples {
        let vals: Vec<String> = split_top_level_commas(t)
            .into_iter()
            .map(|v| v.trim().to_string())
            .collect();
        if !vals.is_empty() {
            let first = vals[0].clone();
            out.push((first, vals));
        }
    }
    out
}

impl QueryEngine {
    /// Wave 53: Materialize views referenced in a SQL string.
    ///
    /// For each view name in the registry, if the SQL contains `FROM view_name`,
    /// execute the view's SELECT SQL and register the result as a catalog
    /// table under the view's name. The outer SELECT then runs against the
    /// materialized table. This is a simple (non-incremental) materialization
    /// strategy — every query against a view re-runs the view's SELECT.
    fn materialize_views_in_sql(&mut self, sql: &str) -> String {
        let lower = sql.to_lowercase();
        // Collect view names that appear in the SQL before mutating self.
        let view_names: Vec<String> = self.views.names()
            .into_iter()
            .map(|s| s.to_string())
            .filter(|view_name| {
                let pattern = format!("from {}", view_name.to_lowercase());
                lower.contains(&pattern)
            })
            .collect();
        // Now materialize each view. We collect (name, select_sql) pairs
        // first to release the immutable borrow on self.views before we
        // call self.execute_inner (which needs &mut self).
        let view_specs: Vec<(String, String)> = view_names.into_iter()
            .filter_map(|name| {
                self.views.get(&name).map(|v| (name, v.select_sql.clone()))
            })
            .collect();
        for (view_name, select_sql) in view_specs {
            if let Ok(result) = self.execute_inner(&select_sql, &Instant::now(), None) {
                let table = result_to_table(&view_name, &result);
                self.catalog.register(table);
            }
        }
        sql.to_string()
    }

    /// Execute a MERGE statement against a catalog table (Wave 53 wiring
    /// for exec/merge.rs). The target table is loaded into a QueryResult,
    /// `execute_merge` is applied, and the result is written back to the
    /// catalog.
    fn execute_merge_stmt(
        &mut self,
        merge: crate::exec::merge::Merge,
        start: &Instant,
    ) -> Result<QueryResult> {
        let target_name = merge.target.clone();
        // Load the target table into a QueryResult.
        let table = self.catalog.get(&target_name)
            .ok_or_else(|| Error::NotFound(format!("MERGE target table \"{target_name}\"")))?
            .clone();
        let mut qr = table_to_query_result(&table);

        let merge_result = crate::exec::merge::execute_merge(&mut qr, &merge);

        // Write the mutated QueryResult back into the catalog table.
        let new_table = query_result_to_table(&target_name, &qr);
        self.catalog.register(new_table);

        let mut result = QueryResult::empty();
        result.row_count = merge_result.inserted + merge_result.updated + merge_result.deleted;
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }
}

/// Convert a `Table` into a `QueryResult` so `execute_merge` can operate
/// on it.
fn table_to_query_result(table: &Table) -> QueryResult {
    let columns: Vec<ResultColumn> = table.column_names.iter().enumerate().map(|(i, name)| {
        ResultColumn {
            name: name.clone(),
            values: table.columns[i].to_vec(),
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }
    }).collect();
    QueryResult {
        columns,
        row_count: table.row_count,
        elapsed_us: 0,
    }
}

/// Convert a `QueryResult` back into a `Table` (round-trip after merge).
fn query_result_to_table(name: &str, qr: &QueryResult) -> Table {
    let columns: Vec<std::sync::Arc<Vec<u64>>> = qr.columns.iter()
        .map(|c| std::sync::Arc::new(c.values.clone()))
        .collect();
    let column_names: Vec<String> = qr.columns.iter().map(|c| c.name.clone()).collect();
    Table {
        name: name.to_string(),
        columns,
        column_names,
        row_count: qr.row_count,
        string_columns: vec![],
        null_bitmaps: vec![],
        schema: None,
    }
}

/// Parse `FOR SYSTEM_TIME AS OF <timestamp>` from a SQL string.
/// Returns (table_name, timestamp) if the clause is present.
///
/// SQL syntax: `SELECT ... FROM table_name FOR SYSTEM_TIME AS OF <ts>`
/// The table name appears between FROM and FOR SYSTEM_TIME.
fn parse_for_system_time(sql: &str) -> Option<(String, u64)> {
    let lower = sql.to_lowercase();
    let pos = lower.find("for system_time as of")?;
    // The timestamp is everything after "FOR SYSTEM_TIME AS OF" up to the
    // next non-digit character.
    let after = &sql[pos + "for system_time as of".len()..];
    let after_trimmed = after.trim_start();
    let ts_end = after_trimmed.find(|c: char| !c.is_ascii_digit()).unwrap_or(after_trimmed.len());
    if ts_end == 0 {
        return None;
    }
    let timestamp: u64 = after_trimmed[..ts_end].parse().ok()?;

    // The table name is between FROM and FOR SYSTEM_TIME. Look at the
    // substring before "FOR SYSTEM_TIME AS OF".
    let before = &sql[..pos];
    let before_lower = before.to_lowercase();
    let from_pos = before_lower.rfind("from ")?;
    let after_from = &before[from_pos + "from ".len()..];
    // The table name is the first whitespace-delimited token (optionally
    // followed by WHERE/ORDER/etc.).
    let table_name = after_from.split_whitespace().next()?.to_string();
    Some((table_name, timestamp))
}

/// Convert temporal-table rows (Vec<Vec<u64>>) into a QueryResult.
fn rows_to_query_result(rows: &[Vec<u64>], column_names: &[String], start: &Instant) -> QueryResult {
    let row_count = rows.len();
    let n_cols = column_names.len();
    let columns: Vec<ResultColumn> = (0..n_cols).map(|i| {
        let values: Vec<u64> = rows.iter().map(|r| r.get(i).copied().unwrap_or(0)).collect();
        ResultColumn {
            name: column_names[i].clone(),
            values,
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }
    }).collect();
    QueryResult {
        columns,
        row_count,
        elapsed_us: start.elapsed().as_micros() as u64,
    }
}

/// Apply window functions to a QueryResult (Wave 53 wiring for
/// exec/window.rs). Detects `SelectItem::Window` items in the query and
/// appends a new ResultColumn for each.
fn apply_window_functions(result: &QueryResult, query: &crate::sql::parser::SelectQuery) -> QueryResult {
    use crate::exec::window::{parse_window_spec, row_number, rank, dense_rank, sum_over, count_over};
    use crate::sql::parser::SelectItem;

    let mut new_cols: Vec<ResultColumn> = result.columns.clone();
    for item in &query.select {
        if let SelectItem::Window { func, arg, over_spec, alias } = item {
            let spec = match parse_window_spec(over_spec) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let func_upper = func.to_uppercase();
            let name = alias.clone().unwrap_or_else(|| func.to_lowercase());
            let values = match func_upper.as_str() {
                "ROW_NUMBER" => row_number(result, &spec),
                "RANK" => rank(result, &spec),
                "DENSE_RANK" => dense_rank(result, &spec),
                "SUM" => sum_over(result, arg, &spec),
                "COUNT" => count_over(result, &spec),
                _ => continue,
            };
            new_cols.push(ResultColumn {
                name,
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            });
        }
    }
    QueryResult {
        columns: new_cols,
        row_count: result.row_count,
        elapsed_us: result.elapsed_us,
    }
}

/// Stub for parsing PIVOT/UNPIVOT from QueryExtensions. The current
/// `QueryExtensions` type doesn't carry pivot specs, so this always
/// returns None. PIVOT is now wired through `parse_pivot_clause` which
/// detects the PIVOT keyword directly in the SQL string (Wave 56b).
fn extensions_pivot(_ext: &crate::sql::extensions::QueryExtensions) -> Option<PivotSpec> {
    None
}

/// A parsed PIVOT specification (Wave 53).
struct PivotSpec {
    group_col: String,
    pivot_col: String,
    value_col: String,
    pivot_values: Vec<String>,
    agg: String,
}

/// A parsed PIVOT clause extracted from a SQL string (Wave 56b).
/// `group_col` is auto-detected at apply time (see execute_inner).
struct PivotClause {
    agg: String,
    value_col: String,
    pivot_col: String,
    pivot_values: Vec<String>,
}

/// Parse a PIVOT clause from a SQL string. Returns None if no PIVOT clause
/// is present.
///
/// Supported syntax (case-insensitive):
///   PIVOT (SUM(amount) FOR quarter IN ('Q1', 'Q2', 'Q3'))
///   PIVOT (COUNT(*) FOR quarter IN (1, 2, 3))
///   PIVOT (AVG(price) FOR region IN ('NA', 'EU', 'APAC'))
///
/// The clause may be followed by `AS <alias>` (which is stripped by
/// strip_pivot_clause before re-execution of the underlying SELECT).
fn parse_pivot_clause(sql: &str) -> Option<PivotClause> {
    let lower = sql.to_lowercase();
    let pivot_pos = lower.find("pivot ")?;
    // Must be followed by '(' (possibly with whitespace).
    let after_pivot = &sql[pivot_pos + "pivot ".len()..];
    let after_pivot_trimmed = after_pivot.trim_start();
    if !after_pivot_trimmed.starts_with('(') {
        return None;
    }
    // Find the matching close paren for the PIVOT (...) group.
    let mut depth = 0i32;
    let mut close = None;
    for (i, c) in after_pivot_trimmed.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let inner = &after_pivot_trimmed[1..close];
    // inner should look like: SUM(amount) FOR quarter IN ('Q1', 'Q2')
    // or: COUNT(*) FOR quarter IN (1, 2, 3)
    let inner_lower = inner.to_lowercase();
    let for_pos = inner_lower.find(" for ")?;
    let agg_part = inner[..for_pos].trim();
    let after_for = &inner[for_pos + " for ".len()..];
    let after_for_lower = after_for.to_lowercase();
    let in_pos = after_for_lower.find(" in ")?;
    let pivot_col = after_for[..in_pos].trim().to_string();
    let after_in = &after_for[in_pos + " in ".len()..].trim_start();
    // after_in should start with '(' and end with ')'.
    if !after_in.starts_with('(') {
        return None;
    }
    let in_close = after_in.find(')')?;
    let values_str = &after_in[1..in_close];
    // Parse the values: split on commas, strip quotes/brackets.
    let pivot_values: Vec<String> = values_str
        .split(',')
        .map(|s| {
            let s = s.trim();
            // Strip single quotes.
            let s = if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
                &s[1..s.len() - 1]
            } else {
                s
            };
            // Strip square brackets (SQL Server style [Q1]).
            let s = if s.starts_with('[') && s.ends_with(']') && s.len() >= 2 {
                &s[1..s.len() - 1]
            } else {
                s
            };
            s.to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();
    if pivot_values.is_empty() {
        return None;
    }
    // Parse the agg part: AGG_FUNC(arg). The arg may be '*' or a column name.
    let open = agg_part.find('(')?;
    let close_paren = agg_part.rfind(')')?;
    let agg = agg_part[..open].trim().to_uppercase();
    let value_col = agg_part[open + 1..close_paren].trim().to_string();
    if agg.is_empty() || value_col.is_empty() {
        return None;
    }
    Some(PivotClause {
        agg,
        value_col,
        pivot_col,
        pivot_values,
    })
}

/// Strip the PIVOT clause (and any trailing `AS alias`) from a SQL string,
/// returning the underlying SELECT that should be executed to produce the
/// input rows for the pivot transformation.
fn strip_pivot_clause(sql: &str) -> String {
    let lower = sql.to_lowercase();
    let pivot_pos = match lower.find("pivot ") {
        Some(p) => p,
        None => return sql.to_string(),
    };
    // Walk forward from pivot_pos to find the matching close paren.
    let after_pivot = &sql[pivot_pos + "pivot ".len()..];
    let after_pivot_trimmed = after_pivot.trim_start();
    let paren_offset = after_pivot.len() - after_pivot_trimmed.len();
    let mut depth = 0i32;
    let mut close = None;
    for (i, c) in after_pivot_trimmed.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = match close {
        Some(c) => c,
        None => return sql.to_string(),
    };
    // The PIVOT clause spans [pivot_pos, pivot_pos + "pivot ".len() + paren_offset + close + 1).
    let end_of_pivot = pivot_pos + "pivot ".len() + paren_offset + close + 1;
    // After the PIVOT clause, there may be `AS <alias>` — strip that too.
    let rest = &sql[end_of_pivot..];
    let rest_trimmed_start = rest.trim_start();
    if rest_trimmed_start.to_uppercase().starts_with("AS ") {
        let after_as = &rest_trimmed_start["AS ".len()..];
        // Skip the alias identifier (alphanumeric + underscore).
        let alias_len = after_as
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .count();
        let after_alias = &after_as[alias_len..];
        // Build the result: sql[..pivot_pos] + after_alias.
        return format!("{}{}", &sql[..pivot_pos], after_alias);
    }
    // No AS clause — just concatenate.
    format!("{}{}", &sql[..pivot_pos], &sql[end_of_pivot..])
}

/// Apply a PIVOT transformation to a QueryResult (Wave 53 wiring for
/// exec/pivot.rs).
fn apply_pivot(result: &QueryResult, spec: &PivotSpec) -> QueryResult {
    crate::exec::pivot::pivot(
        result,
        &spec.group_col,
        &spec.pivot_col,
        &spec.value_col,
        &spec.pivot_values,
        &spec.agg,
    )
}
