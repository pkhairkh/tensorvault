//! SQL execution engine — expanded with GROUP BY, JOIN, range WHERE, AND/OR, ORDER BY, LIMIT.
//!
//! Supported query shapes:
//! - `SELECT count(*) FROM t`
//! - `SELECT count(*) FROM t WHERE col = N`
//! - `SELECT count(*) FROM t WHERE col < N` (range)
//! - `SELECT count(*) FROM t WHERE col = N AND col2 = M` (AND)
//! - `SELECT sum(col) FROM t`
//! - `SELECT sum(col) FROM t WHERE col < N`
//! - `SELECT avg(col) FROM t`
//! - `SELECT min(col) FROM t`
//! - `SELECT max(col) FROM t`
//! - `SELECT count(DISTINCT col) FROM t`
//! - `SELECT col1, sum(col2) FROM t GROUP BY col1`
//! - `SELECT col1, count(*) FROM t GROUP BY col1 ORDER BY col1`
//! - `SELECT * FROM t WHERE col = N LIMIT 10`
//! - `SELECT col1, col2 FROM t WHERE col1 < N ORDER BY col2 DESC LIMIT 5`
//! - `SELECT count(*) FROM t1, t2 WHERE t1.id = t2.id` (cross join + filter)
//! - `SELECT count(*) FROM t1 JOIN t2 ON t1.id = t2.id` (inner join)

use crate::catalog::Catalog;
use crate::datasource::table::Table;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::kernel::{KernelParams, KernelTable, Operator};
use crate::memory::tier::MemoryTier;
use crate::sql::parser::{Expr, SelectItem, SelectQuery, Value};
use crate::sql::extensions::QueryExtensions;
use crate::Error;
use crate::engine::dispatch;
use std::collections::HashMap;

type Result<T> = std::result::Result<T, Error>;

/// Execute a parsed SELECT query against the catalog.
pub fn execute_select(
    query: &SelectQuery,
    extensions: &QueryExtensions,
    catalog: &Catalog,
    kernel_table: &KernelTable,
    cost_model: &crate::planner::CostModel,
) -> Result<QueryResult> {
    // 1. Resolve the table(s)
    let table = catalog
        .get(&query.from)
        .ok_or_else(|| Error::NotFound(format!("table '{}'", query.from)))?;

    // 0. Consult the cost-based optimizer to choose an execution strategy.
    let row_count = table.row_count as u64;
    let has_where = query.where_clause.is_some();
    let has_group_by = !query.group_by.is_empty();
    let has_join = !query.joins.is_empty();
    let plan = crate::planner::optimizer::choose_plan(
        cost_model,
        row_count,
        has_where,
        has_group_by,
        has_join,
        false, // subquery detection is handled by the tpch fallback
        query.select.len(),
    );
    log::debug!(
        "execute_select: table='{}' rows={} strategy={:?} est_cost={:.1}us est_rows={}",
        query.from, row_count, plan.strategy, plan.estimated_cost_us, plan.estimated_rows
    );

    // JOIN support: materialize joined table, then dispatch on it.
    if !query.joins.is_empty() || plan.strategy == crate::planner::optimizer::ExecStrategy::HashJoin {
        return execute_with_join(query, extensions, catalog, kernel_table);
    }

    // If the optimizer says TpchFallback, return an error so the caller
    // (execute_inner) routes to the tpch interpreter.
    if plan.strategy == crate::planner::optimizer::ExecStrategy::TpchFallback {
        return Err(Error::Other("optimizer chose tpch fallback".into()));
    }

    // Try kernel-direct dispatch first (10-30x faster than per-row evaluation).
    // Only attempt if the optimizer recommends KernelDirect or doesn't object.
    if plan.strategy == crate::planner::optimizer::ExecStrategy::KernelDirect
        || plan.strategy == crate::planner::optimizer::ExecStrategy::Vectorized
    {
        if let Some(result) = dispatch::execute_dispatched(query, table) {
            return result;
        }
    }

    // 2. Parse the WHERE clause
    let filter = parse_where(&query.where_clause, table)?;

    // 3. Pick the memory tier
    let tier = pick_tier(extensions);

    // 4. Execute based on select-list shape
    let result = if !query.group_by.is_empty() {
        // GROUP BY query
        execute_group_by(query, &filter, table, tier, kernel_table)?
    } else if query.select.len() == 1 {
        match &query.select[0] {
            SelectItem::Aggregate { func, arg, alias } => {
                execute_aggregate(func, arg, alias.as_deref(), &filter, table, tier, kernel_table)?
            }
            SelectItem::Star => execute_select_star(&filter, table, query.limit)?,
            SelectItem::Column(name) => execute_select_column(name, &filter, table, query.limit)?,
            // `SELECT <int>` — emit a single-row, single-column literal.
            SelectItem::Literal(v) => {
                QueryResult {
                    columns: vec![ResultColumn {
                        name: v.to_string(),
                        values: vec![*v],
                        string_values: None,
                    }],
                    row_count: 1,
                    elapsed_us: 0,
                }
            }
            // Window functions are handled by the tpch fallback.
            SelectItem::Window { .. } => {
                return Err(Error::Other("window function in execute_select — should use tpch fallback".into()));
            }
        }
    } else if query.select.len() > 1 {
        // Multi-column select (could be columns or column+aggregate without GROUP BY)
        let has_agg = query.select.iter().any(|s| matches!(s, SelectItem::Aggregate { .. }));
        if has_agg {
            // Treat as implicit GROUP BY (aggregate without group = single row)
            execute_aggregate_no_group(&query.select, &filter, table, tier, kernel_table)?
        } else {
            execute_select_multi(&query.select, &filter, table, query.order_by.as_slice(), query.limit)?
        }
    } else {
        return Err(Error::Other("empty SELECT list".into()));
    };

    // 5. Apply ORDER BY if needed (for non-group-by queries)
    let result = if !query.order_by.is_empty() && query.group_by.is_empty() {
        apply_order_by(result, &query.order_by, table)?
    } else {
        result
    };

    Ok(result)
}

