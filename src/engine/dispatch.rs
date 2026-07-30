//! Kernel-direct query dispatch — pattern-match SQL shape, call kernels.
//!
//! Research: Compiler pattern recognition (recognize complex expressions
//! as single operations). Signal processing: FFT convolution for complex
//! multi-predicate filters (future work).
//!
//! This eliminates ALL abstraction overhead:
//! SQL → Parse Tree → Pattern Match → Kernel Call → Result
//! No ScalarValue, no Expr tree, no per-row evaluation.

use crate::datasource::table::Table;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::exec::vectorized;
use crate::sql::parser::{Expr, SelectItem, SelectQuery, Value};
use crate::Error;

type Result<T> = std::result::Result<T, Error>;

/// Query shape classification — determines which kernel combination to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryShape {
    /// SELECT count(*) FROM t
    CountAll,
    /// SELECT count(*) FROM t WHERE col OP val
    CountFilter,
    /// SELECT sum(col) FROM t [WHERE col2 OP val]
    SumCol,
    /// SELECT min(col) / max(col) FROM t [WHERE ...]
    MinMax,
    /// SELECT count(DISTINCT col) FROM t [WHERE ...]
    CountDistinct,
    /// SELECT avg(col) FROM t [WHERE ...]
    AvgCol,
    /// SELECT col, count(*) FROM t GROUP BY col
    GroupByCount,
    /// SELECT col, sum(col2) FROM t GROUP BY col
    GroupBySum,
    /// SELECT col, count(*) ... GROUP BY ... ORDER BY ... LIMIT
    GroupByOrderByLimit,
    /// SELECT * FROM t [WHERE ...] [LIMIT]
    SelectStar,
    /// SELECT col FROM t [WHERE ...] [LIMIT]
    SelectColumn,
    /// SELECT col1, col2 FROM t [WHERE ...] [LIMIT]
    SelectMulti,
    /// Too complex for kernel dispatch — use fallback evaluator
    Complex,
}

/// Classify a parsed SELECT query into a shape for kernel dispatch.
pub fn classify_query(query: &SelectQuery) -> QueryShape {
    // Note: Wave 22 SelectQuery doesn't have joins field.
    // JOIN support will be added in Wave 45.

    let has_group_by = !query.group_by.is_empty();
    let has_order_by = !query.order_by.is_empty();
    let has_limit = query.limit.is_some();
    let has_where = query.where_clause.is_some();

    if has_group_by {
        // Check if all select items are either GROUP BY columns or count(*)
        let has_agg = query.select.iter().any(|s| matches!(s, SelectItem::Aggregate { .. }));
        if !has_agg { return QueryShape::Complex; }

        // Check if the aggregate is count(*) or sum(col)
        let agg = query.select.iter().find_map(|s| {
            if let SelectItem::Aggregate { func, arg, .. } = s {
                Some((func.as_str(), arg.as_str()))
            } else {
                None
            }
        });

        match agg {
            Some(("COUNT", "*")) | Some(("COUNT", _)) => {
                if has_order_by && has_limit {
                    QueryShape::GroupByOrderByLimit
                } else {
                    QueryShape::GroupByCount
                }
            }
            Some(("SUM", _)) => QueryShape::GroupBySum,
            _ => QueryShape::Complex,
        }
    } else if query.select.len() == 1 {
        match &query.select[0] {
            SelectItem::Aggregate { func, arg, .. } => {
                let f = func.to_uppercase();
                match (f.as_str(), arg.as_str(), has_where) {
                    ("COUNT", "*", false) => QueryShape::CountAll,
                    ("COUNT", "*", true) | ("COUNT", _, true) => QueryShape::CountFilter,
                    ("COUNT", _, false) => QueryShape::CountFilter,
                    ("COUNT_DISTINCT", _, _) => QueryShape::CountDistinct,
                    ("SUM", _, _) => QueryShape::SumCol,
                    ("AVG", _, _) => QueryShape::AvgCol,
                    ("MIN", _, _) | ("MAX", _, _) => QueryShape::MinMax,
                    _ => QueryShape::Complex,
                }
            }
            SelectItem::Star => QueryShape::SelectStar,
            SelectItem::Column(_) => QueryShape::SelectColumn,
        }
    } else if query.select.len() > 1 {
        let has_agg = query.select.iter().any(|s| matches!(s, SelectItem::Aggregate { .. }));
        if has_agg {
            QueryShape::Complex // mixed column+agg without GROUP BY
        } else {
            QueryShape::SelectMulti
        }
    } else {
        QueryShape::Complex
    }
}

