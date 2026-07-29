// Smoke test: build a mixed-type column, run all the bitcell operations.
use bitcell::{Cell, CellColumn};
use bitcell::bitcell::{bsi::BitSlicedIndex, hash::JoinTable, mdl, scan};

fn main() {
    println!("=== bitcell Phase-1 prototype smoke test ===\n");

    // 1. Build a mixed-type column (the killer feature).
    let mut col = CellColumn::new();
    col.push_i32(42);
    col.push_f64(3.14);
    col.push(Cell::from_bool(true));
    col.push_null();
    col.push(Cell::from_short_str(b"hello").unwrap());
    col.push_i32(42);
    col.push_f64(2.71);
    println!("Mixed-type column ({} cells, {} bytes):", col.len(), col.byte_size());
    for (i, c) in col.cells.iter().enumerate() {
        println!("  [{}] bits=0x{:016X} tag={:?}", i, c.to_bits(), c.tag());
    }

    // 2. MDL schema selection.
    let result = mdl::schema_select_with_diagnostics(&col);
    println!("\nMDL schema selection: chose = {}", result.chosen.name());
    for (t, dl) in &result.all_costs {
        println!("  {:10} total={:>10.1} bits (model={:.0}, data={:.1})",
            t.name(), dl.total(), dl.model_bits, dl.data_bits);
    }

    // 3. Scan: count_eq.
    let count = scan::count_eq(&col.cells, Cell::from_i32(42));
    println!("\ncount_eq(i32=42) = {}", count);

    // 4. Scan: count_similar (Hamming distance).
    let similar = scan::count_similar(&col.cells, Cell::from_f64(3.14), 30);
    println!("count_similar(f64=3.14, d<=30) = {}", similar);

    // 5. BSI: find_eq.
    let bsi = BitSlicedIndex::build(&col.cells);
    let matches = bsi.find_eq(Cell::from_i32(42));
    println!("BSI find_eq(i32=42) -> rows {:?}", matches.set_indices());

    // 6. Hash join on mixed types.
    let build: Vec<Cell> = vec![Cell::from_i32(42), Cell::from_f64(3.14), Cell::from_short_str(b"hi").unwrap()];
    let probe: Vec<Cell> = vec![Cell::from_f64(3.14), Cell::from_i32(99), Cell::from_short_str(b"hi").unwrap()];
    let table = JoinTable::build(build);
    let joined = table.probe_all(&probe);
    println!("\nHash join (mixed types): {} matches", joined.len());
    for (pi, bi) in &joined {
        println!("  probe[{}] <-> build[{}]", pi, bi);
    }

    // 7. Performance: large monomorphic column.
    let big: Vec<Cell> = (0..1_000_000).map(|i| Cell::from_f64((i as f64) + 1.0)).collect();
    let start = std::time::Instant::now();
    let _ = scan::sum_f64(&big);
    let elapsed = start.elapsed();
    println!("\nSum 1M f64 cells: {:?} ({:.0} cells/sec)",
        elapsed, 1_000_000.0 / elapsed.as_secs_f64());

    let start = std::time::Instant::now();
    let _ = scan::count_eq(&big, Cell::from_f64(500_000.0 + 1.0));
    let elapsed = start.elapsed();
    println!("count_eq 1M cells: {:?} ({:.0} cells/sec)",
        elapsed, 1_000_000.0 / elapsed.as_secs_f64());

    println!("\n=== smoke test complete ===");
}
