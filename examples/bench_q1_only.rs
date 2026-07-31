use std::time::Instant;
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

const Q1: &str = "SELECT l_returnflag, l_linestatus, sum(l_quantity) AS sum_qty, sum(l_extendedprice) AS sum_base_price, sum(l_extendedprice * (1 - l_discount)) AS sum_disc_price, sum(l_extendedprice * (1 - l_discount) * (1 + l_tax)) AS sum_charge, avg(l_quantity) AS avg_qty, avg(l_extendedprice) AS avg_price, avg(l_discount) AS avg_disc, count(*) AS count_order FROM lineitem WHERE l_shipdate <= date '1998-09-02' GROUP BY l_returnflag, l_linestatus ORDER BY l_returnflag, l_linestatus";

fn main() {
    let mut engine = QueryEngine::new();
    let loaded = read_tpch_csv("/tmp/tpch_lineitem.csv", "lineitem").expect("load");
    engine.register_table(Table::from_loaded(loaded));
    let _ = engine.execute_tpch(Q1).expect("warmup");
    let mut best = u128::MAX;
    for i in 0..5 {
        let t = Instant::now();
        let res = engine.execute_tpch(Q1).expect("Q1");
        let ms = t.elapsed().as_millis();
        println!("Q1 run {}: {} ms, {} rows", i, ms, res.row_count);
        if ms < best { best = ms; }
    }
    println!("Q1 best_ms: {}", best);
}
