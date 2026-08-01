//! Verify Q16 vs DuckDB: print top-5 (p_brand, p_type, p_size, supplier_cnt).
//! Used to capture baseline before W9-2 reformulation, then verify the new
//! implementation produces bit-identical output.
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

const Q16: &str = "SELECT p_brand, p_type, p_size, count(DISTINCT ps_suppkey) AS supplier_cnt FROM partsupp, part WHERE p_partkey = ps_partkey AND p_brand <> 'Brand#45' AND p_type NOT LIKE 'MEDIUM POLISHED%' AND p_size IN (49, 14, 23, 45, 19, 3, 36, 9) GROUP BY p_brand, p_type, p_size ORDER BY supplier_cnt DESC, p_brand, p_type, p_size";

fn main() {
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).expect("load");
        engine.register_table(Table::from_loaded(loaded));
    }
    // Build hash->string maps from the part table (p_brand col 3, p_type col 4).
    let part_loaded = read_tpch_csv("/tmp/tpch_part.csv", "part").expect("part");
    let part_tbl = Table::from_loaded(part_loaded);
    let mut brand_hash_to_str: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    let mut type_hash_to_str: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    if let Some(ref sc) = part_tbl.string_columns[3] {
        for i in 0..part_tbl.row_count {
            brand_hash_to_str.insert(part_tbl.columns[3][i], sc.get(i).to_string());
        }
    }
    if let Some(ref sc) = part_tbl.string_columns[4] {
        for i in 0..part_tbl.row_count {
            type_hash_to_str.insert(part_tbl.columns[4][i], sc.get(i).to_string());
        }
    }

    let res = engine.execute_tpch(Q16).expect("Q16");
    println!("Q16 result: {} rows, {} cols", res.row_count, res.columns.len());
    for (ci, col) in res.columns.iter().enumerate() {
        println!("col[{}] name='{}'", ci, col.name);
    }
    // Print top 5 rows
    let n = res.row_count.min(5);
    println!("--- top {} (p_brand, p_type, p_size, supplier_cnt) ---", n);
    for r in 0..n {
        let brand_h = res.columns[0].values[r];
        let type_h = res.columns[1].values[r];
        let p_size = res.columns[2].values[r];
        let cnt = res.columns[3].values[r];
        let bs = brand_hash_to_str.get(&brand_h).cloned().unwrap_or_else(|| format!("?0x{:x}", brand_h));
        let ts = type_hash_to_str.get(&type_h).cloned().unwrap_or_else(|| format!("?0x{:x}", type_h));
        println!("row[{}]: p_brand={:?} p_type={:?} p_size={} supplier_cnt={}", r, bs, ts, p_size, cnt);
    }
    println!("--- raw top-5 u64 values (for bit-exact compare) ---");
    for r in 0..n {
        println!("row[{}]: brand_h=0x{:016x} type_h=0x{:016x} size={} cnt={}", r,
            res.columns[0].values[r], res.columns[1].values[r],
            res.columns[2].values[r], res.columns[3].values[r]);
    }
    // Print last row for sanity
    if res.row_count > 5 {
        let r = res.row_count - 1;
        println!("row[{}] (last): brand_h=0x{:016x} type_h=0x{:016x} size={} cnt={}", r,
            res.columns[0].values[r], res.columns[1].values[r],
            res.columns[2].values[r], res.columns[3].values[r]);
    }
}
