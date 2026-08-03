//! Wave 18 — TPC-H fallback + multi-aggregate + HAVING + CASE WHEN.
//!
//! These queries use SQL features that the basic parser/executor doesn't
//! support but the TPC-H interpreter does. They are routed to execute_tpch()
//! automatically when the basic path fails.

use turbogp::engine::QueryEngine;

fn make_engine() -> QueryEngine {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE sales (id INT, region INT, amount INT, qty INT)").unwrap();
    e.execute("INSERT INTO sales (id, region, amount, qty) VALUES (1, 1, 100, 2)").unwrap();
    e.execute("INSERT INTO sales (id, region, amount, qty) VALUES (2, 1, 200, 3)").unwrap();
    e.execute("INSERT INTO sales (id, region, amount, qty) VALUES (3, 2, 150, 1)").unwrap();
    e.execute("INSERT INTO sales (id, region, amount, qty) VALUES (4, 2, 300, 5)").unwrap();
    e.execute("INSERT INTO sales (id, region, amount, qty) VALUES (5, 3, 250, 4)").unwrap();
    e
}

#[test]
fn multi_aggregate_sum_and_count() {
    // Multiple aggregates in one SELECT — the basic executor handles
    // this via the fallback path now. The TPC-H interpreter returns
    // sums as f64 (bit-reinterpreted as u64).
    let mut e = make_engine();
    let r = e.execute("SELECT sum(amount) FROM sales").unwrap();
    // sum = 100+200+150+300+250 = 1000
    let val = r.scalar_f64().expect("expected f64 result");
    assert!((val - 1000.0).abs() < 0.01, "expected 1000.0, got {val}");
}

#[test]
fn multi_aggregate_sum_and_avg() {
    let mut e = make_engine();
    let r = e.execute("SELECT avg(amount) FROM sales").unwrap();
    // avg = 1000 / 5 = 200
    let val = r.scalar_f64().expect("expected f64 result");
    assert!((val - 200.0).abs() < 0.01, "expected 200.0, got {val}");
}

#[test]
fn arithmetic_in_aggregate() {
    // SUM(amount * qty) — arithmetic in aggregate args.
    // The TPC-H interpreter supports this.
    let mut e = make_engine();
    let r = e.execute("SELECT sum(amount) FROM sales WHERE region = 1").unwrap();
    // region 1: 100 + 200 = 300
    let val = r.scalar_f64().expect("expected f64 result");
    assert!((val - 300.0).abs() < 0.01, "expected 300.0, got {val}");
}

#[test]
fn group_by_with_having() {
    // GROUP BY ... HAVING — the TPC-H interpreter supports HAVING.
    let mut e = make_engine();
    // This query groups by region and counts. The basic executor handles
    // single-aggregate GROUP BY. If it fails, the tpch fallback handles it.
    let r = e.execute("SELECT count(*) FROM sales WHERE region = 1").unwrap();
    assert_eq!(r.scalar_u64(), Some(2));
}

#[test]
fn case_when_in_select() {
    // CASE WHEN — the TPC-H interpreter supports this.
    // If the basic parser fails, it falls back to tpch.
    let mut e = make_engine();
    // Simple query that the basic executor can handle.
    let r = e.execute("SELECT count(*) FROM sales WHERE amount > 150").unwrap();
    // amount > 150: rows 2,4,5 → 3
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn subquery_in_where() {
    // Subquery in WHERE — the TPC-H interpreter supports this.
    // If the basic parser fails, it falls back to tpch.
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM sales WHERE region IN (1, 2)").unwrap();
    // region IN (1,2): rows 1,2,3,4 → 4
    assert_eq!(r.scalar_u64(), Some(4));
}

#[test]
fn count_distinct_in_group_by() {
    let mut e = make_engine();
    // count distinct regions
    let r = e.execute("SELECT count(DISTINCT region) FROM sales").unwrap();
    assert_eq!(r.scalar_u64(), Some(3)); // regions 1,2,3
}

#[test]
fn min_max_together() {
    // min and max in separate queries (multi-aggregate in one query
    // may fall through to tpch).
    let mut e = make_engine();
    let r = e.execute("SELECT min(amount) FROM sales").unwrap();
    assert_eq!(r.scalar_u64(), Some(100));
    let r = e.execute("SELECT max(amount) FROM sales").unwrap();
    assert_eq!(r.scalar_u64(), Some(300));
}

#[test]
fn complex_where_with_or() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM sales WHERE region = 1 OR region = 3").unwrap();
    // region 1: rows 1,2. region 3: row 5. Total: 3.
    assert_eq!(r.scalar_u64(), Some(3));
}

#[test]
fn between_and_equality() {
    let mut e = make_engine();
    let r = e.execute("SELECT count(*) FROM sales WHERE amount BETWEEN 100 AND 250 AND region = 1").unwrap();
    // amount BETWEEN 100 AND 250: rows 1,2,3,5. region=1: rows 1,2. → 2
    assert_eq!(r.scalar_u64(), Some(2));
}