// ---------------------------------------------------------------------------
// WHERE clause parsing — now supports =, <, >, <=, >=, !=, AND, OR
// ---------------------------------------------------------------------------

/// A compiled WHERE filter.
#[derive(Debug, Clone)]
pub struct Filter {
    /// Column index in the table.
    pub col_idx: usize,
    /// Comparison operator.
    pub op: String,
    /// Comparison value.
    pub value: u64,
}

/// A compiled WHERE clause — can be a single filter or AND/OR of multiple.
#[derive(Debug, Clone)]
pub enum WhereClause {
    /// No WHERE clause.
    None,
    /// A single predicate.
    Single(Filter),
    /// AND of two clauses.
    And(Box<WhereClause>, Box<WhereClause>),
    /// OR of two clauses.
    Or(Box<WhereClause>, Box<WhereClause>),
}

/// Parse the optional WHERE clause.
#[allow(clippy::only_used_in_recursion)]
fn parse_where(where_clause: &Option<Expr>, table: &Table) -> Result<WhereClause> {
    let Some(expr) = where_clause else {
        return Ok(WhereClause::None);
    };
    parse_expr(expr, table)
}

fn parse_expr(expr: &Expr, table: &Table) -> Result<WhereClause> {
    match expr {
        Expr::Binary { left, op, right } => {
            let op_upper = op.to_uppercase();
            match op_upper.as_str() {
                "AND" => {
                    let l = parse_expr(left, table)?;
                    let r = parse_expr(right, table)?;
                    Ok(WhereClause::And(Box::new(l), Box::new(r)))
                }
                "OR" => {
                    let l = parse_expr(left, table)?;
                    let r = parse_expr(right, table)?;
                    Ok(WhereClause::Or(Box::new(l), Box::new(r)))
                }
                "=" | "!=" | "<" | ">" | "<=" | ">=" => {
                    let (col, val) = extract_col_and_value(left, right, table)?;
                    Ok(WhereClause::Single(Filter {
                        col_idx: col,
                        op: op_upper,
                        value: val,
                    }))
                }
                _ => Err(Error::Other(format!("unsupported operator in WHERE: {}", op))),
            }
        }
        _ => Err(Error::Other(format!("unsupported WHERE expression: {:?}", expr))),
    }
}

fn extract_col_and_value(left: &Expr, right: &Expr, table: &Table) -> Result<(usize, u64)> {
    // Try left=column, right=literal
    if let Expr::Column(name) = left {
        if let Expr::Literal(val) = right {
            let idx = table.column_idx(name)
                .ok_or_else(|| Error::NotFound(format!("column '{}'", name)))?;
            return Ok((idx, literal_to_u64(val)?));
        }
    }
    // Try right=column, left=literal
    if let Expr::Column(name) = right {
        if let Expr::Literal(val) = left {
            let idx = table.column_idx(name)
                .ok_or_else(|| Error::NotFound(format!("column '{}'", name)))?;
            return Ok((idx, literal_to_u64(val)?));
        }
    }
    Err(Error::Other(format!("WHERE clause must be col OP literal, got: {:?} OP {:?}", left, right)))
}

