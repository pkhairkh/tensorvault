//! Verify Q2 reformulation vs DuckDB: print top 5 rows.
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

const Q2: &str = "SELECT s_acctbal, s_name, n_name, p_partkey, p_mfgr, s_address, s_phone, s_comment FROM part, partsupp, supplier, nation, region WHERE p_partkey = ps_partkey AND s_suppkey = ps_suppkey AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'EUROPE' AND p_size = 15 AND p_type LIKE '%BRASS' AND ps_supplycost = (SELECT min(ps_supplycost) FROM partsupp, supplier, nation, region WHERE p_partkey = ps_partkey AND s_suppkey = ps_suppkey AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'EUROPE') ORDER BY s_acctbal DESC, n_name, s_name, p_partkey LIMIT 100";

fn main() {
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).expect("load");
        engine.register_table(Table::from_loaded(loaded));
    }
    let res = engine.execute_tpch(Q2).expect("Q2");
    println!("Q2 baseline: {} rows, {} cols", res.row_count, res.columns.len());
    println!("col names: {:?}", res.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
    // Print top 5 rows: (s_acctbal, s_name_hash, n_name_hash, p_partkey, p_mfgr_hash)
    let n = res.row_count.min(5);
    for i in 0..n {
        let acctbal = f64::from_bits(res.columns[0].values[i]);
        let s_name_h = res.columns[1].values[i];
        let n_name_h = res.columns[2].values[i];
        let p_partkey = res.columns[3].values[i];
        let p_mfgr_h = res.columns[4].values[i];
        let s_addr_h = res.columns[5].values[i];
        let s_phone_h = res.columns[6].values[i];
        let s_comment_h = res.columns[7].values[i];
        println!("row[{}]: acctbal={:.6} s_name_h={:#018x} n_name_h={:#018x} p_partkey={} p_mfgr_h={:#018x} s_addr_h={:#018x} s_phone_h={:#018x} s_comment_h={:#018x}",
            i, acctbal, s_name_h, n_name_h, p_partkey, p_mfgr_h, s_addr_h, s_phone_h, s_comment_h);
    }
    // Also print the last row (row 99) for sanity
    if res.row_count >= 100 {
        let i = 99;
        let acctbal = f64::from_bits(res.columns[0].values[i]);
        let s_name_h = res.columns[1].values[i];
        let n_name_h = res.columns[2].values[i];
        let p_partkey = res.columns[3].values[i];
        println!("row[99]: acctbal={:.6} s_name_h={:#018x} n_name_h={:#018x} p_partkey={}",
            acctbal, s_name_h, n_name_h, p_partkey);
    }
}
