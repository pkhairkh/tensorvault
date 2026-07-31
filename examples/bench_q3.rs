//! Benchmark: run TPC-H Q3 only (3 runs) for profiling.
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;
use std::time::Instant;

const Q3: &str = "SELECT l_orderkey, sum(l_extendedprice * (1 - l_discount)) AS revenue, o_orderdate, o_shippriority FROM customer, orders, lineitem WHERE c_mktsegment = 'BUILDING' AND c_custkey = o_custkey AND l_orderkey = o_orderkey AND o_orderdate < date '1995-03-15' AND l_shipdate > date '1995-03-15' GROUP BY l_orderkey, o_orderdate, o_shippriority ORDER BY revenue DESC, o_orderdate LIMIT 10";

fn main() {
    println!("=== turboGP TPC-H Q3 benchmark ===");
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).unwrap();
        engine.register_table(Table::from_loaded(loaded));
    }
    println!("Loaded. Running Q3 x3...\n");

    let _ = engine.execute_tpch(Q3);

    for i in 0..3 {
        let start = Instant::now();
        let r = engine.execute_tpch(Q3).unwrap();
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        println!("  Q3 run {}: {:.1}ms ({} rows)", i + 1, ms, r.row_count);
    }
}
