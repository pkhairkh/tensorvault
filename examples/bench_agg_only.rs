//! Measure ONLY the fused aggregation, no from_catalog, no join, no clone.
use std::time::Instant;
use std::sync::Arc;
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

fn main() {
    let mut engine = QueryEngine::new();
    let loaded = read_tpch_csv("/tmp/tpch_lineitem.csv", "lineitem").expect("load");
    let n = loaded.row_count;
    engine.register_table(Table::from_loaded(loaded));
    println!("loaded {} rows", n);

    // Warmup
    let _ = engine.execute_tpch("SELECT count(*) FROM lineitem").unwrap();

    // Measure simple count (no aggregation, just scan)
    let t = Instant::now();
    let _ = engine.execute_tpch("SELECT count(*) FROM lineitem").unwrap();
    println!("count(*): {} ms", t.elapsed().as_millis());

    // Measure sum(l_quantity) — single aggregate, no GROUP BY
    let t = Instant::now();
    let _ = engine.execute_tpch("SELECT sum(l_quantity) FROM lineitem").unwrap();
    println!("sum(l_quantity): {} ms", t.elapsed().as_millis());

    // Measure Q1 (GROUP BY + 10 aggregates)
    let q1 = "SELECT l_returnflag, l_linestatus, sum(l_quantity) AS sum_qty, sum(l_extendedprice) AS sum_base_price, sum(l_extendedprice * (1 - l_discount)) AS sum_disc_price, sum(l_extendedprice * (1 - l_discount) * (1 + l_tax)) AS sum_charge, avg(l_quantity) AS avg_qty, avg(l_extendedprice) AS avg_price, avg(l_discount) AS avg_disc, count(*) AS count_order FROM lineitem WHERE l_shipdate <= date '1998-09-02' GROUP BY l_returnflag, l_linestatus ORDER BY l_returnflag, l_linestatus";
    let t = Instant::now();
    let _ = engine.execute_tpch(q1).unwrap();
    println!("Q1 full: {} ms", t.elapsed().as_millis());

    // Measure Q6 (single table, filter + sum)
    let q6 = "SELECT sum(l_extendedprice * l_discount) AS revenue FROM lineitem WHERE l_shipdate >= date '1994-01-01' AND l_shipdate < date '1995-01-01' AND l_discount >= 0.05 AND l_discount <= 0.07 AND l_quantity < 24";
    let t = Instant::now();
    let _ = engine.execute_tpch(q6).unwrap();
    println!("Q6: {} ms", t.elapsed().as_millis());
}
