use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

const Q15: &str = "SELECT s_suppkey, s_name, s_address, s_phone, total_revenue FROM supplier, (SELECT l_suppkey AS supplier_no, sum(l_extendedprice * (1 - l_discount)) AS total_revenue FROM lineitem WHERE l_shipdate >= date '1996-01-01' AND l_shipdate < date '1996-04-01' GROUP BY l_suppkey) AS revenue WHERE s_suppkey = supplier_no AND total_revenue = (SELECT max(total_revenue) FROM (SELECT l_suppkey AS supplier_no, sum(l_extendedprice * (1 - l_discount)) AS total_revenue FROM lineitem WHERE l_shipdate >= date '1996-01-01' AND l_shipdate < date '1996-04-01' GROUP BY l_suppkey) AS revenue) ORDER BY s_suppkey";

fn main() {
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).expect("load");
        engine.register_table(Table::from_loaded(loaded));
    }
    match engine.execute_tpch(Q15) {
        Ok(r) => {
            println!("Q15: {} rows", r.row_count);
            for col in &r.columns { println!("  {} = {:?}", col.name, col.values.get(0)); }
        }
        Err(e) => println!("Q15 FAIL: {}", e),
    }
}
