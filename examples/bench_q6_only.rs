//! Q6-only benchmark for Wave 13 verification.
//! Loads lineitem, runs Q6 best-of-5, prints ms.

use std::time::Instant;
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

const Q6: &str = "SELECT sum(l_extendedprice * l_discount) AS revenue FROM lineitem WHERE l_shipdate >= date '1994-01-01' AND l_shipdate < date '1995-01-01' AND l_discount >= 0.05 AND l_discount <= 0.07 AND l_quantity < 24";

fn main() {
    println!("loading lineitem...");
    let t0 = Instant::now();
    let mut engine = QueryEngine::new();
    let path = "/tmp/tpch_lineitem.csv";
    let loaded = read_tpch_csv(path, "lineitem").expect("load lineitem");
    let n = loaded.row_count;
    engine.register_table(Table::from_loaded(loaded));
    println!("loaded lineitem in {:?} ({} rows)", t0.elapsed(), n);

    // warmup
    let _ = engine.execute_tpch(Q6).expect("Q6 warmup");

    let mut best = u128::MAX;
    for i in 0..5 {
        let t = Instant::now();
        let res = engine.execute_tpch(Q6).expect("Q6");
        let ms = t.elapsed().as_millis();
        println!("Q6 run {}: {} ms, {} rows", i, ms, res.row_count);
        if ms < best { best = ms; }
    }
    println!("Q6 best_ms: {}", best);
}
