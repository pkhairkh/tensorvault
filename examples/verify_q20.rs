//! Verify Q20 reformulation: print row count + first 5 s_name/s_address hashes.
//! Also prints diagnostics: forest part count.
use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

const Q20: &str = "SELECT s_name, s_address FROM supplier, nation WHERE s_suppkey IN (SELECT ps_suppkey FROM partsupp WHERE ps_partkey IN (SELECT p_partkey FROM part WHERE p_name LIKE 'forest%') AND ps_availqty > (SELECT 0.5 * sum(l_quantity) FROM lineitem WHERE l_partkey = ps_partkey AND l_suppkey = ps_suppkey AND l_shipdate >= date '1994-01-01' AND l_shipdate < date '1995-01-01')) AND s_nationkey = n_nationkey AND n_name = 'CANADA' ORDER BY s_name";

fn main() {
    let mut engine = QueryEngine::new();
    let tables = ["region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"];
    for t in &tables {
        let path = format!("/tmp/tpch_{}.csv", t);
        let loaded = read_tpch_csv(&path, t).expect("load");
        engine.register_table(Table::from_loaded(loaded));
    }

    // Diagnostics: count forest parts via the part StringSearchColumn.
    {
        let part = engine.catalog().get("part").expect("part table");
        let sc = part.string_columns[1].as_ref().expect("p_name StringSearchColumn");
        let mut n_forest = 0usize;
        for i in 0..part.row_count {
            if sc.get(i).as_bytes().starts_with(b"forest") {
                n_forest += 1;
            }
        }
        println!("forest parts (p_name LIKE 'forest%'): {}", n_forest);
    }

    let res = engine.execute_tpch(Q20).expect("Q20");
    println!("Q20: {} rows, {} cols", res.row_count, res.columns.len());
    println!("col names: {:?}", res.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
    let n = res.row_count.min(5);
    for i in 0..n {
        let s_name_h = res.columns[0].values[i];
        let s_addr_h = res.columns[1].values[i];
        println!("row[{}]: s_name_h={:#018x} s_addr_h={:#018x}", i, s_name_h, s_addr_h);
    }
    if res.row_count > 5 {
        let i = res.row_count - 1;
        let s_name_h = res.columns[0].values[i];
        let s_addr_h = res.columns[1].values[i];
        println!("row[{}]: s_name_h={:#018x} s_addr_h={:#018x}", i, s_name_h, s_addr_h);
    }
}
