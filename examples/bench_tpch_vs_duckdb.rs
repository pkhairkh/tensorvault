//! TPC-H benchmark: turboGP vs DuckDB comparison.

use std::process::{Command, Stdio};
use std::io::Write;
use std::time::Instant;
use turbogp::engine::QueryEngine;
use turbogp::datasource::table::Table;

fn generate_lineitem(n: usize) -> Table {
    let mut cols = vec![vec![], vec![], vec![], vec![]];
    let names = vec!["l_quantity".to_string(), "l_extendedprice".to_string(),
                     "l_discount".to_string(), "l_shipdate".to_string()];
    for i in 0..n {
        cols[0].push((i % 50) as u64);
        cols[1].push((i * 100) as u64);
        cols[2].push((i % 10) as u64);
        cols[3].push((i % 365) as u64);
    }
    Table { name: "lineitem".into(), columns: cols, column_names: names, row_count: n, string_columns: vec![None; 4] }
}

fn run_duckdb(sql: &str, db_path: &str) -> Option<String> {
    let mut child = Command::new("duckdb")
        .arg(db_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(sql.as_bytes());
    }
    
    let output = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

fn main() {
    let n = 100_000;
    println!("============================================================");
    println!("  turboGP vs DuckDB — TPC-H Micro Benchmark");
    println!("  Hardware: AMD EPYC-Turin (Zen 5), 8 vCPU, 32GB RAM");
    println!("  Data: {} synthetic lineitem rows", n);
    println!("============================================================\n");

    let mut engine = QueryEngine::new();
    engine.register_table(generate_lineitem(n));
    
    let duckdb_path = "/root/tpch-duckdb/tpch_sf1.duckdb";
    let has_duckdb = std::path::Path::new("/usr/local/bin/duckdb").exists() 
                     && std::path::Path::new(duckdb_path).exists();

    let queries: Vec<(&str, &str)> = vec![
        ("Q1 (count eq)", "SELECT count(*) FROM lineitem WHERE l_quantity = 10"),
        ("Q6 (sum)", "SELECT sum(l_quantity) FROM lineitem"),
        ("Full scan", "SELECT count(*) FROM lineitem"),
    ];

    println!("  {:<20} {:>12} {:>12} {:>10}", "Query", "turboGP (ms)", "DuckDB (ms)", "Ratio");
    println!("  {}", "-".repeat(58));

    for (name, sql) in &queries {
        let tg_start = Instant::now();
        let tg_result = engine.execute(sql);
        let tg_ms = tg_start.elapsed().as_secs_f64() * 1000.0;
        
        let duckdb_ms = if has_duckdb {
            let _ = run_duckdb(sql, duckdb_path);
            let start = Instant::now();
            let _ = run_duckdb(sql, duckdb_path);
            Some(start.elapsed().as_secs_f64() * 1000.0)
        } else {
            None
        };
        
        match (tg_result, duckdb_ms) {
            (Ok(_), Some(dms)) => {
                let ratio = if dms > 0.0 { tg_ms / dms } else { 0.0 };
                println!("  {:<20} {:>10.2}ms {:>10.2}ms {:>8.2}x", name, tg_ms, dms, ratio);
            }
            (Ok(_), None) => {
                println!("  {:<20} {:>10.2}ms {:>10} {:>8}", name, tg_ms, "N/A", "-");
            }
            (Err(e), _) => {
                let msg: String = e.to_string().chars().take(30).collect();
                println!("  {:<20} {:>10} {:>10} {:>8}", name, "ERROR", "-", msg);
            }
        }
    }
    
    println!("  {}", "-".repeat(58));
    println!("\n=== Benchmark complete ===");
}