/// Execute a query using kernel-direct dispatch.
/// Returns None if the shape is Complex (caller should use fallback).
pub fn execute_dispatched(
    query: &SelectQuery,
    table: &Table,
) -> Option<Result<QueryResult>> {
    let shape = classify_query(query);
    if shape == QueryShape::Complex {
        return None;
    }
    Some(execute_shape(shape, query, table))
}

fn execute_shape(shape: QueryShape, query: &SelectQuery, table: &Table) -> Result<QueryResult> {
    match shape {
        QueryShape::CountAll => {
            Ok(single_value("count", table.row_count as u64))
        }
        QueryShape::CountFilter => {
            let mask = build_filter_mask(query, table)?;
            let count = vectorized::count_masked(&mask);
            Ok(single_value("count", count))
        }
        QueryShape::SumCol => {
            let mask = build_filter_mask(query, table)?;
            let col_idx = resolve_agg_col(&query.select[0], table)?;
            let sum = vectorized::sum_masked(&table.columns[col_idx], &mask);
            Ok(single_value("sum", sum))
        }
        QueryShape::AvgCol => {
            let mask = build_filter_mask(query, table)?;
            let col_idx = resolve_agg_col(&query.select[0], table)?;
            let avg = vectorized::avg_masked(&table.columns[col_idx], &mask);
            Ok(single_value("avg", avg))
        }
        QueryShape::MinMax => {
            let mask = build_filter_mask(query, table)?;
            let col_idx = resolve_agg_col(&query.select[0], table)?;
            let func = if let SelectItem::Aggregate { func, .. } = &query.select[0] {
                func.to_uppercase()
            } else {
                return Err(Error::Other("expected aggregate".into()));
            };
            let val = match func.as_str() {
                "MIN" => vectorized::min_masked(&table.columns[col_idx], &mask),
                "MAX" => vectorized::max_masked(&table.columns[col_idx], &mask),
                _ => return Err(Error::Other(format!("unsupported: {func}"))),
            };
            Ok(single_value(&func.to_lowercase(), val))
        }
        QueryShape::CountDistinct => {
            let mask = build_filter_mask(query, table)?;
            let col_idx = resolve_agg_col(&query.select[0], table)?;
            let count = vectorized::count_distinct_masked(&table.columns[col_idx], &mask);
            Ok(single_value("count", count))
        }
        QueryShape::GroupByCount | QueryShape::GroupBySum | QueryShape::GroupByOrderByLimit => {
            execute_group_by(query, table)
        }
        QueryShape::SelectStar => {
            let mask = build_filter_mask(query, table)?;
            let limit = query.limit.unwrap_or(mask.iter().filter(|&&b| b).count());
            let indices: Vec<usize> = (0..table.row_count).filter(|&i| mask[i]).take(limit).collect();
            let cols: Vec<ResultColumn> = table.column_names.iter().enumerate().map(|(i, name)| {
                let values: Vec<u64> = indices.iter().map(|&idx| table.columns[i][idx]).collect();
                ResultColumn { name: name.clone(), values }
            }).collect();
            Ok(QueryResult { columns: cols, row_count: indices.len(), elapsed_us: 0 })
        }
        QueryShape::SelectColumn => {
            let mask = build_filter_mask(query, table)?;
            let name = if let SelectItem::Column(n) = &query.select[0] { n.clone() } else { return Err(Error::Other("expected column".into())); };
            let col_idx = resolve_col_name(&name, table)?;
            let limit = query.limit.unwrap_or(mask.iter().filter(|&&b| b).count());
            let values: Vec<u64> = (0..table.row_count).filter(|&i| mask[i]).take(limit).map(|i| table.columns[col_idx][i]).collect();
            Ok(QueryResult { columns: vec![ResultColumn { name, values: values.clone() }], row_count: values.len(), elapsed_us: 0 })
        }
        QueryShape::SelectMulti => {
            let mask = build_filter_mask(query, table)?;
            let limit = query.limit.unwrap_or(mask.iter().filter(|&&b| b).count());
            let indices: Vec<usize> = (0..table.row_count).filter(|&i| mask[i]).take(limit).collect();
            let mut cols = Vec::new();
            for item in &query.select {
                if let SelectItem::Column(name) = item {
                    let col_idx = resolve_col_name(name, table)?;
                    let values: Vec<u64> = indices.iter().map(|&i| table.columns[col_idx][i]).collect();
                    cols.push(ResultColumn { name: name.clone(), values });
                } else if let SelectItem::Star = item {
                    for (col_idx, name) in table.column_names.iter().enumerate() {
                        let values: Vec<u64> = indices.iter().map(|&row_idx| table.columns[col_idx][row_idx]).collect();
                        cols.push(ResultColumn { name: name.clone(), values });
                    }
                }
            }
            Ok(QueryResult { columns: cols, row_count: indices.len(), elapsed_us: 0 })
        }
        QueryShape::Complex => Err(Error::Other("complex query not supported by dispatcher".into())),
    }
}

