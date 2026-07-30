//! TPC-H CSV loader demo (Wave 5).
//!
//! Loads all 8 TPC-H SF=1 tables from `/tmp/tpch_*.csv` (pipe-delimited,
//! with headers) into turboGP's `LoadedTable` format via the schema-aware
//! `read_tpch_csv` loader, registers them in a `QueryEngine`, and runs a
//! trivial `SELECT count(*) FROM lineitem` as a sanity check.
//!
//! Usage:
//!   cargo run --release --example load_tpch
//!
//! Prerequisites:
//!   - All 8 `/tmp/tpch_*.csv` files present (customer, lineitem, nation,
//!     orders, part, partsupp, region, supplier) at SF=1.
//!
//! Expected output (row counts MUST match SF=1):
//!   - region:    5 rows
//!   - nation:    25 rows
//!   - supplier:  10,000 rows
//!   - customer:  150,000 rows
//!   - part:      200,000 rows
//!   - partsupp:  800,000 rows
//!   - orders:    1,500,000 rows
//!   - lineitem:  6,001,215 rows

use std::time::Instant;
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

fn main() {
    // Order: load smallest-first so any error surfaces early. lineitem
    // (770 MB, 6 M rows) is loaded last — it dominates total load time.
    let tables = [
        "region",
        "nation",
        "supplier",
        "customer",
        "part",
        "partsupp",
        "orders",
        "lineitem",
    ];

    // SF=1 expected row counts (from the TPC-H spec).
    let expected: std::collections::HashMap<&str, usize> = [
        ("region", 5),
        ("nation", 25),
        ("supplier", 10_000),
        ("customer", 150_000),
        ("part", 200_000),
        ("partsupp", 800_000),
        ("orders", 1_500_000),
        ("lineitem", 6_001_215),
    ]
    .iter()
    .copied()
    .collect();

    let mut engine = QueryEngine::new();
    let mut total_ms: u128 = 0;
    let mut all_ok = true;

    println!("=== TPC-H CSV load (turboGP Wave 5) ===\n");

    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let start = Instant::now();
        match read_tpch_csv(&path, t) {
            Ok(loaded) => {
                let ms = start.elapsed().as_millis();
                let rows = loaded.row_count;
                let exp = expected[t];
                let ok = rows == exp;
                all_ok &= ok;
                let status = if ok { "OK" } else { "MISMATCH" };
                let str_cols = loaded
                    .columns
                    .iter()
                    .filter(|c| c.string_search.is_some())
                    .count();
                println!(
                    "  {:10} {:>10} rows  ({:5} ms)  [string cols: {}]  {}",
                    t, rows, ms, str_cols, status
                );
                if !ok {
                    println!("    !! expected {} rows, got {}", exp, rows);
                }
                total_ms += ms;
                let table = Table::from_loaded(loaded);
                engine.register_table(table);
            }
            Err(e) => {
                let ms = start.elapsed().as_millis();
                all_ok = false;
                println!("  {:10} ERROR after {} ms: {}", t, ms, e);
            }
        }
    }

    println!("\n  Total load time: {} ms", total_ms);

    // Sanity-check that the engine can run a simple aggregation on the
    // largest table. `SELECT count(*) FROM lineitem` exercises the
    // scan + aggregate path; the result MUST be 6,001,215.
    println!("\n=== Sanity check ===");
    let r = match engine.execute("SELECT count(*) FROM lineitem") {
        Ok(r) => r,
        Err(e) => {
            println!("  SELECT count(*) FROM lineitem FAILED: {}", e);
            std::process::exit(1);
        }
    };
    if r.row_count == 1 && !r.columns.is_empty() {
        // The first column's first cell holds the count (u64).
        let count_cell = r.columns[0].values.first().copied().unwrap_or(0);
        println!("  SELECT count(*) FROM lineitem  ->  {}", count_cell);
        if count_cell == 6_001_215 {
            println!("  (matches expected SF=1 lineitem row count)");
        } else {
            println!("  !! expected 6001215, got {}", count_cell);
            all_ok = false;
        }
    } else {
        println!("  unexpected result shape: row_count={}, cols={}", r.row_count, r.columns.len());
        all_ok = false;
    }

    // One more: count(*) on each table to confirm each was registered
    // with the right name. NOTE: `region` is a SQL keyword in
    // turboGP's lexer (Token::Keyword("REGION")) so the parser rejects
    // `FROM region` — we skip it here (the row count was already
    // verified at load time). The other 7 tables all have non-keyword
    // names and round-trip through the parser correctly.
    println!("\n=== Per-table count(*) ===");
    for t in &tables {
        if *t == "region" {
            println!("  {:10} count(*) skipped (REGION is a SQL keyword in turboGP)", t);
            continue;
        }
        let sql = format!("SELECT count(*) FROM {}", t);
        match engine.execute(&sql) {
            Ok(r) => {
                let count = r.columns.first().and_then(|c| c.values.first()).copied().unwrap_or(0);
                let exp = expected[t] as u64;
                let status = if count == exp { "OK" } else { "MISMATCH" };
                println!("  {:10} count(*) = {:>10}  (expected {:>10})  {}", t, count, exp, status);
                if count != exp {
                    all_ok = false;
                }
            }
            Err(e) => {
                println!("  {:10} count(*) FAILED: {}", t, e);
                all_ok = false;
            }
        }
    }

    println!("\n=== TPC-H load {} ===", if all_ok { "complete (all OK)" } else { "FAILED" });
    if !all_ok {
        std::process::exit(1);
    }
}