fn literal_to_u64(val: &Value) -> Result<u64> {
    match val {
        Value::Int(i) => Ok(*i as u64),
        Value::Float(f) => Ok(f.to_bits()),
        Value::String(s) => {
            Ok(s.parse::<i64>().map(|i| i as u64)
                .unwrap_or_else(|_| xxhash_rust::xxh3::xxh3_64(s.as_bytes())))
        }
        Value::Hex(bytes) => {
            Ok(bytes.iter().enumerate().fold(0u64, |acc, (i, &b)| acc | ((b as u64) << (8 * i))))
        }
    }
}

// ---------------------------------------------------------------------------
// Row filtering — evaluate WhereClause against a row
// ---------------------------------------------------------------------------

#[allow(clippy::only_used_in_recursion)]
fn row_matches(where_clause: &WhereClause, row: &[u64], table: &Table) -> bool {
    match where_clause {
        WhereClause::None => true,
        WhereClause::Single(f) => {
            let cell = row[f.col_idx];
            match f.op.as_str() {
                "=" => cell == f.value,
                "!=" => cell != f.value,
                "<" => cell < f.value,
                ">" => cell > f.value,
                "<=" => cell <= f.value,
                ">=" => cell >= f.value,
                _ => false,
            }
        }
        WhereClause::And(l, r) => row_matches(l, row, table) && row_matches(r, row, table),
        WhereClause::Or(l, r) => row_matches(l, row, table) || row_matches(r, row, table),
    }
}

fn filter_indices_old(where_clause: &WhereClause, table: &Table) -> Vec<usize> {
    match where_clause {
        WhereClause::None => (0..table.row_count).collect(),
        _ => {
            let mut indices = Vec::new();
            for i in 0..table.row_count {
                let row: Vec<u64> = table.columns.iter().map(|c| c[i]).collect();
                if row_matches(where_clause, &row, table) {
                    indices.push(i);
                }
            }
            indices
        }
    }
}

// ---------------------------------------------------------------------------
// GROUP BY execution
// ---------------------------------------------------------------------------

fn execute_group_by(
    query: &SelectQuery,
    where_clause: &WhereClause,
    table: &Table,
    _tier: MemoryTier,
    _kernel_table: &KernelTable,
) -> Result<QueryResult> {
    // Get matching row indices
    let indices = filter_indices(where_clause, table);

    // Resolve GROUP BY column indices
    let group_cols: Vec<usize> = query.group_by.iter()
        .map(|name| table.column_idx(name)
            .ok_or_else(|| Error::NotFound(format!("GROUP BY column '{}'", name))))
        .collect::<Result<Vec<_>>>()?;

    // Group rows by the composite key
    let mut groups: HashMap<Vec<u64>, Vec<usize>> = HashMap::new();
    for &idx in &indices {
        let key: Vec<u64> = group_cols.iter().map(|&c| table.columns[c][idx]).collect();
        groups.entry(key).or_default().push(idx);
    }

    // Build result columns
    let mut result_cols: Vec<ResultColumn> = Vec::new();

    // GROUP BY columns come first
    for (i, col_name) in query.group_by.iter().enumerate() {
        let values: Vec<u64> = groups.keys().map(|k| k[i]).collect();
        result_cols.push(ResultColumn { name: col_name.clone(), values, string_values: None });
    }

    // Aggregate columns
    for item in &query.select {
        if let SelectItem::Aggregate { func, arg, alias } = item {
            let name = alias.as_deref().unwrap_or(func.as_str());
            let values: Vec<u64> = groups.values().map(|indices| {
                compute_aggregate(func, arg, indices, table)
            }).collect();
            result_cols.push(ResultColumn { name: name.to_string(), values, string_values: None });
        }
    }

    let row_count = groups.len();

    // Apply ORDER BY if present
    let mut result = QueryResult { columns: result_cols, row_count, elapsed_us: 0 };
    if !query.order_by.is_empty() {
        result = order_group_result(result, &query.order_by)?;
    }

    Ok(result)
}

