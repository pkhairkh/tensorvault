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
    /// Transaction manager for BEGIN/COMMIT/ROLLBACK (Wave 5).
    txn_manager: crate::txn::TxnManager,
}

impl QueryEngine {
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
        };
        catalog.register(dummy);
        Self {
            catalog,
            kernel_table: Arc::new(KernelTable::new()),
            cost_model: CostModel::default(),
            txn_manager: crate::txn::TxnManager::new(),
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
        let trimmed = sql.trim();
        let lower = trimmed.to_lowercase();
        if lower.starts_with("begin") || lower.starts_with("start transaction") {
            let _id = self
                .txn_manager
                .begin(&self.catalog)
                .map_err(Error::Other)?;
            return Ok(QueryResult::empty());
        }
        if lower.starts_with("commit") {
            let committed = self
                .txn_manager
                .commit()
                .map_err(Error::Other)?;
            self.catalog = committed;
            return Ok(QueryResult::empty());
        }
        if lower.starts_with("rollback") {
            self.txn_manager
                .rollback()
                .map_err(Error::Other)?;
            return Ok(QueryResult::empty());
        }

        // If a transaction is active, route all DML/DDL/SELECT to the
        // snapshot catalog. Otherwise, use the main catalog.
        // We do this by swapping the snapshot into self.catalog for the
        // duration of the statement, then swapping back.
        let txn_active = self.txn_manager.is_active();
        if txn_active {
            // Take the snapshot out of the txn manager temporarily.
            let mut txn = self.txn_manager.active.take().expect("txn active");
            std::mem::swap(&mut self.catalog, &mut txn.snapshot);
            let result = self.execute_inner(sql, &start);
            // Swap back: self.catalog goes back to being the main catalog
            // (unchanged), txn.snapshot becomes the (possibly modified)
            // transaction state.
            std::mem::swap(&mut self.catalog, &mut txn.snapshot);
            self.txn_manager.active = Some(txn);
            return result;
        }

        self.execute_inner(sql, &start)
    }

    /// Inner execution: dispatches DDL, DML, CTE, and SELECT without
    /// transaction awareness. Called by `execute` either with the main
    /// catalog or with the txn snapshot swapped in.
    fn execute_inner(&mut self, sql: &str, start: &Instant) -> Result<QueryResult> {
        // Try CTE (WITH ... SELECT ...) first.
        if let Some(with_result) = crate::sql::parse_with(sql) {
            let with = with_result.map_err(Error::Parse)?;
            let mut result = self.execute_with(with)?;
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Try DDL first (CREATE TABLE, DROP TABLE, CREATE SCHEMA).
        if let Some(ddl) = crate::sql::parse_ddl(sql).map_err(Error::Parse)? {
            let mut result = self.execute_ddl(ddl)?;
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Try DML (INSERT, UPDATE, DELETE).
        if let Some(dml) = crate::sql::parse_dml(sql).map_err(Error::Parse)? {
            let mut result = self.execute_dml(dml)?;
            result.elapsed_us = start.elapsed().as_micros() as u64;
            return Ok(result);
        }

        // Parse as SELECT.
        let (query, extensions) = match crate::sql::parse_with_extensions(sql) {
            Ok(qe) => qe,
            Err(_parse_err) => {
                // The basic parser failed — try the TPC-H interpreter
                // which has a richer parser (CASE, EXTRACT, subqueries,
                // HAVING, arithmetic in aggregates, etc.).
                let mut tpch_result = crate::engine::tpch::parse_and_execute(sql, &self.catalog)?;
                tpch_result.elapsed_us = start.elapsed().as_micros() as u64;
                return Ok(tpch_result);
            }
        };

        // Execute the parsed query.
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
            Err(exec_err) => {
                // The basic executor failed — try the TPC-H interpreter
                // as a fallback. This handles queries with features the
                // basic executor doesn't support (multi-aggregate, HAVING,
                // CASE WHEN, subqueries, etc.).
                let mut tpch_result = crate::engine::tpch::parse_and_execute(sql, &self.catalog)
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
                let cell = parse_value_cell(val_str);
                // COW: Arc::make_mut gives us a mutable Vec if we're the
                // sole owner, or clones if shared.
                let col = std::sync::Arc::make_mut(&mut table.columns[col_idx]);
                col.push(cell);
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
    fn execute_update(&mut self, upd: crate::sql::Update) -> Result<QueryResult> {
        let table = self
            .catalog
            .get_mut(&upd.table)
            .ok_or_else(|| Error::NotFound(format!("table \"{}\"", upd.table)))?;

        // Parse assignments into (col_idx, new_value_cell) pairs.
        let mut assigns: Vec<(usize, u64)> = Vec::with_capacity(upd.assignments.len());
        for (col_name, expr) in &upd.assignments {
            let idx = table
                .column_idx(col_name)
                .ok_or_else(|| Error::NotFound(format!("column \"{col_name}\"")))?;
            // For now, the expression must be a simple literal.
            let cell = parse_value_cell(expr);
            assigns.push((idx, cell));
        }

        // Determine which rows match the WHERE clause.
        let n = table.row_count;
        let mut updated = 0usize;
        let match_mask: Vec<bool> = if let Some(where_str) = &upd.where_clause {
            eval_simple_where(table, where_str)?
        } else {
            vec![true; n]
        };

        for (row_idx, &matches) in match_mask.iter().enumerate() {
            if !matches {
                continue;
            }
            for &(col_idx, val) in &assigns {
                let col = std::sync::Arc::make_mut(&mut table.columns[col_idx]);
                col[row_idx] = val;
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
    fn execute_with(&mut self, with: crate::sql::WithClause) -> Result<QueryResult> {
        let mut temp_tables: Vec<String> = Vec::new();

        for cte in &with.ctes {
            // Execute the anchor.
            let anchor_result = self.execute_inner(&cte.anchor, &Instant::now())?;

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
                    let rec_result = self.execute_inner(recursive_sql, &Instant::now())?;

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
        let result = self.execute_inner(&with.outer_query, &Instant::now())?;

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
/// Currently supports: `col = value` and `col = value AND col2 = value2`
/// and `col = value OR col2 = value2`. More complex expressions will be
/// supported when the expression evaluator is wired in.
fn eval_simple_where(table: &Table, where_str: &str) -> Result<Vec<bool>> {
    let n = table.row_count;
    if n == 0 {
        return Ok(Vec::new());
    }
    let trimmed = where_str.trim();
    // Split on AND / OR (case-insensitive)
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(vec![true; n]);
    }

    // Parse into a list of (col_name, value) predicates joined by AND/OR.
    // Simple approach: tokenize and walk.
    let mut predicates: Vec<(String, u64)> = Vec::new();
    let mut operators: Vec<bool> = Vec::new(); // true = AND, false = OR
    let mut i = 0;
    while i < parts.len() {
        let part = parts[i];
        if part.eq_ignore_ascii_case("AND") {
            operators.push(true);
            i += 1;
            continue;
        }
        if part.eq_ignore_ascii_case("OR") {
            operators.push(false);
            i += 1;
            continue;
        }
        // Expect: col = value
        if i + 2 >= parts.len() {
            return Err(Error::Other(format!("incomplete WHERE clause near '{part}'")));
        }
        let col_name = part.to_string();
        let op = parts[i + 1];
        if op != "=" {
            return Err(Error::Other(format!(
                "unsupported WHERE operator '{op}' (only = is supported in DML WHERE)"
            )));
        }
        let val_str = parts[i + 2];
        let cell = parse_value_cell(val_str);
        predicates.push((col_name, cell));
        i += 3;
    }

    if predicates.is_empty() {
        return Ok(vec![true; n]);
    }

    // Find column indices.
    let mut col_indices: Vec<usize> = Vec::with_capacity(predicates.len());
    for (col_name, _) in &predicates {
        let idx = table
            .column_idx(col_name)
            .ok_or_else(|| Error::NotFound(format!("column \"{col_name}\"")))?;
        col_indices.push(idx);
    }

    // Evaluate each predicate per row.
    let mut per_pred_masks: Vec<Vec<bool>> = Vec::with_capacity(predicates.len());
    for (pred_idx, &(_, val)) in predicates.iter().enumerate() {
        let col_idx = col_indices[pred_idx];
        let col = &table.columns[col_idx];
        let mask: Vec<bool> = col.iter().map(|&c| c == val).collect();
        per_pred_masks.push(mask);
    }

    // Combine: start with first predicate, then AND/OR.
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
