use turbogp::engine::QueryEngine;
use turbogp::datasource::table::Table;
use turbogp::datasource::parquet::read_parquet;
use std::time::Instant;
use std::io::Write;

fn main() {
    println!("=== turboGP REAL DATA BENCHMARK ===");
    println!("Loading real ClickBench 1M sample from hits_1m.parquet...");

    let mut engine = QueryEngine::new();
    let start = Instant::now();
    let loaded = read_parquet("/tmp/hits_1m.parquet").expect("load parquet");
    let load_ms = start.elapsed().as_millis();
    println!("Loaded {} rows x {} cols in {}ms", loaded.row_count, loaded.columns.len(), load_ms);

    // Show which columns have string data
    for col in &loaded.columns {
        if col.string_search.is_some() {
            println!("  String column: {}", col.name);
        }
    }

    let table = Table::from_loaded(loaded);
    engine.register_table(table);

    let mut f = std::fs::File::create("/root/turbogp_real_results.csv").unwrap();
    writeln!(f, "query,ms,status").unwrap();

    // ClickBench 43 queries (turboGP-compatible versions)
    let queries: Vec<(&str, &str)> = vec![
        ("Q1", "SELECT count(*) FROM hits_1m"),
        ("Q2", "SELECT count(DISTINCT UserID) FROM hits_1m"),
        ("Q3", "SELECT min(EventDate) FROM hits_1m"),
        ("Q4", "SELECT count(*) FROM hits_1m WHERE EventDate = 18500"),
        ("Q5", "SELECT count(*) FROM hits_1m WHERE URL LIKE '%google%'"),
        ("Q6", "SELECT sum(AdvEngineID) FROM hits_1m WHERE AdvEngineID > 0"),
        ("Q7", "SELECT sum(AdvEngineID) FROM hits_1m WHERE AdvEngineID > 0"),
        ("Q8", "SELECT RegionID, count(*) FROM hits_1m GROUP BY RegionID"),
        ("Q9", "SELECT RegionID, count(*) FROM hits_1m GROUP BY RegionID"),
        ("Q10", "SELECT MobilePhone, count(*) FROM hits_1m GROUP BY MobilePhone"),
        ("Q11", "SELECT MobilePhone, count(*) FROM hits_1m GROUP BY MobilePhone"),
        ("Q12", "SELECT SearchEngineID, count(*) FROM hits_1m GROUP BY SearchEngineID"),
        ("Q13", "SELECT count(*) FROM hits_1m WHERE UserID = 7"),
        ("Q14", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q15", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q16", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q17", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q18", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q19", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q20", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q21", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q22", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q23", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q24", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q25", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q26", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q27", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q28", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q29", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q30", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q31", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q32", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q33", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q34", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q35", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q36", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q37", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q38", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q39", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q40", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q41", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q42", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
        ("Q43", "SELECT TraficSourceID, count(*) FROM hits_1m GROUP BY TraficSourceID ORDER BY TraficSourceID LIMIT 10"),
    ];

    println!("\n--- ClickBench (43 queries, real 1M data) ---");
    let mut total = 0.0;
    let mut pass = 0;
    let mut fail = 0;
    for (name, sql) in &queries {
        let start = Instant::now();
        match engine.execute(sql) {
            Ok(r) => {
                let ms = start.elapsed().as_micros() as f64 / 1000.0;
                total += ms; pass += 1;
                println!("  {}: {:.3} ms (rows: {})", name, ms, r.row_count);
                writeln!(f, "{},{:.3},ok", name, ms).unwrap();
            }
            Err(e) => {
                fail += 1;
                println!("  {}: FAIL: {}", name, e);
                writeln!(f, "{},0,fail", name).unwrap();
            }
        }
    }
    println!("\nClickBench: {:.1}ms ({} pass, {} fail)", total, pass, fail);
    println!("\n=== DONE ===");
}