fn compute_aggregate(func: &str, arg: &str, indices: &[usize], table: &Table) -> u64 {
    let func_upper = func.to_uppercase();
    match func_upper.as_str() {
        "COUNT" => {
            if arg == "*" {
                indices.len() as u64
            } else {
                let idx = table.column_idx(arg).unwrap_or(0);
                indices.iter().filter(|&&i| table.columns[idx][i] != 0).count() as u64
            }
        }
        "COUNT_DISTINCT" => {
            // COUNT(DISTINCT col) — count unique non-zero values.
            use std::collections::HashSet;
            let idx = table.column_idx(arg).unwrap_or(0);
            let unique: HashSet<u64> = indices
                .iter()
                .map(|&i| table.columns[idx][i])
                .filter(|&v| v != 0)
                .collect();
            unique.len() as u64
        }
        "SUM" => {
            let idx = table.column_idx(arg).unwrap_or(0);
            let sum: u64 = indices.iter().map(|&i| table.columns[idx][i]).sum();
            sum
        }
        "AVG" => {
            let idx = table.column_idx(arg).unwrap_or(0);
            if indices.is_empty() { return 0; }
            let sum: u64 = indices.iter().map(|&i| table.columns[idx][i]).sum();
            sum / indices.len() as u64
        }
        "MIN" => {
            let idx = table.column_idx(arg).unwrap_or(0);
            indices.iter().map(|&i| table.columns[idx][i]).min().unwrap_or(0)
        }
        "MAX" => {
            let idx = table.column_idx(arg).unwrap_or(0);
            indices.iter().map(|&i| table.columns[idx][i]).max().unwrap_or(0)
        }
        _ => 0,
    }
}

fn order_group_result(result: QueryResult, order_by: &[(String, bool)]) -> Result<QueryResult> {
    if order_by.is_empty() || result.columns.is_empty() {
        return Ok(result);
    }

    let (col_name, ascending) = &order_by[0];
    let col_idx = result.columns.iter().position(|c| c.name == *col_name)
        .ok_or_else(|| Error::NotFound(format!("ORDER BY column '{}'", col_name)))?;

    let mut indices: Vec<usize> = (0..result.row_count).collect();
    indices.sort_by(|&a, &b| {
        let va = result.columns[col_idx].values[a];
        let vb = result.columns[col_idx].values[b];
        if *ascending { va.cmp(&vb) } else { vb.cmp(&va) }
    });

    let new_cols: Vec<ResultColumn> = result.columns.iter().map(|c| {
        let values: Vec<u64> = indices.iter().map(|&i| c.values[i]).collect();
        ResultColumn { name: c.name.clone(), values, string_values: None }
    }).collect();

    Ok(QueryResult { columns: new_cols, row_count: result.row_count, elapsed_us: result.elapsed_us })
}

// ---------------------------------------------------------------------------
// Aggregate without GROUP BY (scalar result)
// ---------------------------------------------------------------------------

fn execute_aggregate_no_group(
    select: &[SelectItem],
    where_clause: &WhereClause,
    table: &Table,
    _tier: MemoryTier,
    _kernel_table: &KernelTable,
) -> Result<QueryResult> {
    let indices = filter_indices(where_clause, table);
    let mut cols = Vec::new();

    for item in select {
        match item {
            SelectItem::Column(name) => {
                let idx = table.column_idx(name)
                    .ok_or_else(|| Error::NotFound(format!("column '{}'", name)))?;
                let val = if indices.len() == 1 { table.columns[idx][indices[0]] } else { 0 };
                cols.push(ResultColumn { name: name.clone(), values: vec![val] , string_values: None });
            }
            SelectItem::Aggregate { func, arg, alias } => {
                let name = alias.as_deref().unwrap_or(func.as_str());
                let val = compute_aggregate(func, arg, &indices, table);
                cols.push(ResultColumn { name: name.to_string(), values: vec![val] , string_values: None });
            }
            SelectItem::Star => {
                cols.push(ResultColumn { name: "count".into(), values: vec![indices.len() as u64] , string_values: None });
            }
            SelectItem::Literal(v) => {
                cols.push(ResultColumn { name: v.to_string(), values: vec![*v] , string_values: None });
            }
            SelectItem::Window { .. } => {
                return Err(Error::Other("window function in multi-aggregate — should use tpch fallback".into()));
            }
        }
    }

    Ok(QueryResult { columns: cols, row_count: 1, elapsed_us: 0 })
}

// ---------------------------------------------------------------------------
// Original aggregate execution (count, sum, avg, min, max, count distinct)
// ---------------------------------------------------------------------------

