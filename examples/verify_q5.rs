//! Verify Q5 vs DuckDB: print (n_name, revenue) sorted by revenue DESC.
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;
use std::collections::HashMap;

const Q5: &str = "SELECT n_name, sum(l_extendedprice * (1 - l_discount)) AS revenue FROM customer, orders, lineitem, supplier, nation, region WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey AND l_suppkey = s_suppkey AND c_nationkey = s_nationkey AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'ASIA' AND o_orderdate >= date '1994-01-01' AND o_orderdate < date '1995-01-01' GROUP BY n_name ORDER BY revenue DESC";

fn main() {
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).expect("load");
        engine.register_table(Table::from_loaded(loaded));
    }
    // Build hash->name map from nation table for human-readable output.
    let nat_loaded = read_tpch_csv("/tmp/tpch_nation.csv", "nation").expect("nation");
    let nat_tbl = Table::from_loaded(nat_loaded);
    let mut nname_by_hash: HashMap<u64, String> = HashMap::new();
    if let Some(ref sc) = nat_tbl.string_columns[1] {
        for i in 0..nat_tbl.row_count {
            nname_by_hash.insert(nat_tbl.columns[1][i], sc.get(i).to_string());
        }
    }

    let res = engine.execute_tpch(Q5).expect("Q5");
    println!("Q5 result: {} rows, {} cols", res.row_count, res.columns.len());
    for (ci, col) in res.columns.iter().enumerate() {
        println!("col[{}] name='{}' first3={:?}", ci, col.name, &col.values[..3]);
    }
    println!("--- turboGP (n_name, revenue) ---");
    for r in 0..res.row_count {
        let n_hash = res.columns[0].values[r];
        let rev_bits = res.columns[1].values[r];
        let rev = f64::from_bits(rev_bits);
        let name = nname_by_hash.get(&n_hash).cloned().unwrap_or_else(|| format!("hash={}", n_hash));
        println!("  {:>15}  {:>20.4}", name, rev);
    }
    println!("--- DuckDB ground truth ---");
    println!("  {:>15}  {:>20.4}", "INDONESIA", 55502041.1697_f64);
    println!("  {:>15}  {:>20.4}", "VIETNAM", 55295086.9967_f64);
    println!("  {:>15}  {:>20.4}", "CHINA", 53724494.2566_f64);
    println!("  {:>15}  {:>20.4}", "INDIA", 52035512.0002_f64);
    println!("  {:>15}  {:>20.4}", "JAPAN", 45410175.6954_f64);
}
