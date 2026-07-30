use turbogp::engine::QueryEngine;
use turbogp::datasource::table::Table;
use turbogp::datasource::parquet::{LoadedColumn, LoadedTable};
use std::time::Instant;

fn main() {
    let n = 1_000_000;
    let mut engine = QueryEngine::new();

    // Create lineitem-like table
    let mut li = vec![
        LoadedColumn { name: "l_orderkey".into(), cells: Vec::with_capacity(n), row_count: n, string_search: None },
        LoadedColumn { name: "l_partkey".into(), cells: Vec::with_capacity(n), row_count: n, string_search: None },
        LoadedColumn { name: "l_quantity".into(), cells: Vec::with_capacity(n), row_count: n, string_search: None },
        LoadedColumn { name: "l_extendedprice".into(), cells: Vec::with_capacity(n), row_count: n, string_search: None },
    ];
    for i in 0..n {
        li[0].cells.push((i / 5) as u64);
        li[1].cells.push((i % 200) as u64);
        li[2].cells.push((i % 50) as u64);
        li[3].cells.push((i * 100) as u64);
    }
    engine.register_table(Table::from_loaded(LoadedTable { name: "lineitem".into(), columns: li, row_count: n }));

    // Create orders-like table (n/5 rows)
    let on = n / 5;
    let mut ord = vec![
        LoadedColumn { name: "o_orderkey".into(), cells: Vec::with_capacity(on), row_count: on, string_search: None },
        LoadedColumn { name: "o_orderdate".into(), cells: Vec::with_capacity(on), row_count: on, string_search: None },
        LoadedColumn { name: "o_totalprice".into(), cells: Vec::with_capacity(on), row_count: on, string_search: None },
    ];
    for i in 0..on {
        ord[0].cells.push(i as u64);
        ord[1].cells.push(18489 + (i % 365) as u64);
        ord[2].cells.push((i * 1000) as u64);
    }
    engine.register_table(Table::from_loaded(LoadedTable { name: "orders".into(), columns: ord, row_count: on }));

    // Create customer table (1500 rows)
    let cn = 1500;
    let mut cust = vec![
        LoadedColumn { name: "c_custkey".into(), cells: Vec::with_capacity(cn), row_count: cn, string_search: None },
        LoadedColumn { name: "c_nationkey".into(), cells: Vec::with_capacity(cn), row_count: cn, string_search: None },
    ];
    for i in 0..cn {
        cust[0].cells.push(i as u64);
        cust[1].cells.push((i % 25) as u64);
    }
    engine.register_table(Table::from_loaded(LoadedTable { name: "customer".into(), columns: cust, row_count: cn }));

    let queries = vec![
        ("2-table JOIN count", "SELECT count(*) FROM lineitem JOIN orders ON l_orderkey = o_orderkey"),
        ("3-table JOIN count", "SELECT count(*) FROM customer JOIN orders ON c_custkey = o_orderkey JOIN lineitem ON l_orderkey = o_orderkey"),
        ("JOIN + filter", "SELECT count(*) FROM lineitem JOIN orders ON l_orderkey = o_orderkey WHERE l_quantity > 25"),
        ("JOIN + GROUP BY", "SELECT l_partkey, count(*) FROM lineitem JOIN orders ON l_orderkey = o_orderkey GROUP BY l_partkey"),
    ];

    for (name, sql) in &queries {
        let start = Instant::now();
        match engine.execute(sql) {
            Ok(r) => {
                let ms = start.elapsed().as_micros() as f64 / 1000.0;
                println!("{}: {:.3} ms (rows: {})", name, ms, r.row_count);
            }
            Err(e) => println!("{}: ERROR: {}", name, e),
        }
    }
}