fn pick_tier(ext: &QueryExtensions) -> MemoryTier {
    if let Some(tier_name) = &ext.tier {
        match tier_name.to_uppercase().as_str() {
            "L3" => MemoryTier::L3,
            "DDR5" | "DRAM" => MemoryTier::Ddr5,
            "CXL" => MemoryTier::Cxl,
            "NVME" => MemoryTier::Nvme,
            _ => MemoryTier::L3,
        }
    } else {
        MemoryTier::L3
    }
}

fn execute_aggregate(
    func: &str,
    arg: &str,
    alias: Option<&str>,
    where_clause: &WhereClause,
    table: &Table,
    _tier: MemoryTier,
    kernel_table: &KernelTable,
) -> Result<QueryResult> {
    let func_upper = func.to_uppercase();
    let name = alias.unwrap_or(func);

    match func_upper.as_str() {
        "COUNT" => execute_count(arg, name, where_clause, table, kernel_table),
        "SUM" => execute_sum(arg, name, where_clause, table),
        "AVG" => execute_avg(arg, name, where_clause, table),
        "MIN" => execute_min(arg, name, where_clause, table),
        "COUNT_DISTINCT" => execute_count_distinct(arg, name, where_clause, table),
        "MAX" => execute_max(arg, name, where_clause, table),
        _ => Err(Error::Other(format!("unsupported aggregate function: {}", func))),
    }
}

fn execute_count(arg: &str, name: &str, where_clause: &WhereClause, table: &Table, kernel_table: &KernelTable) -> Result<QueryResult> {
    // Special case: COUNT(*) with no WHERE = row count
    if arg == "*" {
        if let WhereClause::None = where_clause {
            return Ok(QueryResult {
                columns: vec![ResultColumn { name: name.into(), values: vec![table.row_count as u64] , string_values: None }],
                row_count: 1,
                elapsed_us: 0,
            });
        }
    }

    // Use kernel for single equality filter
    if let WhereClause::Single(f) = where_clause {
        if f.op == "=" {
            let col = &table.columns[f.col_idx];
            let kernel = kernel_table.select(Operator::ScanEqU64, MemoryTier::L3)
                .ok_or_else(|| Error::Unsupported("no ScanEqU64 kernel".into()))?;
            let params = KernelParams {
                target_u64: f.value,
                cell_count: col.len(),
                ..Default::default()
            };
            let mut output = [0u8; 64];
            let result = unsafe { kernel.execute(col.as_ptr() as *const u8, output.as_mut_ptr(), &params) };
            return Ok(QueryResult {
                columns: vec![ResultColumn { name: name.into(), values: vec![result.count] , string_values: None }],
                row_count: 1,
                elapsed_us: 0,
            });
        }
    }

    // Fallback: row-by-row filtering
    let indices = filter_indices(where_clause, table);
    let count = if arg == "*" {
        indices.len() as u64
    } else {
        let idx = table.column_idx(arg).unwrap_or(0);
        indices.iter().filter(|&&i| table.columns[idx][i] != 0).count() as u64
    };
    Ok(QueryResult {
        columns: vec![ResultColumn { name: name.into(), values: vec![count] , string_values: None }],
        row_count: 1,
        elapsed_us: 0,
    })
}

fn execute_sum(arg: &str, name: &str, where_clause: &WhereClause, table: &Table) -> Result<QueryResult> {
    let idx = table.column_idx(arg)
        .ok_or_else(|| Error::NotFound(format!("column '{}'", arg)))?;

    // For large tables with no WHERE, use parallel execution (Wave 29).
    let sum: u64 = if let WhereClause::None = where_clause {
        if table.row_count > 10_000 {
            crate::exec::parallel::parallel_sum(&table.columns[idx])
        } else {
            table.columns[idx].iter().sum()
        }
    } else {
        let indices = filter_indices(where_clause, table);
        indices.iter().map(|&i| table.columns[idx][i]).sum()
    };
    // Return as f64 bits so scalar_f64() interprets correctly
    Ok(QueryResult {
        columns: vec![ResultColumn { name: name.into(), values: vec![(sum as f64).to_bits()] , string_values: None }],
        row_count: 1,
        elapsed_us: 0,
    })
}

fn execute_avg(arg: &str, name: &str, where_clause: &WhereClause, table: &Table) -> Result<QueryResult> {
    let idx = table.column_idx(arg)
        .ok_or_else(|| Error::NotFound(format!("column '{}'", arg)))?;
    let indices = filter_indices(where_clause, table);
    if indices.is_empty() {
        return Ok(QueryResult {
            columns: vec![ResultColumn { name: name.into(), values: vec![0u64] , string_values: None }],
            row_count: 1,
            elapsed_us: 0,
        });
    }
    let sum: u64 = indices.iter().map(|&i| table.columns[idx][i]).sum();
    let avg = sum as f64 / indices.len() as f64;
    Ok(QueryResult {
        columns: vec![ResultColumn { name: name.into(), values: vec![avg.to_bits()] , string_values: None }],
        row_count: 1,
        elapsed_us: 0,
    })
}

