use turbogp::engine::QueryEngine;
use turbogp::datasource::table::Table;
use turbogp::datasource::parquet::{LoadedColumn, LoadedTable};
use std::time::Instant;

fn main() {
    let n = 1_000_000;
    let mut cols = vec![
        LoadedColumn { name: "id".into(), cells: Vec::with_capacity(n), row_count: n, string_search: None },
        LoadedColumn { name: "EventDate".into(), cells: Vec::with_capacity(n), row_count: n, string_search: None },
        LoadedColumn { name: "AdvEngineID".into(), cells: Vec::with_capacity(n), row_count: n, string_search: None },
        LoadedColumn { name: "RegionID".into(), cells: Vec::with_capacity(n), row_count: n, string_search: None },
    ];
    for i in 0..n {
        cols[0].cells.push(i as u64);
        cols[1].cells.push(18489 + (i % 365) as u64);
        cols[2].cells.push((i % 20) as u64);
        cols[3].cells.push((i % 200) as u64);
    }
    let mut engine = QueryEngine::new();
    engine.register_table(Table::from_loaded(LoadedTable { name: "hits".into(), columns: cols, row_count: n }));

    let queries = vec![
        ("Q1 count(*)", "SELECT count(*) FROM hits"),
        ("Q4 count+filter", "SELECT count(*) FROM hits WHERE EventDate = 18500"),
        ("Q6 sum+filter", "SELECT sum(AdvEngineID) FROM hits WHERE AdvEngineID > 5"),
        ("Q8 GROUP BY", "SELECT RegionID, count(*) FROM hits GROUP BY RegionID"),
        ("Q19 OR", "SELECT count(*) FROM hits WHERE AdvEngineID > 5 OR RegionID > 100"),
        ("Q20 AND", "SELECT count(*) FROM hits WHERE AdvEngineID > 5 AND RegionID < 50"),
    ];

    for (name, sql) in &queries {
        let start = Instant::now();
        let _r = engine.execute(sql).unwrap();
        let ms = start.elapsed().as_micros() as f64 / 1000.0;
        println!("{}: {:.3} ms", name, ms);
    }
}