fn build_filter_mask(query: &SelectQuery, table: &Table) -> Result<Vec<bool>> {
    match &query.where_clause {
        None => Ok(vec![true; table.row_count]),
        Some(expr) => {
            // filter_rows returns Vec<usize> (indices), convert to mask
            let indices = vectorized::filter_rows(&table.columns, &table.column_names, table.row_count, expr);
            let mut mask = vec![false; table.row_count];
            for i in indices { mask[i] = true; }
            Ok(mask)
        }
    }
}

fn resolve_agg_col(item: &SelectItem, table: &Table) -> Result<usize> {
    if let SelectItem::Aggregate { arg, .. } = item {
        if arg == "*" { return Ok(0); }
        return resolve_col_name(arg, table);
    }
    Err(Error::Other("expected aggregate".into()))
}

fn resolve_col_name(name: &str, table: &Table) -> Result<usize> {
    // Try direct lookup
    if let Some(idx) = table.column_idx(name) {
        return Ok(idx);
    }
    // Try stripping table prefix
    if let Some(bare) = name.split('.').nth(1) {
        if let Some(idx) = table.column_idx(bare) {
            return Ok(idx);
        }
    }
    Err(Error::NotFound(format!("column '{}'", name)))
}

fn single_value(name: &str, value: u64) -> QueryResult {
    QueryResult {
        columns: vec![ResultColumn { name: name.to_string(), values: vec![value] }],
        row_count: 1,
        elapsed_us: 0,
    }
}

