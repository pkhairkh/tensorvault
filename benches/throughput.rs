//! Throughput benchmarks for the bitcell module.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use bitcell::{Cell, CellColumn};
use bitcell::bitcell::scan;

fn bench_count_eq(c: &mut Criterion) {
    let mut group = c.benchmark_group("count_eq");
    group.throughput(Throughput::Elements(100_000));

    for size in [10_000, 100_000, 1_000_000] {
        let cells: Vec<Cell> = (0..size).map(|i| Cell::from_i32((i % 100) as i32)).collect();
        let target = Cell::from_i32(42);
        group.bench_function(format!("size={size}"), |b| {
            b.iter(|| black_box(scan::count_eq(black_box(&cells), black_box(target))));
        });
    }
    group.finish();
}

fn bench_count_similar(c: &mut Criterion) {
    let mut group = c.benchmark_group("count_similar");
    group.throughput(Throughput::Elements(100_000));

    let cells: Vec<Cell> = (0..100_000).map(|i| Cell::from_f64(i as f64)).collect();
    let target = Cell::from_f64(50_000.0);
    group.bench_function("hamming_le_8", |b| {
        b.iter(|| black_box(scan::count_similar(black_box(&cells), black_box(target), black_box(8))));
    });
    group.finish();
}

fn bench_sum_f64(c: &mut Criterion) {
    let mut group = c.benchmark_group("sum_f64");
    group.throughput(Throughput::Elements(100_000));

    let cells: Vec<Cell> = (0..100_000).map(|i| Cell::from_f64(i as f64)).collect();
    group.bench_function("100k_f64", |b| {
        b.iter(|| black_box(scan::sum_f64(black_box(&cells))));
    });
    group.finish();
}

fn bench_bsi_build(c: &mut Criterion) {
    let cells: Vec<Cell> = (0..10_000).map(|i| Cell::from_i32(i)).collect();
    c.bench_function("bsi_build_10k", |b| {
        b.iter(|| black_box(bitcell::bitcell::bsi::BitSlicedIndex::build(black_box(&cells))));
    });
}

criterion_group!(benches, bench_count_eq, bench_count_similar, bench_sum_f64, bench_bsi_build);
criterion_main!(benches);
