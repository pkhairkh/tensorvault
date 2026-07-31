use turbogp::datasource::csv::read_tpch_csv;
use turbogp::datasource::table::Table;
use turbogp::engine::QueryEngine;

const Q1: &str = "SELECT l_returnflag, l_linestatus, sum(l_quantity) AS sum_qty, sum(l_extendedprice) AS sum_base_price, sum(l_extendedprice * (1 - l_discount)) AS sum_disc_price, sum(l_extendedprice * (1 - l_discount) * (1 + l_tax)) AS sum_charge, avg(l_quantity) AS avg_qty, avg(l_extendedprice) AS avg_price, avg(l_discount) AS avg_disc, count(*) AS count_order FROM lineitem WHERE l_shipdate <= date '1998-09-02' GROUP BY l_returnflag, l_linestatus ORDER BY l_returnflag, l_linestatus";

fn main() {
    let mut engine = QueryEngine::new();
    let loaded = read_tpch_csv("/tmp/tpch_lineitem.csv", "lineitem").expect("load");
    engine.register_table(Table::from_loaded(loaded));
    let res = engine.execute_tpch(Q1).expect("Q1");
    for col in &res.columns {
        print!("{:>20}", col.name);
    }
    println!();
    for r in 0..res.row_count {
        for col in &res.columns {
            let v = col.values[r];
            // Try as f64
            let f = f64::from_bits(v);
            if f.abs() > 1e15 || (f != 0.0 && f.abs() < 1e-300) {
                print!("{:>20}", v);
            } else {
                print!("{:>20.4}", f);
            }
        }
        println!();
    }
}