fn execute_group_by(query: &SelectQuery, table: &Table) -> Result<QueryResult> {
    use crate::exec::flat_hash_table::{hash_group_by_flat, AggFunc};
    use std::collections::HashMap;

    // Filter rows
    let mask = build_filter_mask(query, table)?;
    let indices: Vec<usize> = (0..table.row_count).filter(|&i| mask[i]).collect();

    // Resolve GROUP BY columns
    let group_cols: Vec<usize> = query.group_by.iter()
        .map(|name| resolve_col_name(name, table))
        .collect::<Result<Vec<_>>>()?;

    // For single-key GROUP BY: use flat hash table (fast path)
    if group_cols.len() == 1 {
        let group_col = group_cols[0];
        let keys: Vec<u64> = indices.iter().map(|&i| table.columns[group_col][i]).collect();

        // Determine aggregate function and value column
        for item in &query.select {
            if let SelectItem::Aggregate { func, arg, alias } = item {
                let name = alias.as_deref().unwrap_or(func.as_str());
                let func_upper = func.to_uppercase();
                let (agg_func, values) = match func_upper.as_str() {
                    "COUNT" => {
                        if arg == "*" {
                            (AggFunc::Count, None)
                        } else {
                            let col_idx = resolve_col_name(arg, table)?;
                            let vals: Vec<u64> = indices.iter().map(|&i| table.columns[col_idx][i]).collect();
                            (AggFunc::Count, Some(vals))
                        }
                    }
                    "SUM" => {
                        let col_idx = resolve_col_name(arg, table)?;
                        let vals: Vec<u64> = indices.iter().map(|&i| table.columns[col_idx][i]).collect();
                        (AggFunc::Sum, Some(vals))
                    }
                    "AVG" => {
                        let col_idx = resolve_col_name(arg, table)?;
                        let vals: Vec<u64> = indices.iter().map(|&i| table.columns[col_idx][i]).collect();
                        (AggFunc::Avg, Some(vals))
                    }
                    "MIN" => {
                        let col_idx = resolve_col_name(arg, table)?;
                        let vals: Vec<u64> = indices.iter().map(|&i| table.columns[col_idx][i]).collect();
                        (AggFunc::Min, Some(vals))
                    }
                    "MAX" => {
                        let col_idx = resolve_col_name(arg, table)?;
                        let vals: Vec<u64> = indices.iter().map(|&i| table.columns[col_idx][i]).collect();
                        (AggFunc::Max, Some(vals))
                    }
                    "COUNT_DISTINCT" => {
                        let col_idx = resolve_col_name(arg, table)?;
                        let vals: Vec<u64> = indices.iter().map(|&i| table.columns[col_idx][i]).collect();
                        (AggFunc::CountDistinct, Some(vals))
                    }
                    _ => return Err(Error::Other(format!("unsupported agg: {func}"))),
                };

                let results = hash_group_by_flat(&keys, values.as_deref(), agg_func);

                // Build result columns
                let mut result_cols: Vec<ResultColumn> = Vec::new();
                // GROUP BY column
                let gb_name = &query.group_by[0];
                let gb_values: Vec<u64> = results.iter().map(|(k, _)| *k).collect();
                result_cols.push(ResultColumn { name: gb_name.clone(), values: gb_values });
                // Aggregate column
                let agg_values: Vec<u64> = results.iter().map(|(_, v)| *v).collect();
                result_cols.push(ResultColumn { name: name.to_string(), values: agg_values });

                let row_count = results.len();
                let mut result = QueryResult { columns: result_cols, row_count, elapsed_us: 0 };

                // Apply ORDER BY
                if !query.order_by.is_empty() {
                    let (col_name, ascending) = &query.order_by[0];
                    let col_idx = result.columns.iter().position(|c| c.name == *col_name)
                        .ok_or_else(|| Error::NotFound(format!("ORDER BY column '{}'", col_name)))?;
                    let mut idx: Vec<usize> = (0..result.row_count).collect();
                    idx.sort_by(|&a, &b| {
                        let va = result.columns[col_idx].values[a];
                        let vb = result.columns[col_idx].values[b];
                        if *ascending { va.cmp(&vb) } else { vb.cmp(&va) }
                    });
                    let new_cols: Vec<ResultColumn> = result.columns.iter().map(|c| {
                        let values: Vec<u64> = idx.iter().map(|&i| c.values[i]).collect();
                        ResultColumn { name: c.name.clone(), values }
                    }).collect();
                    result = QueryResult { columns: new_cols, row_count: result.row_count, elapsed_us: result.elapsed_us };
                }

                // Apply LIMIT
                if let Some(limit) = query.limit {
                    if result.row_count > limit {
                        for col in &mut result.columns { col.values.truncate(limit); }
                        result.row_count = limit;
                    }
                }

                return Ok(result);
            }
        }
    }

    // Multi-key GROUP BY: fall back to HashMap
    let mut groups: HashMap<u64, Vec<usize>> = HashMap::new();
    for &idx in &indices {
        let mut h = 0u64;
        for &col in &group_cols {
            h = h.wrapping_mul(0x517cc1b727220a95).wrapping_add(table.columns[col][idx]);
        }
        groups.entry(h).or_default().push(idx);
    }

    let mut result_cols: Vec<ResultColumn> = Vec::new();
    for (i, col_name) in query.group_by.iter().enumerate() {
        let values: Vec<u64> = groups.keys().map(|h| {
            if let Some(indices) = groups.get(h) {
                if let Some(&first_idx) = indices.first() {
                    return table.columns[group_cols[i]][first_idx];
                }
            }
            0
        }).collect();
        result_cols.push(ResultColumn { name: col_name.clone(), values });
    }

    for item in &query.select {
        if let SelectItem::Aggregate { func, arg, alias } = item {
            let name = alias.as_deref().unwrap_or(func.as_str());
            let func_upper = func.to_uppercase();
            let values: Vec<u64> = groups.values().map(|idxs| {
                match func_upper.as_str() {
                    "COUNT" => {
                        if arg == "*" { idxs.len() as u64 }
                        else {
                            let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                            idxs.iter().filter(|&&i| table.columns[col_idx][i] != 0).count() as u64
                        }
                    }
                    "SUM" => {
                        let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                        let sum: u64 = idxs.iter().map(|&i| table.columns[col_idx][i]).sum();
                        (sum as f64).to_bits()
                    }
                    "AVG" => {
                        let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                        if idxs.is_empty() { 0 }
                        else {
                            let sum: u64 = idxs.iter().map(|&i| table.columns[col_idx][i]).sum();
                            (sum as f64 / idxs.len() as f64).to_bits()
                        }
                    }
                    "MIN" => {
                        let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                        idxs.iter().map(|&i| table.columns[col_idx][i]).min().unwrap_or(0)
                    }
                    "MAX" => {
                        let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                        idxs.iter().map(|&i| table.columns[col_idx][i]).max().unwrap_or(0)
                    }
                    "COUNT_DISTINCT" => {
                        let col_idx = resolve_col_name(arg, table).unwrap_or(0);
                        let seen: std::collections::HashSet<u64> = idxs.iter().map(|&i| table.columns[col_idx][i]).collect();
                        seen.len() as u64
                    }
                    _ => 0,
                }
            }).collect();
            result_cols.push(ResultColumn { name: name.to_string(), values });
        }
    }

    let row_count = groups.len();
    let mut result = QueryResult { columns: result_cols, row_count, elapsed_us: 0 };

    if !query.order_by.is_empty() {
        let (col_name, ascending) = &query.order_by[0];
        let col_idx = result.columns.iter().position(|c| c.name == *col_name)
            .ok_or_else(|| Error::NotFound(format!("ORDER BY column '{}'", col_name)))?;
        let mut idx: Vec<usize> = (0..result.row_count).collect();
        idx.sort_by(|&a, &b| {
            let va = result.columns[col_idx].values[a];
            let vb = result.columns[col_idx].values[b];
            if *ascending { va.cmp(&vb) } else { vb.cmp(&va) }
        });
        let new_cols: Vec<ResultColumn> = result.columns.iter().map(|c| {
            let values: Vec<u64> = idx.iter().map(|&i| c.values[i]).collect();
            ResultColumn { name: c.name.clone(), values }
        }).collect();
        result = QueryResult { columns: new_cols, row_count: result.row_count, elapsed_us: result.elapsed_us };
    }

    if let Some(limit) = query.limit {
        if result.row_count > limit {
            for col in &mut result.columns { col.values.truncate(limit); }
            result.row_count = limit;
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::{LoadedColumn, LoadedTable};

    fn make_table(n: usize) -> Table {
        let cols = vec![
            LoadedColumn { name: "id".into(), cells: (0..n).map(|i| i as u64).collect(), row_count: n },
            LoadedColumn { name: "val".into(), cells: (0..n).map(|i| (i % 20) as u64).collect(), row_count: n },
            LoadedColumn { name: "grp".into(), cells: (0..n).map(|i| (i % 5) as u64).collect(), row_count: n },
        ];
        Table::from_loaded(LoadedTable { name: "t".into(), columns: cols, row_count: n })
    }

    #[test]
    fn classify_count_all() {
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT count(*) FROM t").unwrap()).unwrap();
        assert_eq!(classify_query(&q), QueryShape::CountAll);
    }

    #[test]
    fn classify_count_filter() {
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT count(*) FROM t WHERE id = 5").unwrap()).unwrap();
        assert_eq!(classify_query(&q), QueryShape::CountFilter);
    }

    #[test]
    fn classify_sum() {
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT sum(val) FROM t").unwrap()).unwrap();
        assert_eq!(classify_query(&q), QueryShape::SumCol);
    }

    #[test]
    fn classify_group_by() {
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT grp, count(*) FROM t GROUP BY grp").unwrap()).unwrap();
        assert_eq!(classify_query(&q), QueryShape::GroupByCount);
    }

    #[test]
    fn dispatch_count_all() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT count(*) FROM t").unwrap()).unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.columns[0].values[0], 100);
    }

    #[test]
    fn dispatch_count_filter() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT count(*) FROM t WHERE val = 5").unwrap()).unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.columns[0].values[0], 5); // 5 rows have val=5 (i=5,25,45,65,85)
    }

    #[test]
    fn dispatch_sum() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT sum(val) FROM t WHERE val > 15").unwrap()).unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        let sum = f64::from_bits(result.columns[0].values[0]);
        // val > 15 means val in {16,17,18,19}, each appears 5 times
        // sum = (16+17+18+19) * 5 = 70 * 5 = 350
        assert_eq!(sum, 350.0);
    }

    #[test]
    fn dispatch_group_by_count() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT grp, count(*) FROM t GROUP BY grp").unwrap()).unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.row_count, 5); // 5 groups
    }

    #[test]
    fn dispatch_group_by_order_limit() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT grp, count(*) FROM t GROUP BY grp ORDER BY grp LIMIT 3").unwrap()).unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.row_count, 3);
    }

    #[test]
    fn dispatch_min_max() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT max(val) FROM t").unwrap()).unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.columns[0].values[0], 19);
    }

    #[test]
    fn dispatch_count_distinct() {
        let table = make_table(100);
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT count(DISTINCT val) FROM t").unwrap()).unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        // val = i % 20, so 20 distinct values
        assert_eq!(result.columns[0].values[0], 20);
    }

    #[test]
    fn dispatch_select_star() {
        let table = make_table(10);
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT * FROM t LIMIT 5").unwrap()).unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.row_count, 5);
        assert_eq!(result.columns.len(), 3);
    }

    #[test]
    fn dispatch_select_column() {
        let table = make_table(10);
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT val FROM t WHERE id < 5").unwrap()).unwrap();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        assert_eq!(result.row_count, 5);
        assert_eq!(result.columns[0].name, "val");
    }

    #[test]
    fn large_filter_performance() {
        let n = 1_000_000;
        let table = make_table(n);
        let q = crate::sql::parser::parse(crate::sql::lexer::tokenize("SELECT count(*) FROM t WHERE val = 5").unwrap()).unwrap();
        let start = std::time::Instant::now();
        let result = execute_dispatched(&q, &table).unwrap().unwrap();
        let elapsed = start.elapsed();
        assert_eq!(result.columns[0].values[0], 50000);
        assert!(elapsed.as_millis() < 5, "took {}ms", elapsed.as_millis());
    }
}


