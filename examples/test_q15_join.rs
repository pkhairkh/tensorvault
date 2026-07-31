use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

fn main() {
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).expect("load");
        engine.register_table(Table::from_loaded(loaded));
    }
    // Test just the join without the max filter
    let q = "SELECT s_suppkey, total_revenue FROM supplier, (SELECT l_suppkey AS supplier_no, sum(l_extendedprice * (1 - l_discount)) AS total_revenue FROM lineitem WHERE l_shipdate >= date '1996-01-01' AND l_shipdate < date '1996-04-01' GROUP BY l_suppkey) AS revenue WHERE s_suppkey = supplier_no";
    match engine.execute_tpch(q) {
        Ok(r) => println!("join only: {} rows", r.row_count),
        Err(e) => println!("FAIL: {}", e),
    }
    // Test with the max filter
    let q2 = "SELECT s_suppkey, total_revenue FROM supplier, (SELECT l_suppkey AS supplier_no, sum(l_extendedprice * (1 - l_discount)) AS total_revenue FROM lineitem WHERE l_shipdate >= date '1996-01-01' AND l_shipdate < date '1996-04-01' GROUP BY l_suppkey) AS revenue WHERE s_suppkey = supplier_no AND total_revenue = 1772627.2087";
    match engine.execute_tpch(q2) {
        Ok(r) => println!("with literal filter: {} rows", r.row_count),
        Err(e) => println!("FAIL: {}", e),
    }
}
