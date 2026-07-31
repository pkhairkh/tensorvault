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
    // Test the subquery alone
    let subq = "SELECT l_suppkey AS supplier_no, sum(l_extendedprice * (1 - l_discount)) AS total_revenue FROM lineitem WHERE l_shipdate >= date '1996-01-01' AND l_shipdate < date '1996-04-01' GROUP BY l_suppkey";
    match engine.execute_tpch(subq) {
        Ok(r) => {
            println!("subquery: {} rows", r.row_count);
            if r.row_count > 0 {
                for col in &r.columns {
                    print!("{} = ", col.name);
                    for v in col.values.iter().take(5) { print!("{:?} ", v); }
                    println!();
                }
            }
        }
        Err(e) => println!("subquery FAIL: {}", e),
    }
    // Test max subquery
    let maxq = "SELECT max(total_revenue) FROM (SELECT l_suppkey AS supplier_no, sum(l_extendedprice * (1 - l_discount)) AS total_revenue FROM lineitem WHERE l_shipdate >= date '1996-01-01' AND l_shipdate < date '1996-04-01' GROUP BY l_suppkey) AS revenue";
    match engine.execute_tpch(maxq) {
        Ok(r) => {
            println!("max subquery: {} rows", r.row_count);
            for col in &r.columns {
                println!("  {} = {:?} (f64={})", col.name, col.values.get(0), col.values.get(0).map(|&v| f64::from_bits(v)).unwrap_or(0.0));
            }
        }
        Err(e) => println!("max subquery FAIL: {}", e),
    }
}
