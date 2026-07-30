//! # CSV reader — load `.csv` files into u64 columns.
//!
//! A minimal CSV reader using only `std::fs`. Numeric columns are
//! parsed as `i64` and cast to `u64`; non-numeric columns are hashed
//! with xxh3 so the engine can still filter on equality (the same
//! lossy contract as [`super::parquet`] string columns).
//!
//! ## Limitations
//!
//! - No quoted-field handling. Fields containing commas, embedded
//!   newlines, or quote characters will be mis-split. This is fine for
//!   the ClickBench / TPC-H CSV exports, which use simple
//!   comma-separated values without quoting.
//! - No type inference beyond "all values parse as i64 ⇒ numeric,
//!   else hash". Float columns are hashed. (Use Parquet for float
//!   data.)
//! - Empty values are encoded as `0u64` in numeric columns and as
//!   `xxh3_64(b"")` in hashed columns.
//!
//! ## Why not `arrow-csv`
//!
//! The `arrow-csv` crate (transitively pulled in by `arrow = "55"`)
//! would do this in five lines, but turboGP deliberately keeps the
//! CSV path dependency-light: CSV is the lowest-common-denominator
//! format and the reader should remain auditable without pulling in
//! arrow's full CSV parser. Parquet, by contrast, has no simple
//! implementation and gets the full `parquet` crate.

use crate::datasource::parquet::{LoadedColumn, LoadedTable};
use std::error::Error;
use std::fs;
use xxhash_rust::xxh3;