fn execute_min(arg: &str, name: &str, where_clause: &WhereClause, table: &Table) -> Result<QueryResult> {
    let idx = table.column_idx(arg)
        .ok_or_else(|| Error::NotFound(format!("column '{}'", arg)))?;
    let min = if let WhereClause::None = where_clause {
        if table.row_count > 10_000 {
            crate::exec::parallel::parallel_min(&table.columns[idx])
        } else {
            table.columns[idx].iter().min().copied().unwrap_or(0)
        }
    } else {
        let indices = filter_indices(where_clause, table);
        indices.iter().map(|&i| table.columns[idx][i]).min().unwrap_or(0)
    };
    Ok(QueryResult {
        columns: vec![ResultColumn { name: name.into(), values: vec![min] , string_values: None }],
        row_count: 1,
        elapsed_us: 0,
    })
}

fn execute_max(arg: &str, name: &str, where_clause: &WhereClause, table: &Table) -> Result<QueryResult> {
    let idx = table.column_idx(arg)
        .ok_or_else(|| Error::NotFound(format!("column '{}'", arg)))?;
    let max = if let WhereClause::None = where_clause {
        if table.row_count > 10_000 {
            crate::exec::parallel::parallel_max(&table.columns[idx])
        } else {
            table.columns[idx].iter().max().copied().unwrap_or(0)
        }
    } else {
        let indices = filter_indices(where_clause, table);
        indices.iter().map(|&i| table.columns[idx][i]).max().unwrap_or(0)
    };
    Ok(QueryResult {
        columns: vec![ResultColumn { name: name.into(), values: vec![max] , string_values: None }],
        row_count: 1,
        elapsed_us: 0,
    })
}

// ---------------------------------------------------------------------------
// SELECT * and SELECT col
// ---------------------------------------------------------------------------

fn execute_count_distinct(arg: &str, name: &str, where_clause: &WhereClause, table: &Table) -> Result<QueryResult> {
    let idx = table.column_idx(arg)
        .ok_or_else(|| Error::NotFound(format!("column '{}'", arg)))?;
    let indices = filter_indices(where_clause, table);
    let mut seen = std::collections::HashSet::new();
    for &i in &indices {
        seen.insert(table.columns[idx][i]);
    }
    Ok(QueryResult {
        columns: vec![ResultColumn { name: name.into(), values: vec![seen.len() as u64] , string_values: None }],
        row_count: 1,
        elapsed_us: 0,
    })
}

fn execute_select_star(
    where_clause: &WhereClause,
    table: &Table,
    limit: Option<usize>,
) -> Result<QueryResult> {
    let indices = filter_indices(where_clause, table);
    let limit = limit.unwrap_or(indices.len());
    let indices: Vec<usize> = indices.into_iter().take(limit).collect();

    let cols: Vec<ResultColumn> = table.column_names.iter().enumerate().map(|(i, name)| {
        let values: Vec<u64> = indices.iter().map(|&idx| table.columns[i][idx]).collect();
        ResultColumn { name: name.clone(), values, string_values: None }
    }).collect();

    Ok(QueryResult {
        columns: cols,
        row_count: indices.len(),
        elapsed_us: 0,
    })
}

fn execute_select_column(
    name: &str,
    where_clause: &WhereClause,
    table: &Table,
    limit: Option<usize>,
) -> Result<QueryResult> {
    let idx = table.column_idx(name)
        .ok_or_else(|| Error::NotFound(format!("column '{}'", name)))?;
    let indices = filter_indices(where_clause, table);
    let limit = limit.unwrap_or(indices.len());
    let indices: Vec<usize> = indices.into_iter().take(limit).collect();

    let values: Vec<u64> = indices.iter().map(|&i| table.columns[idx][i]).collect();

    // If the column has a string sidecar, return the original strings.
    let string_values = if idx < table.string_columns.len() {
        if let Some(ref sc) = table.string_columns[idx] {
            let strings: Vec<String> = indices.iter().map(|&i| sc.get(i).to_string()).collect();
            Some(strings)
        } else {
            None
        }
    } else {
        None
    };

    Ok(QueryResult {
        columns: vec![ResultColumn { name: name.into(), values, string_values }],
        row_count: indices.len(),
        elapsed_us: 0,
    })
}

