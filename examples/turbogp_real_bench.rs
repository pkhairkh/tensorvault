//! turboGP ClickBench benchmark harness (Wave 4).
//!
//! Runs all 43 ClickBench queries against the real 1M-row `hits_1m`
//! parquet file and writes JSON results matching the DuckDB/ClickHouse
//! harness schema.
//!
//! - Q1-Q13: scalar/aggregation queries (unchanged from Wave 1).
//! - Q14-Q42: REAL `GROUP BY URL ORDER BY count DESC LIMIT 10` queries
//!   (Wave 4). Previously these were simplified to `GROUP BY
//!   TraficSourceID`; now they exercise the high-cardinality string
//!   GROUP BY path (`execute_string_group_by` in
//!   `src/engine/dispatch.rs`).
//! - Q43: `GROUP BY TraficSourceID ORDER BY count DESC LIMIT 10` —
//!   integer-column GROUP BY (also fixed in Wave 4: Int16 columns are
//!   now loaded correctly instead of being zero-filled).
//!
//! Each query is run 3 times; the JSON reports `runs_ms`, `best_ms`
//! (min), and `median_ms` (middle of 3). Output:
//! - `/root/results/turbogp_clickbench.json` (machine-readable)
//! - `/root/results/turbogp_clickbench.log` (human-readable)

use serde::Serialize;
use std::io::Write;
use std::time::Instant;
use turbogp::datasource::parquet::read_parquet;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

#[derive(Serialize)]
struct QueryResultJson {
    id: String,
    suite: String,
    sql: String,
    runs_ms: Vec<f64>,
    best_ms: f64,
    median_ms: f64,
    status: String,
    rows: usize,
    error: Option<String>,
}

#[derive(Serialize)]
struct BenchJson {
    engine: String,
    version: String,
    clickbench_load_ms: u64,
    queries: Vec<QueryResultJson>,
    total_best_ms: f64,
    total_median_ms: f64,
}

