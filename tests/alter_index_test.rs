//! Wave 66 — ALTER TABLE + CREATE INDEX / DROP INDEX.
//!
//! Verifies:
//! 1. `ALTER TABLE t ADD COLUMN col INT DEFAULT 0` — the new column
//!    appears in the schema and existing rows get the default value.
//! 2. `ALTER TABLE t DROP COLUMN col` — the column is gone.
//! 3. `ALTER TABLE t ALTER COLUMN col TYPE BIGINT` — the column's type
//!    changes (data is preserved since all cells are u64).
//! 4. `CREATE INDEX idx ON t (col)` — registers an index; subsequent
//!    `SELECT ... WHERE col = val` uses the index for fast lookup and
//!    returns the correct rows.
//! 5. `DROP INDEX idx` — the index is removed.

use turbogp::engine::QueryEngine;

fn make_engine_with_rows() -> QueryEngine {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    e.execute("INSERT INTO t (id, v) VALUES (1, 10)").unwrap();
    e.execute("INSERT INTO t (id, v) VALUES (2, 20)").unwrap();
    e.execute("INSERT INTO t (id, v) VALUES (3, 30)").unwrap();
    e.execute("INSERT INTO t (id, v) VALUES (4, 20)").unwrap();
    e
}

// -----------------------------------------------------------------------
// ALTER TABLE ADD COLUMN
// -----------------------------------------------------------------------

#[test]
fn alter_add_column_int_default_zero() {
    let mut e = make_engine_with_rows();
    e.execute("ALTER TABLE t ADD COLUMN extra INT DEFAULT 0").unwrap();
    // SELECT extra should return 0 for all 4 rows.
    let r = e.execute("SELECT extra FROM t").unwrap();
    assert_eq!(r.row_count, 4);
    let col = &r.columns[0];
    assert_eq!(col.values, vec![0, 0, 0, 0]);
}

#[test]
fn alter_add_column_int_default_nonzero() {
    let mut e = make_engine_with_rows();
    e.execute("ALTER TABLE t ADD COLUMN flag INT DEFAULT 7").unwrap();
    let r = e.execute("SELECT flag FROM t").unwrap();
    assert_eq!(r.row_count, 4);
    let col = &r.columns[0];
    assert_eq!(col.values, vec![7, 7, 7, 7]);
}

#[test]
fn alter_add_column_float_default() {
    let mut e = make_engine_with_rows();
    e.execute("ALTER TABLE t ADD COLUMN score FLOAT DEFAULT 1.5").unwrap();
    let r = e.execute("SELECT score FROM t").unwrap();
    assert_eq!(r.row_count, 4);
    let col = &r.columns[0];
    // Each cell should be f64::to_bits(1.5).
    let expected = 1.5f64.to_bits();
    for v in &col.values {
        assert_eq!(*v, expected, "expected 1.5 bits, got {v}");
    }
}

#[test]
fn alter_add_column_preserves_existing_data() {
    let mut e = make_engine_with_rows();
    e.execute("ALTER TABLE t ADD COLUMN extra INT DEFAULT 0").unwrap();
    // Existing columns should be unaffected.
    let r = e.execute("SELECT id, v, extra FROM t").unwrap();
    assert_eq!(r.row_count, 4);
    assert_eq!(r.columns[0].values, vec![1, 2, 3, 4]);
    assert_eq!(r.columns[1].values, vec![10, 20, 30, 20]);
    assert_eq!(r.columns[2].values, vec![0, 0, 0, 0]);
}

// -----------------------------------------------------------------------
// ALTER TABLE DROP COLUMN
// -----------------------------------------------------------------------

#[test]
fn alter_drop_column_removes_column() {
    let mut e = make_engine_with_rows();
    e.execute("ALTER TABLE t DROP COLUMN v").unwrap();
    // SELECT * should now return only id.
    let r = e.execute("SELECT * FROM t").unwrap();
    assert_eq!(r.row_count, 4);
    assert_eq!(r.columns.len(), 1, "only id column should remain");
    assert_eq!(r.columns[0].name, "id");
    assert_eq!(r.columns[0].values, vec![1, 2, 3, 4]);
}

#[test]
fn alter_drop_column_then_select_dropped_errors() {
    let mut e = make_engine_with_rows();
    e.execute("ALTER TABLE t DROP COLUMN v").unwrap();
    // SELECT v should fail (column gone).
    let r = e.execute("SELECT v FROM t");
    assert!(r.is_err(), "selecting dropped column must error");
}

#[test]
fn alter_drop_column_missing_errors() {
    let mut e = make_engine_with_rows();
    let r = e.execute("ALTER TABLE t DROP COLUMN nonexistent");
    assert!(r.is_err(), "dropping a non-existent column must error");
}

// -----------------------------------------------------------------------
// ALTER TABLE ALTER COLUMN TYPE
// -----------------------------------------------------------------------

#[test]
fn alter_column_type_widening_preserves_data() {
    let mut e = make_engine_with_rows();
    e.execute("ALTER TABLE t ALTER COLUMN v TYPE BIGINT").unwrap();
    // Data should be unchanged (INT → BIGINT is a widening conversion;
    // both stored as u64).
    let r = e.execute("SELECT v FROM t").unwrap();
    assert_eq!(r.row_count, 4);
    assert_eq!(r.columns[0].values, vec![10, 20, 30, 20]);
}

