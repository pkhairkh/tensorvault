//! # MERGE statement + TRY_CONVERT/TRY_CAST (Wave 10).
//!
//! MERGE: upsert operation that INSERTs new rows, UPDATEs matching rows,
//! and optionally DELETEs unmatched rows, in a single statement.
//! TRY_CONVERT/TRY_CAST: type conversion that returns NULL on failure
//! instead of raising an error.

use crate::engine::{QueryResult, ResultColumn};

/// A MERGE action: what to do for matched/unmatched rows.
#[derive(Debug, Clone)]
pub enum MergeAction {
    /// UPDATE SET col = val, ...
    Update(Vec<(String, String)>),
    /// INSERT (cols) VALUES (vals)
    Insert(Vec<String>, Vec<String>),
    /// DELETE matched rows.
    Delete,
}

/// A parsed MERGE statement.
#[derive(Debug, Clone)]
pub struct Merge {
    /// Target table name.
    pub target: String,
    /// Source: (table_name, join_column, value) rows to merge.
    /// For simplicity, the source is inline values rather than a subquery.
    pub source_rows: Vec<(String, Vec<String>)>,
    /// Join condition: target.col = source.col.
    pub join_target_col: String,
    pub join_source_col: String,
    /// Action when a match is found.
    pub when_matched: Option<MergeAction>,
    /// Action when no match is found (target row has no source).
    pub when_not_matched_by_source: Option<MergeAction>,
    /// Action when source has no target (new row to insert).
    pub when_not_matched_by_target: Option<MergeAction>,
}

/// Result of a MERGE: counts of inserted/updated/deleted rows.
#[derive(Debug, Clone, Default)]
pub struct MergeResult {
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
}

/// TRY_CONVERT: convert a string to a u64. Returns None on failure.
pub fn try_convert_to_u64(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok()
}

/// TRY_CONVERT: convert a string to an f64. Returns None on failure.
pub fn try_convert_to_f64(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok()
}

