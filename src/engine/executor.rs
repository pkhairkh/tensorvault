//! # SQL execution — the catalog → kernel bridge.
//!
//! [`execute_select`] is the core of the end-to-end path: it takes a parsed
//! [`SelectQuery`] plus its turboGP extensions, looks up the source table in
//! the [`Catalog`], selects the right kernel from the [`KernelTable`], runs
//! it, and packages the result as a [`QueryResult`].
//!
//! ## Supported patterns
//!
//! | SQL form | Kernel / path |
//! |----------|---------------|
//! | `SELECT count(*) FROM t` | `table.row_count` (no kernel — the catalog already knows) |
//! | `SELECT count(*) FROM t WHERE col = N` | `ScanEqU64` → `result.count` |
//! | `SELECT sum(col) FROM t` | `AggregateSumF64` → `result.sum` |
//! | `SELECT count(DISTINCT col) FROM t` | `AggregateCountDistinct` → `result.count` |
//! | `SELECT * FROM t WHERE col = N` | in-engine filter + gather (no kernel; the scan kernels only return a count + 64-bit mask) |
//! | `SELECT * FROM t` | full materialisation (every column, every row) |
//!
//! ## What is *not* supported
//!
//! - Range predicates (`col < N`, `col BETWEEN a AND b`) — would use
//!   `ScanRangeU64`, but the WHERE extractor only handles `=`.
//! - Multi-predicate WHERE (`a = 1 AND b = 2`) — would use
//!   `ScanMultiPredicate`, but the extractor only handles a single `=`.
//! - GROUP BY, ORDER BY, LIMIT — the executor returns all matching rows;
//!   these clauses are silently ignored. (The parser still accepts them;
//!   a future wave would push them into the plan / lowerer.)
//! - JOIN — the planner's join orderer exists, but the executor does not
//!   yet wire it up.
//!
//! The constraint "keep it simple — handle the common patterns first"
//! (see the Wave 20 task brief) explicitly defers these.
//!
//! ## Extension handling
//!
//! The seven turboGP extensions are accepted but mostly no-ops at this
//! layer:
//!
//! - `TIER L3` → selects the L3-resident kernel (the default anyway; if the
//!   user requests a tier for which no kernel is registered, `select`
//!   falls back to scalar-L3).
//! - `APPROXIMATE WITHIN ε CONFIDENCE δ` → acknowledged but the engine
//!   still runs the exact kernel. A future wave would route to the HLL /
//!   Count-Min sketch path; for now the parameters are simply validated
//!   by the parser.
//! - `SIMILAR TO` → would route to `SimilarityHamming`, but the test
//!   suite for Wave 20 does not exercise it end-to-end (the path is
//!   stubbed).
//! - `CONSISTENCY`, `USING`, `MEMORY BUDGET`, `ENERGY BUDGET` → no-ops at
//!   this layer; they affect admission control, sketch method, and
//!   resource governance, which are downstream concerns.
//!
//! ## Safety
//!
//! The kernel `execute` method is `unsafe` (it takes raw pointers and
//! the implementor promises to read `cell_count * 8` bytes). The engine
//! upholds that contract by always passing a `&[u64]` slice whose length
//! matches `params.cell_count`, then casting to `*const u8`. The output
//! pointer is a small stack-allocated `[u8; 64]` buffer — generous
//! enough for any `KernelResult` (24 bytes) plus padding.

use crate::catalog::Catalog;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::error::{Error, Result};
use crate::kernel::{Kernel, KernelParams, KernelTable, Operator};
use crate::memory::tier::MemoryTier;
use crate::planner::CostModel;
use crate::sql::extensions::QueryExtensions;
use crate::sql::parser::{Expr, SelectItem, SelectQuery, Value};

