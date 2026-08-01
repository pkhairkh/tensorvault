//! Verify Q14 vs DuckDB: print promo_revenue.
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

const Q14: &str = "SELECT 100.00 * sum(case WHEN p_type LIKE 'PROMO%' THEN l_extendedprice * (1 - l_discount) ELSE 0 END) / sum(l_extendedprice * (1 - l_discount)) AS promo_revenue FROM lineitem, part WHERE l_partkey = p_partkey AND l_shipdate >= date '1995-09-01' AND l_shipdate < date '1995-10-01'";

fn main() {
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).expect("load");
        engine.register_table(Table::from_loaded(loaded));
    }

    let res = engine.execute_tpch(Q14).expect("Q14");
    println!("Q14 result: {} rows, {} cols", res.row_count, res.columns.len());
    for (ci, col) in res.columns.iter().enumerate() {
        println!("col[{}] name='{}' values={:?}", ci, col.name, &col.values);
    }
    if res.row_count >= 1 && !res.columns.is_empty() {
        let pr_bits = res.columns[0].values[0];
        let pr = f64::from_bits(pr_bits);
        println!("--- turboGP promo_revenue ---");
        println!("  {:.10}", pr);
    }
    println!("--- DuckDB ground truth ---");
    println!("  16.380778626395543");
}
