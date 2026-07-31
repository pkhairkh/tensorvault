use std::time::Instant;
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

fn main() {
    let mut engine = QueryEngine::new();
    let loaded = read_tpch_csv("/tmp/tpch_lineitem.csv", "lineitem").expect("load");
    engine.register_table(Table::from_loaded(loaded));
    println!("loaded");

    // Measure just parse + execute (from_catalog is inside execute)
    let q = "SELECT count(*) FROM lineitem";
    let _ = engine.execute_tpch(q).unwrap();
    
    let t = Instant::now();
    let _ = engine.execute_tpch(q).unwrap();
    println!("count(*): {} ms", t.elapsed().as_millis());

    // Measure Q1
    let q1 = "SELECT l_returnflag, l_linestatus, sum(l_quantity) AS sum_qty, sum(l_extendedprice) AS sum_base_price, sum(l_extendedprice * (1 - l_discount)) AS sum_disc_price, sum(l_extendedprice * (1 - l_discount) * (1 + l_tax)) AS sum_charge, avg(l_quantity) AS avg_qty, avg(l_extendedprice) AS avg_price, avg(l_discount) AS avg_disc, count(*) AS count_order FROM lineitem WHERE l_shipdate <= date '1998-09-02' GROUP BY l_returnflag, l_linestatus ORDER BY l_returnflag, l_linestatus";
    let _ = engine.execute_tpch(q1).unwrap();
    let t = Instant::now();
    let _ = engine.execute_tpch(q1).unwrap();
    println!("Q1: {} ms", t.elapsed().as_millis());
}
