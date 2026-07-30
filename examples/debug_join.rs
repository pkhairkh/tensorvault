use turbogp::engine::QueryEngine;
use turbogp::datasource::table::Table;
use turbogp::datasource::parquet::{LoadedColumn, LoadedTable};

fn main() {
    let mut engine = QueryEngine::new();
    let n = 100;
    let li = vec![
        LoadedColumn { name: "l_orderkey".into(), cells: (0..n).map(|i| (i/5) as u64).collect(), row_count: n },
        LoadedColumn { name: "l_partkey".into(), cells: (0..n).map(|i| (i%200) as u64).collect(), row_count: n },
    ];
    engine.register_table(Table::from_loaded(LoadedTable { name: "lineitem".into(), columns: li, row_count: n }));
    let ord = vec![
        LoadedColumn { name: "o_orderkey".into(), cells: (0..20).map(|i| i as u64).collect(), row_count: 20 },
    ];
    engine.register_table(Table::from_loaded(LoadedTable { name: "orders".into(), columns: ord, row_count: 20 }));
    let cust = vec![
        LoadedColumn { name: "c_custkey".into(), cells: (0..15).map(|i| i as u64).collect(), row_count: 15 },
    ];
    engine.register_table(Table::from_loaded(LoadedTable { name: "customer".into(), columns: cust, row_count: 15 }));

    let r = engine.execute("SELECT l_orderkey, count(*) FROM customer JOIN orders ON c_custkey = o_orderkey JOIN lineitem ON l_orderkey = o_orderkey GROUP BY l_orderkey");
    match r {
        Ok(result) => println!("ok, {} groups", result.row_count),
        Err(e) => println!("FAIL: {}", e),
    }
}
