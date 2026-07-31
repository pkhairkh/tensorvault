//! Quick benchmark: run TPC-H Q5 only (3 runs) to measure hash join improvement.
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;
use std::time::Instant;

const Q5: &str = "SELECT n_name, sum(l_extendedprice * (1 - l_discount)) AS revenue FROM customer, orders, lineitem, supplier, nation, region WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey AND l_suppkey = s_suppkey AND c_nationkey = s_nationkey AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'ASIA' AND o_orderdate >= date '1994-01-01' AND o_orderdate < date '1995-01-01' GROUP BY n_name ORDER BY revenue DESC";

fn main() {
    println!("=== turboGP TPC-H Q5 hash-join benchmark ===");
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).unwrap();
        engine.register_table(Table::from_loaded(loaded));
    }
    println!("Loaded. Running Q5 x3...\n");

    // Warmup
    let _ = engine.execute_tpch(Q5);

    for i in 0..3 {
        let start = Instant::now();
        let r = engine.execute_tpch(Q5).unwrap();
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        println!("  Q5 run {}: {:.1}ms ({} rows)", i + 1, ms, r.row_count);
    }
}