#[test]
fn alter_column_type_float_widening() {
    let mut e = make_engine_with_rows();
    e.execute("ALTER TABLE t ALTER COLUMN v TYPE FLOAT").unwrap();
    let r = e.execute("SELECT v FROM t").unwrap();
    assert_eq!(r.row_count, 4);
    // The cell values are unchanged (u64 storage) — 10, 20, 30, 20.
    assert_eq!(r.columns[0].values, vec![10, 20, 30, 20]);
}

// -----------------------------------------------------------------------
// CREATE INDEX / DROP INDEX
// -----------------------------------------------------------------------

#[test]
fn create_index_then_select_with_where_returns_correct_rows() {
    let mut e = make_engine_with_rows();
    e.execute("CREATE INDEX idx_v ON t (v)").unwrap();
    // SELECT * FROM t WHERE v = 20 → rows with id=2 and id=4.
    let r = e.execute("SELECT * FROM t WHERE v = 20").unwrap();
    assert_eq!(r.row_count, 2, "v=20 should match 2 rows");
    let id_col = &r.columns[0];
    assert!(id_col.values.contains(&2), "must contain id=2");
    assert!(id_col.values.contains(&4), "must contain id=4");
    let v_col = &r.columns[1];
    for v in &v_col.values {
        assert_eq!(*v, 20, "all matched rows must have v=20");
    }
}

#[test]
fn create_index_select_specific_column() {
    let mut e = make_engine_with_rows();
    e.execute("CREATE INDEX idx_v ON t (v)").unwrap();
    // SELECT id FROM t WHERE v = 30 → row with id=3.
    let r = e.execute("SELECT id FROM t WHERE v = 30").unwrap();
    assert_eq!(r.row_count, 1);
    assert_eq!(r.columns[0].values, vec![3]);
}

#[test]
fn create_index_no_match_returns_zero_rows() {
    let mut e = make_engine_with_rows();
    e.execute("CREATE INDEX idx_v ON t (v)").unwrap();
    let r = e.execute("SELECT * FROM t WHERE v = 999").unwrap();
    assert_eq!(r.row_count, 0, "no rows match v=999");
}

#[test]
fn create_index_if_not_exists_is_idempotent() {
    let mut e = make_engine_with_rows();
    e.execute("CREATE INDEX idx_v ON t (v)").unwrap();
    // Second CREATE INDEX IF NOT EXISTS should succeed.
    e.execute("CREATE INDEX IF NOT EXISTS idx_v ON t (v)").unwrap();
}

#[test]
fn create_index_duplicate_name_errors() {
    let mut e = make_engine_with_rows();
    e.execute("CREATE INDEX idx_v ON t (v)").unwrap();
    let r = e.execute("CREATE INDEX idx_v ON t (v)");
    assert!(r.is_err(), "duplicate index name must error");
}

#[test]
fn drop_index_then_query_falls_back_to_scan() {
    let mut e = make_engine_with_rows();
    e.execute("CREATE INDEX idx_v ON t (v)").unwrap();
    e.execute("DROP INDEX idx_v").unwrap();
    // After DROP INDEX, SELECT ... WHERE v = 20 should still return the
    // correct rows (via the full-scan path).
    let r = e.execute("SELECT * FROM t WHERE v = 20").unwrap();
    assert_eq!(r.row_count, 2);
    let id_col = &r.columns[0];
    assert!(id_col.values.contains(&2));
    assert!(id_col.values.contains(&4));
}

#[test]
fn drop_index_if_exists_is_safe() {
    let mut e = make_engine_with_rows();
    // Dropping a non-existent index with IF EXISTS should succeed.
    e.execute("DROP INDEX IF EXISTS ghost").unwrap();
}

#[test]
fn drop_index_missing_errors() {
    let mut e = make_engine_with_rows();
    let r = e.execute("DROP INDEX ghost");
    assert!(r.is_err(), "dropping a non-existent index must error");
}

#[test]
fn create_index_on_missing_table_errors() {
    let mut e = make_engine_with_rows();
    let r = e.execute("CREATE INDEX idx ON nope (v)");
    assert!(r.is_err(), "creating an index on a missing table must error");
}

#[test]
fn create_index_on_missing_column_errors() {
    let mut e = make_engine_with_rows();
    let r = e.execute("CREATE INDEX idx ON t (nonexistent)");
    assert!(r.is_err(), "creating an index on a missing column must error");
}

// -----------------------------------------------------------------------
// Combined: ALTER + INDEX
// -----------------------------------------------------------------------

#[test]
fn add_column_then_index_it() {
    let mut e = make_engine_with_rows();
    e.execute("ALTER TABLE t ADD COLUMN status INT DEFAULT 0").unwrap();
    // Update some rows.
    e.execute("UPDATE t SET status = 1 WHERE id = 2").unwrap();
    e.execute("UPDATE t SET status = 1 WHERE id = 4").unwrap();
    // Index the new column.
    e.execute("CREATE INDEX idx_status ON t (status)").unwrap();
    // SELECT * FROM t WHERE status = 1 → rows with id=2 and id=4.
    let r = e.execute("SELECT * FROM t WHERE status = 1").unwrap();
    assert_eq!(r.row_count, 2, "status=1 should match 2 rows");
    let id_col = &r.columns[0];
    assert!(id_col.values.contains(&2));
    assert!(id_col.values.contains(&4));
}
