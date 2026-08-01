//! Verify Q9 vs DuckDB: print (nation_name, year, sum_profit) sorted.
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;
use std::collections::HashMap;

const Q9: &str = "SELECT nation, o_year, sum(amount) AS sum_profit FROM (SELECT n_name AS nation, extract(year FROM o_orderdate) AS o_year, l_extendedprice * (1 - l_discount) - ps_supplycost * l_quantity AS amount FROM part, partsupp, lineitem, orders, supplier, nation WHERE s_suppkey = l_suppkey AND ps_suppkey = l_suppkey AND ps_partkey = l_partkey AND p_partkey = l_partkey AND o_orderkey = l_orderkey AND s_nationkey = n_nationkey AND p_name LIKE '%green%') AS profit GROUP BY nation, o_year ORDER BY nation, o_year DESC";

fn main() {
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).expect("load");
        engine.register_table(Table::from_loaded(loaded));
    }
    // Build hash->name map from the nation table (n_name col 1).
    let nat_loaded = read_tpch_csv("/tmp/tpch_nation.csv", "nation").expect("nation");
    let nat_tbl = Table::from_loaded(nat_loaded);
    let mut hash_to_name: HashMap<u64, String> = HashMap::new();
    if let Some(ref sc) = nat_tbl.string_columns[1] {
        for i in 0..nat_tbl.row_count {
            hash_to_name.insert(nat_tbl.columns[1][i], sc.get(i).to_string());
        }
    }
    let res = engine.execute_tpch(Q9).expect("Q9");
    println!("Q9 result: {} rows, {} cols", res.row_count, res.columns.len());
    for (ci, col) in res.columns.iter().enumerate() {
        println!("col[{}] name='{}' first3={:?}", ci, col.name, &col.values[..3]);
    }
    let mut rows: Vec<(String, i64, f64)> = Vec::with_capacity(res.row_count);
    for r in 0..res.row_count {
        let n_hash = res.columns[0].values[r];
        let year = res.columns[1].values[r] as i64;
        let sp = f64::from_bits(res.columns[2].values[r]);
        let name = hash_to_name.get(&n_hash).cloned().unwrap_or_else(|| format!("?{}", n_hash));
        rows.push((name, year, sp));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    println!("--- turboGP (nation, o_year, sum_profit) ---");
    for (n, y, sp) in &rows {
        println!("{},{},{:.4}", n, y, sp);
    }
}
