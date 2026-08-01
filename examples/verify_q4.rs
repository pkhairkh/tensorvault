//! Verify Q4 reformulation: print all 5 rows (priority hash + count).
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

const Q4: &str = "SELECT o_orderpriority, count(*) AS order_count FROM orders WHERE o_orderdate >= date '1993-07-01' AND o_orderdate < date '1993-10-01' AND exists (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND l_commitdate < l_receiptdate) GROUP BY o_orderpriority ORDER BY o_orderpriority";

fn main() {
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).expect("load");
        engine.register_table(Table::from_loaded(loaded));
    }
    let res = engine.execute_tpch(Q4).expect("Q4");
    println!("Q4: {} rows, {} cols", res.row_count, res.columns.len());
    for i in 0..res.row_count {
        let priority_h = res.columns[0].values[i];
        let count = res.columns[1].values[i];
        println!("row[{}]: priority_hash={:#018x} order_count={}", i, priority_h, count);
    }
}