/// Read a CSV file and return columns as u64 cells.
///
/// If `has_header` is true, the first row is treated as column names;
/// otherwise columns are named `col_0`, `col_1`, …
///
/// Each column is independently typed:
///
/// - If every value in the column parses as `i64`, the column is
///   numeric: each value is cast to `u64` (`value as u64`, which
///   bit-reinterprets negatives).
/// - Otherwise the column is hashed: each value's bytes are passed to
///   `xxh3_64`.
///
/// # Errors
///
/// Returns an error if the file cannot be read or if the rows have
/// inconsistent column counts.
pub fn read_csv(path: &str, has_header: bool) -> Result<LoadedTable, Box<dyn Error>> {
    let contents = fs::read_to_string(path)?;

    // Split into trimmed lines. We accept both `\n` and `\r\n` line
    // endings (trim_end_matches handles the latter). Empty lines are
    // skipped — they would otherwise create phantom zero-column rows.
    let mut lines: Vec<&str> = Vec::new();
    for line in contents.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        lines.push(line);
    }

    if lines.is_empty() {
        return Ok(LoadedTable {
            name: LoadedTable::name_from_path(path),
            columns: Vec::new(),
            row_count: 0,
        });
    }

    // Header (optional) + data rows.
    let (column_names, data_rows) = if has_header {
        let header = lines[0].split(',').map(|s| s.to_string()).collect::<Vec<_>>();
        (header, &lines[1..])
    } else {
        let ncols = lines[0].split(',').count();
        let names = (0..ncols).map(|i| format!("col_{i}")).collect::<Vec<_>>();
        (names, &lines[..])
    };

    if column_names.is_empty() {
        return Err("CSV has zero columns".into());
    }

    let ncols = column_names.len();

    // Parse rows into a Vec<Vec<&str>> (column-major would require
    // two passes; row-major is fine for our sizes).
    let mut parsed_rows: Vec<Vec<&str>> = Vec::with_capacity(data_rows.len());
    for (row_idx, line) in data_rows.iter().enumerate() {
        let row: Vec<&str> = line.split(',').collect();
        if row.len() != ncols {
            return Err(format!(
                "CSV row {} has {} fields, expected {}",
                row_idx + if has_header { 1 } else { 0 },
                row.len(),
                ncols
            )
            .into());
        }
        parsed_rows.push(row);
    }

    let row_count = parsed_rows.len();
    let mut columns: Vec<LoadedColumn> = Vec::with_capacity(ncols);

    for (col_idx, name) in column_names.iter().enumerate() {
        // First pass: try to parse every value in this column as i64.
        let mut as_i64: Vec<i64> = Vec::with_capacity(row_count);
        let mut all_numeric = true;
        for row in &parsed_rows {
            let v = row[col_idx];
            match v.parse::<i64>() {
                Ok(n) => as_i64.push(n),
                Err(_) => {
                    all_numeric = false;
                    break;
                }
            }
        }

        let cells: Vec<u64> = if all_numeric {
            // Numeric column: cast i64 → u64 (bit-reinterpret).
            as_i64.into_iter().map(|v| v as u64).collect()
        } else {
            // Non-numeric column: hash every value with xxh3_64.
            parsed_rows.iter().map(|row| xxh3::xxh3_64(row[col_idx].as_bytes())).collect()
        };

        columns.push(LoadedColumn { name: name.clone(), cells, row_count });
    }

    Ok(LoadedTable { name: LoadedTable::name_from_path(path), columns, row_count })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    /// Write `contents` to a temp file and return its path.
    fn write_tmp(contents: &str) -> (NamedTempFile, String) {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        std::fs::write(&path, contents).expect("write");
        (tmp, path)
    }

    /// Numeric CSV with header parses to i64-as-u64 cells.
    #[test]
    fn read_csv_numeric_with_header() {
        let (_tmp, path) = write_tmp("id,value\n1,10\n2,20\n3,30\n");
        let table = read_csv(&path, true).expect("read");

        assert_eq!(table.row_count, 3);
        assert_eq!(table.columns.len(), 2);
        assert_eq!(table.columns[0].name, "id");
        assert_eq!(table.columns[1].name, "value");
        assert_eq!(table.columns[0].cells, vec![1u64, 2, 3]);
        assert_eq!(table.columns[1].cells, vec![10u64, 20, 30]);
    }

    /// Numeric CSV without header gets synthetic `col_N` names.
    #[test]
    fn read_csv_numeric_no_header() {
        let (_tmp, path) = write_tmp("1,10\n2,20\n");
        let table = read_csv(&path, false).expect("read");

        assert_eq!(table.row_count, 2);
        assert_eq!(table.columns.len(), 2);
        assert_eq!(table.columns[0].name, "col_0");
        assert_eq!(table.columns[1].name, "col_1");
        assert_eq!(table.columns[0].cells, vec![1u64, 2]);
    }

    /// A column with any non-numeric value is hashed.
    #[test]
    fn read_csv_mixed_column_is_hashed() {
        let (_tmp, path) = write_tmp("label\nfoo\nfoo\nbar\n");
        let table = read_csv(&path, true).expect("read");

        assert_eq!(table.row_count, 3);
        let col = &table.columns[0];
        assert_eq!(col.cells.len(), 3);
        // First two cells hash the same string → equal.
        assert_eq!(col.cells[0], col.cells[1]);
        // Third cell hashes a different string → not equal.
        assert_ne!(col.cells[0], col.cells[2]);
        // And the hash matches xxh3_64("foo").
        assert_eq!(col.cells[0], xxh3::xxh3_64(b"foo"));
        assert_eq!(col.cells[2], xxh3::xxh3_64(b"bar"));
    }

    /// Negative integers are bit-reinterpreted as large u64.
    #[test]
    fn read_csv_negative_values() {
        let (_tmp, path) = write_tmp("v\n-1\n-2\n0\n");
        let table = read_csv(&path, true).expect("read");

        assert_eq!(table.columns[0].cells[0], (-1i64) as u64);
        assert_eq!(table.columns[0].cells[1], (-2i64) as u64);
        assert_eq!(table.columns[0].cells[2], 0u64);
    }

    /// Inconsistent column counts return an error.
    #[test]
    fn read_csv_inconsistent_columns_errors() {
        let (_tmp, path) = write_tmp("a,b\n1,2\n3,4,5\n");
        let err = read_csv(&path, true).unwrap_err();
        assert!(err.to_string().contains("expected 2"), "got: {err}");
    }

    /// Empty file → empty table.
    #[test]
    fn read_csv_empty_file() {
        let (_tmp, path) = write_tmp("");
        let table = read_csv(&path, true).expect("read");
        assert_eq!(table.row_count, 0);
        assert!(table.columns.is_empty());
    }

    /// Blank lines (including trailing newlines) are skipped.
    #[test]
    fn read_csv_skips_blank_lines() {
        let (_tmp, path) = write_tmp("id\n1\n\n2\n\n");
        let table = read_csv(&path, true).expect("read");
        assert_eq!(table.row_count, 2);
        assert_eq!(table.columns[0].cells, vec![1u64, 2]);
    }

    /// `\r\n` line endings are handled.
    #[test]
    fn read_csv_handles_crlf() {
        let (_tmp, path) = write_tmp("id,value\r\n1,10\r\n2,20\r\n");
        let table = read_csv(&path, true).expect("read");
        assert_eq!(table.row_count, 2);
        assert_eq!(table.columns[0].cells, vec![1u64, 2]);
        assert_eq!(table.columns[1].cells, vec![10u64, 20]);
    }

    /// A CSV where one column is numeric and another is hashed
    /// produces mixed-type columns in the same table.
    #[test]
    fn read_csv_mixed_columns() {
        let (_tmp, path) = write_tmp("id,name\n1,foo\n2,bar\n");
        let table = read_csv(&path, true).expect("read");
        assert_eq!(table.columns[0].cells, vec![1u64, 2]);
        assert_eq!(table.columns[1].cells[0], xxh3::xxh3_64(b"foo"));
        assert_eq!(table.columns[1].cells[1], xxh3::xxh3_64(b"bar"));
    }
}