// ---------------------------------------------------------------------------
// Arithmetic expression evaluation (for sum(col * (1 - col2)) etc.)
// ---------------------------------------------------------------------------

/// Evaluate an arithmetic expression on a single row, returning u64.
/// Supports: column refs, int literals, +, -, *, /
fn eval_arith_row(
    expr: &crate::sql::parser::Expr,
    columns: &[Vec<u64>],
    column_names: &[String],
    row_idx: usize,
) -> u64 {
    use crate::sql::parser::{Expr, Value};
    match expr {
        Expr::Column(name) => {
            let col_idx = resolve_col_name(name, &crate::datasource::table::Table {
                name: String::new(),
                columns: vec![],
                column_names: column_names.to_vec(),
                row_count: 0,
            }).unwrap_or(0);
            // Find column by name
            if let Some(idx) = column_names.iter().position(|n| n == name || n == name.split('.').nth(1).unwrap_or(name)) {
                return columns[idx][row_idx];
            }
            0
        }
        Expr::Literal(Value::Int(i)) => *i as u64,
        Expr::Literal(Value::Float(f)) => f.to_bits(),
        Expr::Binary { left, op, right } => {
            let l = eval_arith_row(left, columns, column_names, row_idx);
            let r = eval_arith_row(right, columns, column_names, row_idx);
            match op.as_str() {
                "+" => l.wrapping_add(r),
                "-" => l.wrapping_sub(r),
                "*" => l.wrapping_mul(r),
                "/" => if r == 0 { 0 } else { l / r },
                _ => 0,
            }
        }
        _ => 0,
    }
}

