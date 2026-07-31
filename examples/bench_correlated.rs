//! Quick test: run previously-skipped correlated subquery queries.
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;
use std::time::Instant;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() {
    println!("=== Correlated subquery test ===");
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).unwrap();
        engine.register_table(Table::from_loaded(loaded));
    }
    println!("Loaded.\n");

    // Q17: correlated subquery — SELECT sum(l_extendedprice) / 7.0 FROM lineitem, part
    //      WHERE p_partkey = l_partkey AND p_brand = 'Brand#23' AND p_container = 'MED BOX'
    //      AND l_quantity < (SELECT 0.2 * avg(l_quantity) FROM lineitem WHERE l_partkey = p_partkey)
    let q17 = "SELECT sum(l_extendedprice) / 7.0 AS avg_yearly FROM lineitem, part WHERE p_partkey = l_partkey AND p_brand = 'Brand#23' AND p_container = 'MED BOX' AND l_quantity < (SELECT 0.2 * avg(l_quantity) FROM lineitem WHERE l_partkey = p_partkey)";

    // Q20: nested correlated subquery with IN
    let q20 = "SELECT s_name, s_address FROM supplier, nation WHERE s_suppkey IN (SELECT ps_suppkey FROM partsupp WHERE ps_partkey IN (SELECT p_partkey FROM part WHERE p_name LIKE 'forest%') AND ps_availqty > (SELECT 0.5 * sum(l_quantity) FROM lineitem WHERE l_partkey = ps_partkey AND l_suppkey = ps_suppkey AND l_shipdate >= date '1994-01-01' AND l_shipdate < date '1995-01-01')) AND s_nationkey = n_nationkey AND n_name = 'CANADA' ORDER BY s_name";

    // Q4: EXISTS subquery
    let q4 = "SELECT o_orderpriority, count(*) AS order_count FROM orders WHERE o_orderdate >= date '1993-07-01' AND o_orderdate < date '1993-10-01' AND exists (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND l_commitdate < l_receiptdate) GROUP BY o_orderpriority ORDER BY o_orderpriority";

    let engine = Arc::new(engine);

    for (name, sql) in [("Q4", q4), ("Q17", q17), ("Q20", q20)] {
        let (tx, rx) = mpsc::channel();
        let eng = Arc::clone(&engine);
        let sql_owned = sql.to_string();
        thread::spawn(move || {
            let t0 = Instant::now();
            let result = eng.execute_tpch(&sql_owned);
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            let _ = tx.send(result.map(|r| (ms, r.row_count)));
        });
        match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(Ok((ms, rows))) => println!("  {}: OK  {:.0}ms ({} rows)", name, ms, rows),
            Ok(Err(e)) => println!("  {}: FAIL {}", name, e),
            Err(_) => println!("  {}: TIMEOUT (120s)", name),
        }
    }
}