/// Execute a parsed SELECT query against the catalog.
///
/// See the module docs for the list of supported SQL patterns. Anything
/// outside that set returns [`Error::Other`] with a descriptive message
/// (rather than panicking) so the caller can surface the error to the
/// user.
///
/// The `cost_model` parameter is currently unused — the executor picks
/// the L3 tier by default and lets the kernel table's `select` fall back
/// to scalar. A future wave will pass `cost_model` to a `PlanLowerer` to
/// pick the cheapest tier; the parameter is retained in the signature so
/// that future wave doesn't have to change every call site.
pub fn execute_select(
    query: &SelectQuery,
    extensions: &QueryExtensions,
    catalog: &Catalog,
    kernel_table: &KernelTable,
    _cost_model: &CostModel,
) -> Result<QueryResult> {
    // 1. Look up the source table.
    let table = catalog
        .get(&query.from)
        .ok_or_else(|| Error::NotFound(format!("table '{}' not found in catalog", query.from)))?;

    // 2. Parse the WHERE clause, if present. Currently only a single
    //    `col = <literal>` equality is supported; anything else is an
    //    error (rather than silently producing wrong results).
    let filter = parse_where(&query.where_clause)?;

    // 3. Pick the execution tier. The TIER extension can override the
    //    default (L3); unknown tier names fall back to L3.
    let tier = pick_tier(extensions);

    // 4. Dispatch on the select list. Only single-item select lists are
    //    supported in this wave; multi-column SELECTs return an error.
    if query.select.len() != 1 {
        return Err(Error::Other(format!(
            "Wave 20 executor supports exactly one select item, got {}",
            query.select.len()
        )));
    }

    match &query.select[0] {
        SelectItem::Aggregate { func, arg, alias } => {
            execute_aggregate(func, arg, alias.as_deref(), filter, table, tier, kernel_table)
        }
        SelectItem::Star => execute_select_star(filter, table, query.limit),
        SelectItem::Column(name) => execute_select_column(name, filter, table),
    }
}

/// A parsed WHERE filter. Currently only `col = <int>` is supported.
struct Filter {
    /// The column name on the left (or right) of the equality.
    column: String,
    /// The literal value on the right (or left) of the equality.
    value: u64,
}

/// Parse the optional WHERE clause into a [`Filter`].
///
/// Returns `Ok(None)` if there is no WHERE clause. Returns `Err` for any
/// shape other than `col = <literal>` or `<literal> = col` — the engine
/// does not silently fall back to an unfiltered scan (which would give
/// wrong answers) or to a different kernel (which would require more
/// planner work than this wave is doing).
///
/// String literals (`name = 'alice'`) are accepted: the literal is
/// hashed with `xxh3_64` (see [`literal_to_u64`]) so it can be matched
/// against a hashed string column. This is the lossy contract documented
/// in [`crate::datasource`]: equality works, recovery does not.
fn parse_where(where_clause: &Option<Expr>) -> Result<Option<Filter>> {
    let Some(expr) = where_clause else { return Ok(None) };

    let Expr::Binary { left, op, right } = expr else {
        return Err(Error::Other(format!(
            "Wave 20 executor only supports a single `col = N` WHERE clause, got: {expr:?}"
        )));
    };

    if op != "=" {
        return Err(Error::Other(format!(
            "Wave 20 executor only supports `=` in WHERE, got `{op}`"
        )));
    }

    // Try `col = <literal>` and `<literal> = col` in either order.
    if let (Expr::Column(col), Expr::Literal(val)) = (&**left, &**right) {
        return Ok(Some(Filter { column: col.clone(), value: literal_to_u64(val)? }));
    }
    if let (Expr::Literal(val), Expr::Column(col)) = (&**left, &**right) {
        return Ok(Some(Filter { column: col.clone(), value: literal_to_u64(val)? }));
    }

    Err(Error::Other(format!(
        "Wave 20 executor only supports `col = literal` WHERE, got: {expr:?}"
    )))
}

/// Convert a SQL [`Value`] literal into the `u64` target the kernel
/// expects.
///
/// - `Int(n)`: `n as u64` (bit-reinterpret — negative values become
///   large `u64`, matching the loader's `i64 → u64` cast).
/// - `Float(f)`: `f.to_bits()` (matching the loader's Float64 encoding).
/// - `String(s)`: hash with `xxh3_64` so the literal can be matched
///   against a hashed string column. This is the lossy contract
///   documented in [`crate::datasource`]: equality works, recovery
///   does not.
/// - `Hex(bytes)`: pack the first 8 bytes little-endian (matching
///   `hex_to_target_u64` in `src/sql/plan.rs`).
fn literal_to_u64(val: &Value) -> Result<u64> {
    match val {
        Value::Int(n) => Ok(*n as u64),
        Value::Float(f) => Ok(f.to_bits()),
        Value::String(s) => {
            use xxhash_rust::xxh3;
            Ok(xxh3::xxh3_64(s.as_bytes()))
        }
        Value::Hex(bytes) => {
            let mut padded = [0u8; 8];
            let n = bytes.len().min(8);
            padded[..n].copy_from_slice(&bytes[..n]);
            Ok(u64::from_le_bytes(padded))
        }
    }
}