/// TRY_CONVERT: convert a string to a bool. Returns None on failure.
pub fn try_convert_to_bool(s: &str) -> Option<bool> {
    let trimmed = s.trim().to_lowercase();
    match trimmed.as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// TRY_CAST: alias for TRY_CONVERT. Converts a string to the specified
/// type name. Returns None on failure.
pub fn try_cast(s: &str, type_name: &str) -> Option<String> {
    match type_name.to_uppercase().as_str() {
        "INT" | "INTEGER" | "BIGINT" | "SMALLINT" | "TINYINT" => {
            try_convert_to_u64(s).map(|n| n.to_string())
        }
        "FLOAT" | "REAL" | "DOUBLE" | "DECIMAL" | "NUMERIC" => {
            try_convert_to_f64(s).map(|n| n.to_string())
        }
        "BIT" | "BOOLEAN" | "BOOL" => {
            try_convert_to_bool(s).map(|b| if b { "1".into() } else { "0".into() })
        }
        "VARCHAR" | "NVARCHAR" | "TEXT" => Some(s.to_string()),
        _ => None,
    }
}

/// IIF: immediate if. Returns true_val if condition is true, else false_val.
pub fn iif<'a>(condition: bool, true_val: &'a str, false_val: &'a str) -> &'a str {
    if condition { true_val } else { false_val }
}

/// Execute a MERGE against a QueryResult (representing the target table).
/// This is a simplified implementation that operates on QueryResult
/// rather than the live catalog — the engine's execute_merge method
/// will call this with the target table's data.
pub fn execute_merge(target: &mut QueryResult, merge: &Merge) -> MergeResult {
    let mut result = MergeResult::default();

    let target_col_idx = target.columns.iter().position(|c| c.name == merge.join_target_col);

    if target_col_idx.is_none() {
        return result;
    }
    let target_col_idx = target_col_idx.unwrap();

    // Track which target rows were matched.
    let mut matched_mask = vec![false; target.row_count];

    // Process source rows.
    for (source_val_str, _source_row) in &merge.source_rows {
        let source_val = parse_cell(source_val_str);

        // Find matching target row.
        let mut found_match = false;
        for row in 0..target.row_count {
            let target_val = target.columns[target_col_idx]
                .values
                .get(row)
                .copied()
                .unwrap_or(0);
            if target_val == source_val {
                matched_mask[row] = true;
                found_match = true;

                // Apply WHEN MATCHED action.
                if let Some(MergeAction::Update(assigns)) = &merge.when_matched {
                    for (col_name, val_str) in assigns {
                        if let Some(idx) = target.columns.iter().position(|c| c.name == *col_name) {
                            if row < target.columns[idx].values.len() {
                                target.columns[idx].values[row] = parse_cell(val_str);
                            }
                        }
                    }
                    result.updated += 1;
                }
                break;
            }
        }

        // If no match and WHEN NOT MATCHED BY TARGET, insert.
        if !found_match {
            if let Some(MergeAction::Insert(cols, vals)) = &merge.when_not_matched_by_target {
                let cols_to_set: Vec<usize> = if cols.is_empty() {
                    (0..target.columns.len()).collect()
                } else {
                    cols.iter()
                        .filter_map(|c| target.columns.iter().position(|tc| tc.name == *c))
                        .collect()
                };
                for (i, &col_idx) in cols_to_set.iter().enumerate() {
                    if i < vals.len() {
                        target.columns[col_idx].values.push(parse_cell(&vals[i]));
                    } else {
                        target.columns[col_idx].values.push(0);
                    }
                }
                target.row_count += 1;
                result.inserted += 1;
            }
        }
    }

    // Process WHEN NOT MATCHED BY SOURCE (delete unmatched target rows).
    // WHEN NOT MATCHED BY SOURCE means: target rows that have no matching
    // source row → these get the Delete action.
    if let Some(MergeAction::Delete) = &merge.when_not_matched_by_source {
        // keep_mask: keep matched rows, delete unmatched.
        let keep_mask = &matched_mask;
        let deleted = matched_mask.iter().filter(|&&m| !m).count();
        for col in &mut target.columns {
            let mut new_vals = Vec::with_capacity(target.row_count - deleted);
            for (i, &keep) in keep_mask.iter().enumerate() {
                if keep && i < col.values.len() {
                    new_vals.push(col.values[i]);
                }
            }
            col.values = new_vals;
        }
        target.row_count -= deleted;
        result.deleted = deleted;
    }

    result
}

fn parse_cell(s: &str) -> u64 {
    let trimmed = s.trim();
    if trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2 {
        use xxhash_rust::xxh3;
        return xxh3::xxh3_64(trimmed[1..trimmed.len() - 1].as_bytes());
    }
    if let Ok(n) = trimmed.parse::<i64>() {
        return n as u64;
    }
    if let Ok(n) = trimmed.parse::<u64>() {
        return n;
    }
    if let Ok(f) = trimmed.parse::<f64>() {
        return f.to_bits();
    }
    0
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(names: &[&str], cols: &[Vec<u64>]) -> QueryResult {
        let mut r = QueryResult::empty();
        for (i, name) in names.iter().enumerate() {
            r.push_column(ResultColumn { name: name.to_string(), values: cols[i].clone(), string_values: None })
                .unwrap();
        }
        r
    }

    #[test]
    fn try_convert_u64() {
        assert_eq!(try_convert_to_u64("42"), Some(42));
        assert_eq!(try_convert_to_u64("not a number"), None);
        assert_eq!(try_convert_to_u64("  100  "), Some(100));
    }

    #[test]
    fn try_convert_f64() {
        assert_eq!(try_convert_to_f64("3.14"), Some(3.14));
        assert_eq!(try_convert_to_f64("not a number"), None);
    }

    #[test]
    fn try_convert_bool() {
        assert_eq!(try_convert_to_bool("true"), Some(true));
        assert_eq!(try_convert_to_bool("1"), Some(true));
        assert_eq!(try_convert_to_bool("false"), Some(false));
        assert_eq!(try_convert_to_bool("0"), Some(false));
        assert_eq!(try_convert_to_bool("maybe"), None);
    }

    #[test]
    fn try_cast_int() {
        assert_eq!(try_cast("42", "INT"), Some("42".into()));
        assert_eq!(try_cast("abc", "INT"), None);
    }

    #[test]
    fn try_cast_float() {
        assert_eq!(try_cast("3.14", "FLOAT"), Some("3.14".into()));
        assert_eq!(try_cast("abc", "FLOAT"), None);
    }

    #[test]
    fn try_cast_bool() {
        assert_eq!(try_cast("true", "BOOLEAN"), Some("1".into()));
        assert_eq!(try_cast("false", "BOOLEAN"), Some("0".into()));
        assert_eq!(try_cast("maybe", "BOOLEAN"), None);
    }

    #[test]
    fn try_cast_string_passthrough() {
        assert_eq!(try_cast("hello", "VARCHAR"), Some("hello".into()));
    }

    #[test]
    fn iif_true() {
        assert_eq!(iif(true, "yes", "no"), "yes");
    }

    #[test]
    fn iif_false() {
        assert_eq!(iif(false, "yes", "no"), "no");
    }

    #[test]
    fn merge_update_existing() {
        let mut target = make_result(&["id", "val"], &[vec![1, 2, 3], vec![10, 20, 30]]);
        let merge = Merge {
            target: "t".into(),
            source_rows: vec![("2".into(), vec!["2".into(), "99".into()])],
            join_target_col: "id".into(),
            join_source_col: "id".into(),
            when_matched: Some(MergeAction::Update(vec![("val".into(), "99".into())])),
            when_not_matched_by_source: None,
            when_not_matched_by_target: None,
        };
        let result = execute_merge(&mut target, &merge);
        assert_eq!(result.updated, 1);
        assert_eq!(target.columns[1].values, vec![10, 99, 30]);
    }

    #[test]
    fn merge_insert_new() {
        let mut target = make_result(&["id", "val"], &[vec![1, 2], vec![10, 20]]);
        let merge = Merge {
            target: "t".into(),
            source_rows: vec![("3".into(), vec!["3".into(), "30".into()])],
            join_target_col: "id".into(),
            join_source_col: "id".into(),
            when_matched: None,
            when_not_matched_by_source: None,
            when_not_matched_by_target: Some(MergeAction::Insert(
                vec!["id".into(), "val".into()],
                vec!["3".into(), "30".into()],
            )),
        };
        let result = execute_merge(&mut target, &merge);
        assert_eq!(result.inserted, 1);
        assert_eq!(target.row_count, 3);
        assert_eq!(target.columns[0].values, vec![1, 2, 3]);
        assert_eq!(target.columns[1].values, vec![10, 20, 30]);
    }

    #[test]
    fn merge_delete_unmatched() {
        let mut target = make_result(&["id", "val"], &[vec![1, 2, 3], vec![10, 20, 30]]);
        let merge = Merge {
            target: "t".into(),
            source_rows: vec![("2".into(), vec!["2".into(), "20".into()])],
            join_target_col: "id".into(),
            join_source_col: "id".into(),
            when_matched: None,
            when_not_matched_by_source: Some(MergeAction::Delete),
            when_not_matched_by_target: None,
        };
        let result = execute_merge(&mut target, &merge);
        assert_eq!(result.deleted, 2);
        assert_eq!(target.row_count, 1);
        assert_eq!(target.columns[0].values, vec![2]);
    }

    #[test]
    fn merge_upsert_combined() {
        let mut target = make_result(&["id", "val"], &[vec![1, 2], vec![10, 20]]);
        let merge = Merge {
            target: "t".into(),
            source_rows: vec![
                ("2".into(), vec!["2".into(), "99".into()]),
                ("3".into(), vec!["3".into(), "30".into()]),
            ],
            join_target_col: "id".into(),
            join_source_col: "id".into(),
            when_matched: Some(MergeAction::Update(vec![("val".into(), "99".into())])),
            when_not_matched_by_source: None,
            when_not_matched_by_target: Some(MergeAction::Insert(
                vec!["id".into(), "val".into()],
                vec!["3".into(), "30".into()],
            )),
        };
        let result = execute_merge(&mut target, &merge);
        assert_eq!(result.updated, 1);
        assert_eq!(result.inserted, 1);
        assert_eq!(target.row_count, 3);
        assert_eq!(target.columns[0].values, vec![1, 2, 3]);
        assert_eq!(target.columns[1].values, vec![10, 99, 30]);
    }
}