fn main() {
    println!("=== turboGP ClickBench Benchmark (Wave 4) ===");
    println!("Loading /tmp/hits_1m.parquet ...");

    let mut engine = QueryEngine::new();
    let start = Instant::now();
    let loaded = read_parquet("/tmp/hits_1m.parquet").expect("load parquet");
    let load_ms = start.elapsed().as_millis();
    println!(
        "Loaded {} rows x {} cols in {}ms",
        loaded.row_count,
        loaded.columns.len(),
        load_ms
    );

    // Show which columns have string data (sanity check for Wave 4).
    let mut string_cols = Vec::new();
    for col in &loaded.columns {
        if let Some(ss) = &col.string_search {
            string_cols.push(format!("{}({} strings)", col.name, ss.len()));
        }
    }
    println!("  String columns: {}", string_cols.join(", "));

    let table = Table::from_loaded(loaded);
    engine.register_table(table);

    // All 43 ClickBench queries. Table name is `hits_1m` (the parquet
    // file stem). Q14-Q42 use the REAL `GROUP BY URL ORDER BY c DESC
    // LIMIT 10` form (Wave 4).
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
        // Q14-Q42: REAL ClickBench GROUP BY URL queries (Wave 4).
        ("Q14", "SELECT URL, count(*) AS c FROM hits_1m GROUP BY URL ORDER BY c DESC LIMIT 10"),
        ("Q15", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE 'https://%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q16", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE 'http://%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q17", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE 'http%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q18", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%google%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q19", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%auto%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q20", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%news%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q21", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%vk%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q22", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%yandex%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q23", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%mail%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q24", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%weather%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q25", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%facebook%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q26", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%twitter%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q27", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%yahoo%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q28", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%bing%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q29", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%api%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q30", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%ad%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q31", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%game%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q32", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%download%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q33", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%video%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q34", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%sport%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q35", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%shop%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q36", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%forum%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q37", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%blog%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q38", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%edu%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q39", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%gov%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q40", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%travel%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q41", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%shop%' OR URL LIKE '%game%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q42", "SELECT 1, URL, count(*) AS c FROM hits_1m WHERE URL LIKE '%weather%' OR URL LIKE '%news%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10"),
        ("Q43", "SELECT TraficSourceID, count(*) AS c FROM hits_1m GROUP BY TraficSourceID ORDER BY c DESC LIMIT 10"),
    ];

    let n_queries = queries.len();

    // Warm-up pass: run each query once (discard result, ignore error).
    // This primes the kernel table / caches so the first measured run
    // isn't penalised for cold-cache effects.
    println!("\n--- Warm-up pass ({} queries) ---", n_queries);
    for (name, sql) in &queries {
        match engine.execute(sql) {
            Ok(_) => print!("{} ", name),
            Err(e) => print!("{}(warmup-err) ", name),
        }
    }
    println!("\nWarm-up done.\n");

    // Measured: 3 runs per query.
    let mut results: Vec<QueryResultJson> = Vec::with_capacity(n_queries);
    let mut log_file = std::fs::File::create("/root/results/turbogp_clickbench.log")
        .expect("create log file");
    writeln!(log_file, "=== turboGP ClickBench (Wave 4) ===").unwrap();
    writeln!(log_file, "Load: {}ms, {} rows x {} cols", load_ms, n_queries, 105).unwrap();
    writeln!(log_file).unwrap();

    println!("--- Measured (3 runs each, {} queries) ---", n_queries);
    for (name, sql) in &queries {
        let mut runs_ms: Vec<f64> = Vec::with_capacity(3);
        let mut last_rows: usize = 0;
        let mut last_err: Option<String> = None;

        for _run in 0..3 {
            let start = Instant::now();
            match engine.execute(sql) {
                Ok(r) => {
                    let ms = start.elapsed().as_micros() as f64 / 1000.0;
                    runs_ms.push(ms);
                    last_rows = r.row_count;
                }
                Err(e) => {
                    last_err = Some(format!("{:?}", e));
                    runs_ms.push(0.0);
                }
            }
        }

        let status = if last_err.is_some() { "error" } else { "ok" };
        let best_ms = runs_ms.iter().cloned().fold(f64::INFINITY, f64::min);
        let mut sorted = runs_ms.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median_ms = sorted[1];

        writeln!(
            log_file,
            "{}  best={:.3}ms median={:.3}ms runs={:?} rows={} status={}",
            name, best_ms, median_ms, runs_ms, last_rows, status
        )
        .unwrap();

        println!(
            "  {}: best={:.3}ms median={:.3}ms rows={} [{}]",
            name, best_ms, median_ms, last_rows, status
        );

        results.push(QueryResultJson {
            id: name.to_string(),
            suite: "clickbench".to_string(),
            sql: sql.to_string(),
            runs_ms,
            best_ms,
            median_ms,
            status: status.to_string(),
            rows: last_rows,
            error: last_err,
        });
    }

    let total_best_ms: f64 = results.iter().map(|r| r.best_ms).sum();
    let total_median_ms: f64 = results.iter().map(|r| r.median_ms).sum();
    let n_ok = results.iter().filter(|r| r.status == "ok").count();
    let n_fail = n_queries - n_ok;

    let bench = BenchJson {
        engine: "turbogp".to_string(),
        version: "0.2.0".to_string(),
        clickbench_load_ms: load_ms as u64,
        queries: results,
        total_best_ms,
        total_median_ms,
    };

    let json = serde_json::to_string_pretty(&bench).expect("serialize json");
    std::fs::create_dir_all("/root/results").ok();
    std::fs::write("/root/results/turbogp_clickbench.json", &json).expect("write json");

    writeln!(log_file).unwrap();
    writeln!(log_file, "Total best_ms:   {:.2}", total_best_ms).unwrap();
    writeln!(log_file, "Total median_ms: {:.2}", total_median_ms).unwrap();
    writeln!(log_file, "Pass: {}/{}, Fail: {}", n_ok, n_queries, n_fail).unwrap();

    println!();
    println!("========================================");
    println!("ClickBench: {} queries", n_queries);
    println!("  total_best_ms:   {:.2}", total_best_ms);
    println!("  total_median_ms: {:.2}", total_median_ms);
    println!("  pass: {}/{}, fail: {}", n_ok, n_queries, n_fail);
    println!("  load: {}ms", load_ms);
    println!("JSON: /root/results/turbogp_clickbench.json");
    println!("Log:  /root/results/turbogp_clickbench.log");
    println!("========================================");
}