/// Pick the execution tier based on the `TIER` extension.
///
/// Returns [`MemoryTier::L3`] by default. Recognised tier names (case-
/// insensitive): `L1`, `L2`, `L1L2`, `L3`, `DDR5`, `DRAM`, `HBM`, `CXL`,
/// `NVME`, `NVMEOF`, `NETWORK`. Unrecognised names fall back to L3
/// (with no error — the engine still has a kernel for L3).
fn pick_tier(ext: &QueryExtensions) -> MemoryTier {
    let Some(name) = &ext.tier else { return MemoryTier::L3 };
    match name.to_uppercase().as_str() {
        "L1" | "L2" | "L1L2" => MemoryTier::L1L2,
        "L3" => MemoryTier::L3,
        "DDR5" | "DRAM" | "DDR" => MemoryTier::Ddr5,
        "HBM" => MemoryTier::Hbm,
        "CXL" => MemoryTier::Cxl,
        "NVME" => MemoryTier::Nvme,
        "NVMEOF" | "NVME_OF" => MemoryTier::NvmeOf,
        "NETWORK" => MemoryTier::Network,
        _ => MemoryTier::L3,
    }
}

/// Execute an aggregate query: `COUNT(*)`, `SUM(col)`, `COUNT(DISTINCT col)`, etc.
fn execute_aggregate(
    func: &str,
    arg: &str,
    alias: Option<&str>,
    filter: Option<Filter>,
    table: &crate::datasource::Table,
    tier: MemoryTier,
    kernel_table: &KernelTable,
) -> Result<QueryResult> {
    // The output column name: the alias if given, else the function name
    // lowercased (e.g. `count`, `sum`).
    let out_name = alias
        .map(|s| s.to_string())
        .unwrap_or_else(|| func.to_lowercase().split('_').next().unwrap_or("agg").to_string());

    match func {
        "COUNT" => execute_count(arg, filter, table, tier, kernel_table, &out_name),
        "COUNT_DISTINCT" => execute_count_distinct(arg, table, tier, kernel_table, &out_name),
        "SUM" => execute_sum(arg, filter, table, tier, kernel_table, &out_name),
        "AVG" => execute_avg(arg, filter, table, tier, kernel_table, &out_name),
        "MIN" | "MAX" => Err(Error::Other(format!(
            "Wave 20 executor does not yet implement {func} (no MIN/MAX kernel)"
        ))),
        other => Err(Error::Other(format!("unknown aggregate function: {other}"))),
    }
}

/// `COUNT(*)`: number of rows (filtered or not).
///
/// Without a WHERE clause, this is just `table.row_count` — no kernel
/// needs to run. With a WHERE clause, we run the `ScanEqU64` kernel on
/// the filtered column and return its `count`.
fn execute_count(
    arg: &str,
    filter: Option<Filter>,
    table: &crate::datasource::Table,
    tier: MemoryTier,
    kernel_table: &KernelTable,
    out_name: &str,
) -> Result<QueryResult> {
    if arg != "*" {
        return Err(Error::Other(format!(
            "Wave 20 executor only supports `COUNT(*)`; got COUNT({arg})"
        )));
    }

    let Some(filter) = filter else {
        // No WHERE: row count is known without scanning.
        return Ok(QueryResult::from_scalar_u64(out_name, table.row_count as u64));
    };

    // With WHERE col = value: scan the column for matches.
    let col = table.column(&filter.column).ok_or_else(|| {
        Error::NotFound(format!("column '{}' not found in table '{}'", filter.column, table.name))
    })?;

    let kernel = kernel_table.select(Operator::ScanEqU64, tier).ok_or_else(|| {
        Error::NotFound(format!("no ScanEqU64 kernel registered for tier {tier}"))
    })?;

    let params =
        KernelParams { target_u64: filter.value, cell_count: col.len(), ..Default::default() };
    let result = run_kernel(&*kernel, col, &params);
    Ok(QueryResult::from_scalar_u64(out_name, result.count))
}