/// Sum an arithmetic expression over filtered rows.
/// For: SELECT sum(col * (1 - col2)) FROM t WHERE ...
pub fn sum_arithmetic(
    expr: &crate::sql::parser::Expr,
    columns: &[Vec<u64>],
    column_names: &[String],
    mask: &[bool],
) -> u64 {
    let mut sum: u64 = 0;
    for i in 0..mask.len() {
        if mask[i] {
            sum = sum.wrapping_add(eval_arith_row(expr, columns, column_names, i));
        }
    }
    (sum as f64).to_bits()
}

/// Evaluate CASE WHEN cond THEN val ELSE default END for a single row.
fn eval_case_row(
    whens: &[(crate::sql::parser::Expr, crate::sql::parser::Expr)],
    else_expr: Option<&crate::sql::parser::Expr>,
    columns: &[Vec<u64>],
    column_names: &[String],
    row_idx: usize,
) -> u64 {
    for (cond, result) in whens {
        // Evaluate condition — if the row matches, return the result
        let cond_val = eval_arith_row(cond, columns, column_names, row_idx);
        if cond_val != 0 {
            return eval_arith_row(result, columns, column_names, row_idx);
        }
    }
    if let Some(e) = else_expr {
        return eval_arith_row(e, columns, column_names, row_idx);
    }
    0
}