fn execute_select_multi(
    select: &[SelectItem],
    where_clause: &WhereClause,
    table: &Table,
    _order_by: &[(String, bool)],
    limit: Option<usize>,
) -> Result<QueryResult> {
    let indices = filter_indices(where_clause, table);
    let limit = limit.unwrap_or(indices.len());
    let indices: Vec<usize> = indices.into_iter().take(limit).collect();

    let mut cols = Vec::new();
    for item in select {
        if let SelectItem::Column(name) = item {
            let idx = table.column_idx(name)
                .ok_or_else(|| Error::NotFound(format!("column '{}'", name)))?;
            let values: Vec<u64> = indices.iter().map(|&i| table.columns[idx][i]).collect();
            cols.push(ResultColumn { name: name.clone(), values, string_values: None });
        } else if let SelectItem::Star = item {
            for (col_idx, name) in table.column_names.iter().enumerate() {
                let values: Vec<u64> = indices.iter().map(|&row_idx| table.columns[col_idx][row_idx]).collect();
                cols.push(ResultColumn { name: name.clone(), values, string_values: None });
            }
        }
    }

    Ok(QueryResult {
        columns: cols,
        row_count: indices.len(),
        elapsed_us: 0,
    })
}

// ---------------------------------------------------------------------------
// ORDER BY (for non-group-by queries)
// ---------------------------------------------------------------------------

fn apply_order_by(result: QueryResult, order_by: &[(String, bool)], _table: &Table) -> Result<QueryResult> {
    if order_by.is_empty() || result.columns.is_empty() || result.row_count <= 1 {
        return Ok(result);
    }

    let (col_name, ascending) = &order_by[0];
    let col_idx = result.columns.iter().position(|c| c.name == *col_name)
        .ok_or_else(|| Error::NotFound(format!("ORDER BY column '{}'", col_name)))?;

    let mut indices: Vec<usize> = (0..result.row_count).collect();
    indices.sort_by(|&a, &b| {
        let va = result.columns[col_idx].values[a];
        let vb = result.columns[col_idx].values[b];
        if *ascending { va.cmp(&vb) } else { vb.cmp(&va) }
    });

    let new_cols: Vec<ResultColumn> = result.columns.iter().map(|c| {
        let values: Vec<u64> = indices.iter().map(|&i| c.values[i]).collect();
        ResultColumn { name: c.name.clone(), values, string_values: None }
    }).collect();

    Ok(QueryResult { columns: new_cols, row_count: result.row_count, elapsed_us: result.elapsed_us })
}

// ---------------------------------------------------------------------------
// Vectorized batch path (P0 fix) — replaces per-row ScalarValue boxing
// ---------------------------------------------------------------------------

/// Try to evaluate the WHERE clause using the vectorized batch path.
fn filter_indices_batch(where_clause: &WhereClause, table: &Table) -> Option<Vec<usize>> {
    match where_clause {
        WhereClause::Single(f) => {
            let expr = filter_to_expr(f);
            Some(crate::exec::vectorized::filter_rows(&table.columns, &table.column_names, table.row_count, &expr))
        }
        WhereClause::And(l, r) => {
            let left_expr = where_clause_to_expr(l);
            let right_expr = where_clause_to_expr(r);
            let expr = crate::sql::parser::Expr::Binary {
                left: Box::new(left_expr),
                op: String::from("AND"),
                right: Box::new(right_expr),
            };
            Some(crate::exec::vectorized::filter_rows(&table.columns, &table.column_names, table.row_count, &expr))
        }
        WhereClause::Or(l, r) => {
            let left_expr = where_clause_to_expr(l);
            let right_expr = where_clause_to_expr(r);
            let expr = crate::sql::parser::Expr::Binary {
                left: Box::new(left_expr),
                op: String::from("OR"),
                right: Box::new(right_expr),
            };
            Some(crate::exec::vectorized::filter_rows(&table.columns, &table.column_names, table.row_count, &expr))
        }
        WhereClause::None => Some((0..table.row_count).collect()),
    }
}

fn filter_to_expr(f: &Filter) -> crate::sql::parser::Expr {
    crate::sql::parser::Expr::Binary {
        left: Box::new(crate::sql::parser::Expr::Column(f.col_idx.to_string())),
        op: f.op.clone(),
        right: Box::new(crate::sql::parser::Expr::Literal(crate::sql::parser::Value::Int(f.value as i64))),
    }
}