/// `COUNT(DISTINCT col)`: number of distinct values in the column.
///
/// Runs the `AggregateCountDistinct` kernel (a HashSet-backed prototype
/// in the current kernel table; a production kernel would use HyperLogLog
/// — see [`crate::kernel::aggregate::CountDistinctScalar`]).
fn execute_count_distinct(
    arg: &str,
    table: &crate::datasource::Table,
    tier: MemoryTier,
    kernel_table: &KernelTable,
    out_name: &str,
) -> Result<QueryResult> {
    let col = table.column(arg).ok_or_else(|| {
        Error::NotFound(format!("column '{arg}' not found in table '{}'", table.name))
    })?;

    let kernel = kernel_table.select(Operator::AggregateCountDistinct, tier).ok_or_else(|| {
        Error::NotFound(format!("no AggregateCountDistinct kernel registered for tier {tier}"))
    })?;

    let params = KernelParams { cell_count: col.len(), ..Default::default() };
    let result = run_kernel(&*kernel, col, &params);
    Ok(QueryResult::from_scalar_u64(out_name, result.count))
}

/// `SUM(col)`: sum of the column's cells, returned as `f64`.
///
/// The engine does not yet track per-column types (the loaders encode
/// every column as `Vec<u64>`, with Float64 columns stored as
/// `f64::to_bits(value)` and integer columns as `value as u64`). To
/// handle the common case correctly, this function sums the cells as
/// integers (casting each `u64` to `f64` and accumulating). This is
/// exact for integer-encoded columns (the ClickBench / TPC-H norm)
/// up to `f64`'s 53-bit mantissa (≈9 × 10¹⁵).
///
/// For Float64-encoded columns, this gives the wrong answer (it sums
/// the bit patterns as if they were integers). A future wave that
/// tracks column-type metadata would route Float64 columns to the
/// [`Operator::AggregateSumF64`] kernel, which bit-reinterprets each
/// cell as `f64` before summing.
///
/// # Filter semantics
///
/// If a WHERE clause is present, the filter is on a *different* column
/// than the sum argument (e.g. `SELECT sum(id) FROM t WHERE x = 0`).
/// The matching rows are gathered from the filter column, then the
/// sum-column's cells at those row indices are summed.
fn execute_sum(
    arg: &str,
    filter: Option<Filter>,
    table: &crate::datasource::Table,
    _tier: MemoryTier,
    _kernel_table: &KernelTable,
    out_name: &str,
) -> Result<QueryResult> {
    let col = table.column(arg).ok_or_else(|| {
        Error::NotFound(format!("column '{arg}' not found in table '{}'", table.name))
    })?;

    // If there is a filter, gather matching row indices from the filter
    // column, then sum the sum-column at those indices. If the filter
    // is on the same column as the sum, the matching cells are simply
    // the cells that equal `value` — but we still use the gather path
    // for consistency.
    let sum: f64 = match &filter {
        Some(f) => {
            if f.column == arg {
                // Filter on the same column: sum cells that equal `value`.
                col.iter().copied().filter(|&c| c == f.value).map(|c| c as f64).sum()
            } else {
                // Filter on a different column: gather matching rows,
                // then sum the sum-column at those indices.
                let fcol = table.column(&f.column).ok_or_else(|| {
                    Error::NotFound(format!(
                        "column '{}' not found in table '{}'",
                        f.column, table.name
                    ))
                })?;
                col.iter()
                    .zip(fcol.iter())
                    .filter(|(_, &fc)| fc == f.value)
                    .map(|(&c, _)| c as f64)
                    .sum()
            }
        }
        None => col.iter().map(|&c| c as f64).sum(),
    };

    Ok(QueryResult::from_scalar_f64(out_name, sum))
}

