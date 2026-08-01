//! Verify Q10 vs DuckDB: print (c_custkey, c_name, revenue, c_acctbal, n_name,
//! c_address, c_phone, c_comment) sorted by revenue DESC.
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;
use std::collections::HashMap;

const Q10: &str = "SELECT c_custkey, c_name, sum(l_extendedprice * (1 - l_discount)) AS revenue, c_acctbal, n_name, c_address, c_phone, c_comment FROM customer, orders, lineitem, nation WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey AND o_orderdate >= date '1993-10-01' AND o_orderdate < date '1994-01-01' AND l_returnflag = 'R' AND c_nationkey = n_nationkey GROUP BY c_custkey, c_name, c_acctbal, n_name, c_address, c_phone, c_comment ORDER BY revenue DESC LIMIT 20";

fn main() {
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).expect("load");
        engine.register_table(Table::from_loaded(loaded));
    }
    // Build hash->name maps from customer, nation for human-readable output.
    let cust_loaded = read_tpch_csv("/tmp/tpch_customer.csv", "customer").expect("customer");
    let cust_tbl = Table::from_loaded(cust_loaded);
    let mut name_by_hash: HashMap<u64, String> = HashMap::new();
    let mut addr_by_hash: HashMap<u64, String> = HashMap::new();
    let mut phone_by_hash: HashMap<u64, String> = HashMap::new();
    let mut comment_by_hash: HashMap<u64, String> = HashMap::new();
    // customer col 1=c_name, 2=c_address, 4=c_phone, 7=c_comment
    if let (Some(ref sc_name), Some(ref sc_addr), Some(ref sc_phone), Some(ref sc_comment)) =
        (&cust_tbl.string_columns[1], &cust_tbl.string_columns[2], &cust_tbl.string_columns[4], &cust_tbl.string_columns[7]) {
        for i in 0..cust_tbl.row_count {
            name_by_hash.insert(cust_tbl.columns[1][i], sc_name.get(i).to_string());
            addr_by_hash.insert(cust_tbl.columns[2][i], sc_addr.get(i).to_string());
            phone_by_hash.insert(cust_tbl.columns[4][i], sc_phone.get(i).to_string());
            comment_by_hash.insert(cust_tbl.columns[7][i], sc_comment.get(i).to_string());
        }
    }
    let nat_loaded = read_tpch_csv("/tmp/tpch_nation.csv", "nation").expect("nation");
    let nat_tbl = Table::from_loaded(nat_loaded);
    let mut nname_by_hash: HashMap<u64, String> = HashMap::new();
    if let Some(ref sc) = nat_tbl.string_columns[1] {
        for i in 0..nat_tbl.row_count {
            nname_by_hash.insert(nat_tbl.columns[1][i], sc.get(i).to_string());
        }
    }

    let res = engine.execute_tpch(Q10).expect("Q10");
    println!("Q10 result: {} rows, {} cols", res.row_count, res.columns.len());
    for (ci, col) in res.columns.iter().enumerate() {
        println!("col[{}] name='{}' first3={:?}", ci, col.name, &col.values[..3]);
    }
    println!("--- turboGP (c_custkey, c_name, revenue, c_acctbal, n_name) ---");
    for r in 0..res.row_count {
        let ck = res.columns[0].values[r];
        let name_h = res.columns[1].values[r];
        let rev = f64::from_bits(res.columns[2].values[r]);
        let acct = f64::from_bits(res.columns[3].values[r]);
        let nname_h = res.columns[4].values[r];
        let name = name_by_hash.get(&name_h).cloned().unwrap_or_else(|| format!("?{}", name_h));
        let nname = nname_by_hash.get(&nname_h).cloned().unwrap_or_else(|| format!("?{}", nname_h));
        println!("{},{},{:.4},{:.4},{}", ck, name, rev, acct, nname);
    }
    println!("--- top 5 (c_custkey, revenue) ---");
    for r in 0..5.min(res.row_count) {
        let ck = res.columns[0].values[r];
        let rev = f64::from_bits(res.columns[2].values[r]);
        println!("{},{}", ck, rev);
    }
}
