//! Verify Q11 vs DuckDB: print row count and top-5 (ps_partkey, value).
//! Used to capture baseline before W9-4 reformulation, then verify the new
//! implementation produces bit-identical output (within 1e-6 relative FP).
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

const Q11: &str = "SELECT ps_partkey, sum(ps_supplycost * ps_availqty) AS value FROM partsupp, supplier, nation WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND n_name = 'GERMANY' GROUP BY ps_partkey HAVING sum(ps_supplycost * ps_availqty) > (SELECT sum(ps_supplycost * ps_availqty) * 0.0001 FROM partsupp, supplier, nation WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND n_name = 'GERMANY') ORDER BY value DESC";

fn main() {
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).expect("load");
        engine.register_table(Table::from_loaded(loaded));
    }
    let res = engine.execute_tpch(Q11).expect("Q11");
    println!("Q11 result: {} rows, {} cols", res.row_count, res.columns.len());
    for (ci, col) in res.columns.iter().enumerate() {
        println!("col[{}] name='{}'", ci, col.name);
    }
    let n = res.row_count.min(5);
    println!("--- top {} (ps_partkey, value) ---", n);
    for r in 0..n {
        let pk = res.columns[0].values[r];
        let val_bits = res.columns[1].values[r];
        let val = f64::from_bits(val_bits);
        println!("row[{}]: ps_partkey={} value={:.6} (bits=0x{:016x})", r, pk, val, val_bits);
    }
    // Also print value at row 99 and last row if available
    if res.row_count > 5 {
        let r = 99.min(res.row_count - 1);
        let pk = res.columns[0].values[r];
        let val_bits = res.columns[1].values[r];
        let val = f64::from_bits(val_bits);
        println!("row[{}]: ps_partkey={} value={:.6} (bits=0x{:016x})", r, pk, val, val_bits);
        let r = res.row_count - 1;
        let pk = res.columns[0].values[r];
        let val_bits = res.columns[1].values[r];
        let val = f64::from_bits(val_bits);
        println!("row[{}] (last): ps_partkey={} value={:.6} (bits=0x{:016x})", r, pk, val, val_bits);
    }
    // Print sum of all values (sanity check vs threshold)
    let total: f64 = (0..res.row_count).map(|r| f64::from_bits(res.columns[1].values[r])).sum();
    println!("sum of all values = {:.6}", total);
}