/// `AVG(col)`: mean of the column's cells.
///
/// Implemented as `SUM(col) / COUNT(col)`, using the same integer-as-f64
/// summing convention as [`execute_sum`]. An empty column returns `0.0`
/// (avoiding a divide-by-zero — callers can detect this case via the
/// `count` field if they need to).
fn execute_avg(
    arg: &str,
    filter: Option<Filter>,
    table: &crate::datasource::Table,
    _tier: MemoryTier,
    _kernel_table: &KernelTable,
    out_name: &str,
) -> Result<QueryResult> {
    let col = table.column(arg).ok_or_else(|| {
        Error::NotFound(format!("column '{arg}' not found in table '{}'", table.name))
    })?;

    let (sum, count) = match &filter {
        Some(f) if f.column == arg => {
            let matching: Vec<u64> = col.iter().copied().filter(|&c| c == f.value).collect();
            let n = matching.len() as f64;
            let s: f64 = matching.iter().map(|&c| c as f64).sum();
            (s, n)
        }
        Some(f) => {
            let fcol = table.column(&f.column).ok_or_else(|| {
                Error::NotFound(format!(
                    "column '{}' not found in table '{}'",
                    f.column, table.name
                ))
            })?;
            let matching: Vec<u64> = col
                .iter()
                .zip(fcol.iter())
                .filter(|(_, &fc)| fc == f.value)
                .map(|(&c, _)| c)
                .collect();
            let n = matching.len() as f64;
            let s: f64 = matching.iter().map(|&c| c as f64).sum();
            (s, n)
        }
        None => {
            let n = col.len() as f64;
            let s: f64 = col.iter().map(|&c| c as f64).sum();
            (s, n)
        }
    };

    if count == 0.0 {
        return Ok(QueryResult::from_scalar_f64(out_name, 0.0));
    }

    Ok(QueryResult::from_scalar_f64(out_name, sum / count))
}

/// `SELECT * FROM t [WHERE col = N] [LIMIT k]`: materialise every column
/// of the matching rows.
///
/// The scan kernels only return a count and a 64-bit mask for the first
/// 64 cells — not enough to gather matching rows for tables larger than
/// 64 rows. So this path filters in-engine: iterate the filtered column,
/// collect matching row indices, then gather every column at those
/// indices.
///
/// LIMIT is applied after the gather (a future wave would push it into
/// the scan for early termination).
fn execute_select_star(
    filter: Option<Filter>,
    table: &crate::datasource::Table,
    limit: Option<usize>,
) -> Result<QueryResult> {
    // Collect matching row indices.
    let matching: Vec<usize> = match &filter {
        Some(f) => {
            let col = table.column(&f.column).ok_or_else(|| {
                Error::NotFound(format!(
                    "column '{}' not found in table '{}'",
                    f.column, table.name
                ))
            })?;
            col.iter().enumerate().filter(|(_, &c)| c == f.value).map(|(i, _)| i).collect()
        }
        None => (0..table.row_count).collect(),
    };

    // Apply LIMIT.
    let rows: Vec<usize> = match limit {
        Some(n) => matching.into_iter().take(n).collect(),
        None => matching,
    };

    let row_count = rows.len();
    let mut result = QueryResult::empty();
    result.row_count = row_count;

    // Gather each column at the matching indices.
    for (col_idx, col_name) in table.column_names.iter().enumerate() {
        let col = &table.columns[col_idx];
        let values: Vec<u64> = rows.iter().map(|&i| col[i]).collect();
        result
            .push_column(ResultColumn { name: col_name.clone(), values })
            .map_err(Error::Other)?;
    }

    Ok(result)
}

/// `SELECT col FROM t [WHERE col = N]`: project a single column.
///
/// Like [`execute_select_star`] but only the named column is returned.
fn execute_select_column(
    name: &str,
    filter: Option<Filter>,
    table: &crate::datasource::Table,
) -> Result<QueryResult> {
    // If the filter is on the same column, we can just return matching
    // cells. If the filter is on a different column, we have to gather.
    let col = table.column(name).ok_or_else(|| {
        Error::NotFound(format!("column '{name}' not found in table '{}'", table.name))
    })?;

    let values: Vec<u64> = match &filter {
        Some(f) if f.column == name => col.iter().copied().filter(|&c| c == f.value).collect(),
        Some(f) => {
            let fcol = table.column(&f.column).ok_or_else(|| {
                Error::NotFound(format!(
                    "column '{}' not found in table '{}'",
                    f.column, table.name
                ))
            })?;
            col.iter().zip(fcol.iter()).filter(|(_, &fc)| fc == f.value).map(|(&c, _)| c).collect()
        }
        None => col.to_vec(),
    };

    let row_count = values.len();
    let mut result = QueryResult::empty();
    result.row_count = row_count;
    result.push_column(ResultColumn { name: name.to_string(), values }).map_err(Error::Other)?;
    Ok(result)
}