fn where_clause_to_expr(wc: &WhereClause) -> crate::sql::parser::Expr {
    match wc {
        WhereClause::Single(f) => filter_to_expr(f),
        WhereClause::And(l, r) => crate::sql::parser::Expr::Binary {
            left: Box::new(where_clause_to_expr(l)),
            op: String::from("AND"),
            right: Box::new(where_clause_to_expr(r)),
        },
        WhereClause::Or(l, r) => crate::sql::parser::Expr::Binary {
            left: Box::new(where_clause_to_expr(l)),
            op: String::from("OR"),
            right: Box::new(where_clause_to_expr(r)),
        },
        WhereClause::None => crate::sql::parser::Expr::Literal(crate::sql::parser::Value::Int(1)),
    }
}

/// New filter_indices: tries vectorized batch path first, falls back to per-row.
fn filter_indices(where_clause: &WhereClause, table: &Table) -> Vec<usize> {
    if let Some(indices) = filter_indices_batch(where_clause, table) {
        return indices;
    }
    filter_indices_old(where_clause, table)
}


// ---------------------------------------------------------------------------
// JOIN execution — materialize joined table, then dispatch.
// ---------------------------------------------------------------------------

fn execute_with_join(
    query: &crate::sql::parser::SelectQuery,
    _extensions: &crate::sql::extensions::QueryExtensions,
    catalog: &crate::catalog::Catalog,
    _kernel_table: &crate::kernel::KernelTable,
) -> Result<QueryResult> {
    use crate::exec::join::{hash_join, extract_join_keys, JoinType};

    let base = catalog
        .get(&query.from)
        .ok_or_else(|| Error::NotFound(format!("table '{}'", query.from)))?;

    let mut running = base.clone();

    for join in &query.joins {
        let right = catalog
            .get(&join.table)
            .ok_or_else(|| Error::NotFound(format!("table '{}'", join.table)))?;

        let keys = extract_join_keys(&join.on, &running, right)?;
        let result = hash_join(&running, right, &keys, JoinType::Inner)?;
        let mut new_table = result.into_table(&format!("__join_{}", join.table));
        // Rename columns from the right table to be qualified (table.col)
        // so they can be resolved by qualified names like l_orderkey
        let left_col_count = running.columns.len();
        for i in left_col_count..new_table.column_names.len() {
            let right_idx = i - left_col_count;
            if let Some(right_name) = right.column_names.get(right_idx) {
                new_table.column_names[i] = format!("{}.{}", join.table, right_name);
            }
        }
        // Also prefix left columns with their source table
        for i in 0..left_col_count {
            if !new_table.column_names[i].contains('.') {
                new_table.column_names[i] = format!("{}.{}", query.from, new_table.column_names[i]);
            }
        }
        running = new_table;
    }

    // Build a modified query without JOINs and dispatch on the joined table.
    let mut modified = query.clone();
    modified.joins.clear();
    if let Some(result) = dispatch::execute_dispatched(&modified, &running) {
        return result;
    }

    // Fallback to old executor path
    let filter = parse_where(&modified.where_clause, &running)?;
    let tier = crate::memory::tier::MemoryTier::L3;
    if !modified.group_by.is_empty() {
        execute_group_by(&modified, &filter, &running, tier, _kernel_table)
    } else if modified.select.len() == 1 {
        match &modified.select[0] {
            crate::sql::parser::SelectItem::Aggregate { func, arg, alias } => {
                execute_aggregate(func, arg, alias.as_deref(), &filter, &running, tier, _kernel_table)
            }
            crate::sql::parser::SelectItem::Star => execute_select_star(&filter, &running, modified.limit),
            crate::sql::parser::SelectItem::Column(name) => {
                execute_select_column(name, &filter, &running, modified.limit)
            }
            // Bare literal in a join-context SELECT — emit single row.
            // Joins with literal SELECT items are not in the ClickBench /
            // TPC-H query set, so this is a defensive default.
            crate::sql::parser::SelectItem::Literal(v) => {
                Ok(QueryResult {
                    columns: vec![ResultColumn { name: v.to_string(), values: vec![*v] , string_values: None }],
                    row_count: 1,
                    elapsed_us: 0,
                })
            }
            crate::sql::parser::SelectItem::Window { .. } => {
                Err(Error::Other("window function in join context — use tpch fallback".into()))
            }
        }
    } else {
        execute_select_multi(&modified.select, &filter, &running, &modified.order_by, modified.limit)
    }
}
