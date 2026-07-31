//! Profile TPC-H queries to find the real bottleneck.
//! Runs Q6 (no join), Q3 (3-table join), Q5 (6-table join) with timing.
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;
use std::time::Instant;

fn main() {
    println!("=== turboGP TPC-H profiling ===");
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).unwrap();
        engine.register_table(Table::from_loaded(loaded));
    }
    println!("Loaded.\n");

    let queries: Vec<(&str, &str)> = vec![
        ("Q6 (no join)", "SELECT sum(l_extendedprice * l_discount) AS revenue FROM lineitem WHERE l_shipdate >= date '1994-01-01' AND l_shipdate < date '1995-01-01' AND l_discount >= 0.05 AND l_discount <= 0.07 AND l_quantity < 24"),
        ("Q14 (2-table join)", "SELECT 100.00 * sum(case WHEN p_type LIKE 'PROMO%' THEN l_extendedprice * (1 - l_discount) ELSE 0 END) / sum(l_extendedprice * (1 - l_discount)) AS promo_revenue FROM lineitem, part WHERE l_partkey = p_partkey AND l_shipdate >= date '1995-09-01' AND l_shipdate < date '1995-10-01'"),
        ("Q3 (3-table join)", "SELECT l_orderkey, sum(l_extendedprice * (1 - l_discount)) AS revenue, o_orderdate, o_shippriority FROM customer, orders, lineitem WHERE c_mktsegment = 'BUILDING' AND c_custkey = o_custkey AND l_orderkey = o_orderkey AND o_orderdate < date '1995-03-15' AND l_shipdate > date '1995-03-15' GROUP BY l_orderkey, o_orderdate, o_shippriority ORDER BY revenue DESC, o_orderdate LIMIT 10"),
        ("Q5 (6-table join)", "SELECT n_name, sum(l_extendedprice * (1 - l_discount)) AS revenue FROM customer, orders, lineitem, supplier, nation, region WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey AND l_suppkey = s_suppkey AND c_nationkey = s_nationkey AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'ASIA' AND o_orderdate >= date '1994-01-01' AND o_orderdate < date '1995-01-01' GROUP BY n_name ORDER BY revenue DESC"),
    ];

    for (name, sql) in &queries {
        // warmup
        let _ = engine.execute_tpch(sql);
        let start = Instant::now();
        let r = engine.execute_tpch(sql).unwrap();
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        println!("  {}: {:.1}ms ({} rows)", name, ms, r.row_count);
    }
}