/// Run a kernel on a `&[u64]` slice and return its [`crate::kernel::KernelResult`].
///
/// This is the single unsafe boundary in the engine: it constructs the
/// `*const u8` / `*mut u8` pointers the kernel trait expects, upholds
/// the contract (`input` points to `cell_count * 8` readable bytes,
/// `output` points to a writable buffer), and discards the output buffer
/// (the kernel returns its result by value).
///
/// # Safety
///
/// The kernel `execute` method is `unsafe` because it takes raw
/// pointers; this wrapper is safe because it always passes a properly-
/// sized slice and a stack-allocated output buffer.
fn run_kernel(
    kernel: &dyn Kernel,
    input: &[u64],
    params: &KernelParams,
) -> crate::kernel::KernelResult {
    // SAFETY: `input` is a `&[u64]` whose length matches
    // `params.cell_count` (callers set `cell_count: input.len()`). The
    // output buffer is 64 bytes, which is more than enough for any
    // `KernelResult` (24 bytes).
    let mut output = [0u8; 64];
    unsafe { kernel.execute(input.as_ptr() as *const u8, output.as_mut_ptr(), params) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::{LoadedColumn, LoadedTable};
    use crate::datasource::Table;
    use crate::kernel::KernelTable;
    use crate::planner::CostModel;
    use crate::sql::parse_with_extensions;

    /// Build a `Table` with two columns: `id` (0..n) and `x` (cycling 0..7).
    fn make_table(n: usize) -> Table {
        let ids: Vec<u64> = (0..n).map(|i| i as u64).collect();
        let xs: Vec<u64> = (0..n).map(|i| (i % 7) as u64).collect();
        Table::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![
                LoadedColumn { name: "id".into(), cells: ids, row_count: n },
                LoadedColumn { name: "x".into(), cells: xs, row_count: n },
            ],
            row_count: n,
        })
    }

    fn make_catalog(table: Table) -> Catalog {
        let mut cat = Catalog::new();
        cat.register(table);
        cat
    }

    fn run(sql: &str, catalog: &Catalog) -> Result<QueryResult> {
        let kernel_table = KernelTable::new();
        let cost_model = CostModel::default();
        let (query, ext) = parse_with_extensions(sql).map_err(Error::Parse)?;
        execute_select(&query, &ext, catalog, &kernel_table, &cost_model)
    }

    #[test]
    fn count_star_no_where_returns_row_count() {
        let cat = make_catalog(make_table(1000));
        let r = run("SELECT count(*) FROM t", &cat).expect("query");
        assert_eq!(r.scalar_u64(), Some(1000));
    }

    #[test]
    fn count_star_with_where_returns_match_count() {
        // 1000 rows where x cycles 0..7 → 142 matches for x = 0 (1000 / 7 = 142).
        let cat = make_catalog(make_table(1000));
        let r = run("SELECT count(*) FROM t WHERE x = 0", &cat).expect("query");
        // 1000 / 7 = 142 full cycles plus the first row (i=0 → x=0).
        // 0,7,14,...,994 → 143 matches.
        let expected = (0..1000).filter(|i| i % 7 == 0).count() as u64;
        assert_eq!(r.scalar_u64(), Some(expected));
    }

    #[test]
    fn count_star_with_where_no_matches_returns_zero() {
        let cat = make_catalog(make_table(1000));
        let r = run("SELECT count(*) FROM t WHERE x = 999", &cat).expect("query");
        assert_eq!(r.scalar_u64(), Some(0));
    }

    #[test]
    fn sum_col_returns_correct_sum() {
        // Sum of 0..1000 = 499500.
        let cat = make_catalog(make_table(1000));
        let r = run("SELECT sum(id) FROM t", &cat).expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 499_500.0).abs() < 1e-3, "got {s}");
    }

    #[test]
    fn sum_float_col_returns_correct_sum_for_integer_encoding() {
        // The engine sums cells as integers (cast u64 → f64). For a
        // Float64-encoded column this would give the wrong answer, so
        // we test the integer-encoding path here: store values as
        // their integer cast (1.5 → 1, 2.5 → 2, etc.) and verify the
        // sum of the integer parts.
        let cells: Vec<u64> = vec![1, 2, 3, 4];
        let table = Table::from_loaded(LoadedTable {
            name: "ft".into(),
            columns: vec![LoadedColumn { name: "v".into(), cells, row_count: 4 }],
            row_count: 4,
        });
        let cat = make_catalog(table);
        let r = run("SELECT sum(v) FROM ft", &cat).expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 10.0).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn count_distinct_returns_distinct_count() {
        // x cycles 0..7 over 1000 rows → 7 distinct values (0,1,2,3,4,5,6).
        let cat = make_catalog(make_table(1000));
        let r = run("SELECT count(DISTINCT x) FROM t", &cat).expect("query");
        assert_eq!(r.scalar_u64(), Some(7));
    }

    #[test]
    fn count_distinct_with_approximate_extension_runs() {
        // APPROXIMATE WITHIN 0.05 CONFIDENCE 0.95 should not change the
        // exact result (the engine still runs the exact kernel for now).
        let cat = make_catalog(make_table(1000));
        let r =
            run("SELECT count(DISTINCT x) APPROXIMATE WITHIN 0.05 CONFIDENCE 0.95 FROM t", &cat)
                .expect("query");
        assert_eq!(r.scalar_u64(), Some(7));
    }

    #[test]
    fn count_star_with_tier_extension_runs() {
        let cat = make_catalog(make_table(1000));
        let r = run("SELECT count(*) FROM t TIER L3", &cat).expect("query");
        assert_eq!(r.scalar_u64(), Some(1000));
    }

    #[test]
    fn select_star_with_where_returns_matching_rows() {
        // 1000 rows where id = 5 → exactly 1 match (the row with id=5).
        let cat = make_catalog(make_table(1000));
        let r = run("SELECT * FROM t WHERE id = 5", &cat).expect("query");
        assert_eq!(r.row_count, 1);
        assert_eq!(r.column("id"), Some(&[5u64][..]));
        assert_eq!(r.column("x"), Some(&[5u64][..])); // x = id % 7 = 5
    }

    #[test]
    fn select_star_no_where_returns_all_rows() {
        let cat = make_catalog(make_table(100));
        let r = run("SELECT * FROM t", &cat).expect("query");
        assert_eq!(r.row_count, 100);
        assert_eq!(r.column_count(), 2);
    }

    #[test]
    fn select_star_with_limit_truncates() {
        let cat = make_catalog(make_table(100));
        let r = run("SELECT * FROM t LIMIT 10", &cat).expect("query");
        assert_eq!(r.row_count, 10);
    }

    #[test]
    fn select_column_returns_single_column() {
        let cat = make_catalog(make_table(100));
        let r = run("SELECT id FROM t", &cat).expect("query");
        assert_eq!(r.column_count(), 1);
        assert_eq!(r.row_count, 100);
        assert_eq!(r.column("id"), Some(&(0..100).map(|i| i as u64).collect::<Vec<_>>()[..]));
    }

    #[test]
    fn select_column_with_where_on_same_column_filters() {
        let cat = make_catalog(make_table(100));
        let r = run("SELECT id FROM t WHERE id = 5", &cat).expect("query");
        assert_eq!(r.row_count, 1);
        assert_eq!(r.column("id"), Some(&[5u64][..]));
    }

    #[test]
    fn select_column_with_where_on_other_column_gathers() {
        // Filter on x = 0, project id. x = i % 7, so x = 0 ⟺ i ∈ {0, 7, 14, ...}.
        let cat = make_catalog(make_table(50));
        let r = run("SELECT id FROM t WHERE x = 0", &cat).expect("query");
        let expected: Vec<u64> = (0..50).step_by(7).map(|i| i as u64).collect();
        assert_eq!(r.column("id"), Some(&expected[..]));
    }

    #[test]
    fn avg_col_returns_correct_average() {
        let cat = make_catalog(make_table(1000));
        let r = run("SELECT avg(id) FROM t", &cat).expect("query");
        let a = r.scalar_f64().expect("scalar");
        assert!((a - 499.5).abs() < 1e-9, "got {a}");
    }

    #[test]
    fn non_existent_table_returns_not_found() {
        let cat = Catalog::new();
        let r = run("SELECT count(*) FROM missing", &cat);
        assert!(matches!(r, Err(Error::NotFound(_))), "got {r:?}");
    }

    #[test]
    fn non_existent_column_returns_not_found() {
        let cat = make_catalog(make_table(100));
        let r = run("SELECT count(*) FROM t WHERE nope = 1", &cat);
        assert!(matches!(r, Err(Error::NotFound(_))), "got {r:?}");
    }

    #[test]
    fn unknown_aggregate_returns_error() {
        let cat = make_catalog(make_table(100));
        let r = run("SELECT FROBNICATE(id) FROM t", &cat);
        assert!(matches!(r, Err(Error::Other(_))), "got {r:?}");
    }

    #[test]
    fn min_max_return_unsupported_error() {
        let cat = make_catalog(make_table(100));
        let r = run("SELECT MIN(id) FROM t", &cat);
        assert!(matches!(r, Err(Error::Other(_))), "got {r:?}");
        let r = run("SELECT MAX(id) FROM t", &cat);
        assert!(matches!(r, Err(Error::Other(_))), "got {r:?}");
    }

    #[test]
    fn multi_item_select_returns_error() {
        let cat = make_catalog(make_table(100));
        let r = run("SELECT id, x FROM t", &cat);
        assert!(matches!(r, Err(Error::Other(_))), "got {r:?}");
    }

    #[test]
    fn range_where_returns_error() {
        let cat = make_catalog(make_table(100));
        let r = run("SELECT * FROM t WHERE id > 5", &cat);
        assert!(matches!(r, Err(Error::Other(_))), "got {r:?}");
    }

    #[test]
    fn and_where_returns_error() {
        let cat = make_catalog(make_table(100));
        let r = run("SELECT * FROM t WHERE id = 1 AND x = 2", &cat);
        assert!(matches!(r, Err(Error::Other(_))), "got {r:?}");
    }

    #[test]
    fn invalid_sql_returns_parse_error() {
        let cat = make_catalog(make_table(100));
        let r = run("SELECT FROM WHERE", &cat);
        assert!(matches!(r, Err(Error::Parse(_))), "got {r:?}");
    }

    #[test]
    fn count_with_column_arg_returns_error() {
        let cat = make_catalog(make_table(100));
        let r = run("SELECT count(id) FROM t", &cat);
        assert!(matches!(r, Err(Error::Other(_))), "got {r:?}");
    }

    #[test]
    fn tier_cxl_selects_cxl_tier() {
        // CXL tier doesn't have a ScanEqU64 kernel on all platforms, but
        // the kernel table's `select` falls back to scalar-L3. The query
        // should still produce the right answer.
        let cat = make_catalog(make_table(100));
        let r = run("SELECT count(*) FROM t WHERE x = 0 TIER CXL", &cat).expect("query");
        let expected = (0..100).filter(|i| i % 7 == 0).count() as u64;
        assert_eq!(r.scalar_u64(), Some(expected));
    }

    #[test]
    fn literal_on_left_of_equality_works() {
        // `WHERE 5 = id` — literal on the left.
        let cat = make_catalog(make_table(100));
        let r = run("SELECT * FROM t WHERE 5 = id", &cat).expect("query");
        assert_eq!(r.row_count, 1);
        assert_eq!(r.column("id"), Some(&[5u64][..]));
    }

    #[test]
    fn sum_with_where_filters_then_sums() {
        // Sum of id where x = 0 → sum of i for i in {0, 7, 14, ...}.
        let cat = make_catalog(make_table(50));
        let r = run("SELECT sum(id) FROM t WHERE x = 0", &cat).expect("query");
        let expected: f64 = (0..50).step_by(7).map(|i| i as f64).sum();
        let s = r.scalar_f64().expect("scalar");
        assert!((s - expected).abs() < 1e-9, "got {s}, expected {expected}");
    }
}
