//! Benchmark: run TPC-H Q10 only (warmup + 3 runs) to verify W25 arena
//! allocator does not regress 4-way join queries.
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;
use std::time::Instant;

const Q10: &str = "SELECT c_custkey, c_name, sum(l_extendedprice * (1 - l_discount)) AS revenue, c_acctbal, n_name, c_address, c_phone, c_comment FROM customer, orders, lineitem, nation WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey AND o_orderdate >= date '1993-10-01' AND o_orderdate < date '1994-01-01' AND l_returnflag = 'R' AND c_nationkey = n_nationkey GROUP BY c_custkey, c_name, c_acctbal, n_name, c_address, c_phone, c_comment ORDER BY revenue DESC LIMIT 20";

fn main() {
    println!("=== turboGP TPC-H Q10 benchmark (W25 arena verify) ===");
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).unwrap();
        engine.register_table(Table::from_loaded(loaded));
    }
    println!("Loaded. Running Q10 warmup + 3 runs...\n");

    // Warmup
    let warm = engine.execute_tpch(Q10).unwrap();
    println!("  Q10 warmup: {} rows", warm.row_count);

    let mut best = f64::INFINITY;
    for i in 0..3 {
        let start = Instant::now();
        let r = engine.execute_tpch(Q10).unwrap();
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        if ms < best { best = ms; }
        println!("  Q10 run {}: {:.1}ms ({} rows)", i + 1, ms, r.row_count);
    }
    println!("\n  Q10 best: {:.1}ms", best);
}
