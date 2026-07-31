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
    let subq = "SELECT l_suppkey AS supplier_no, sum(l_extendedprice * (1 - l_discount)) AS total_revenue FROM lineitem WHERE l_shipdate >= date '1996-01-01' AND l_shipdate < date '1996-04-01' GROUP BY l_suppkey";
    let r = engine.execute_tpch(subq).unwrap();
    let tr_col = r.columns.iter().find(|c| c.name == "total_revenue").unwrap();
    let max_val = tr_col.values.iter().map(|&v| f64::from_bits(v)).fold(0.0f64, f64::max);
    println!("max total_revenue = {}", max_val);
    // Find how many match exactly
    let exact = tr_col.values.iter().filter(|&&v| f64::from_bits(v) == max_val).count();
    println!("exact matches: {}", exact);
    // Find how many match with epsilon
    let eps = tr_col.values.iter().filter(|&&v| (f64::from_bits(v) - max_val).abs() < 1e-6).count();
    println!("epsilon matches: {}", eps);
}
