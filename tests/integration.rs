//! Integration tests — end-to-end engine behavior.

use std::sync::Arc;
use tensorvault::{
    executor::Scheduler,
    kernel::KernelTable,
    memory::{region::Region, tier::MemoryTier},
};

/// The engine can scan a region and find matching cells.
#[test]
fn scan_eq_finds_matches() {
    let table = Arc::new(KernelTable::new());
    let sched = Scheduler::new(table);

    let cells: Vec<u64> = (0..10_000).map(|i| i % 7).collect();
    let mut bytes = vec![0u8; 2 * 1024 * 1024]; // 2 MB region
    for (i, &c) in cells.iter().enumerate() {
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&c.to_le_bytes());
    }

    let region = Arc::new(Region::from_bytes(0, MemoryTier::L3, &bytes));
    sched.register_region(region);

    let count = sched.scan_eq(0, 3).unwrap();
    assert!(count > 0, "should find some matches");
}

/// The engine can sum f64 cells correctly.
#[test]
fn sum_f64_produces_correct_result() {
    let table = Arc::new(KernelTable::new());
    let sched = Scheduler::new(table);

    let values: Vec<f64> = (1..=1000).map(|i| i as f64).collect();
    let mut bytes = vec![0u8; 2 * 1024 * 1024];
    for (i, &v) in values.iter().enumerate() {
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&v.to_bits().to_le_bytes());
    }

    let region = Arc::new(Region::from_bytes(0, MemoryTier::L3, &bytes));
    sched.register_region(region);

    let sum = sched.sum_f64(0).unwrap();
    let expected: f64 = (1..=1000).map(|i| i as f64).sum();
    assert!((sum - expected).abs() < 1e-3, "sum mismatch: {} vs {}", sum, expected);
}

/// The engine can count similar cells (Hamming distance).
#[test]
fn count_similar_finds_exact_matches() {
    let table = Arc::new(KernelTable::new());
    let sched = Scheduler::new(table);

    let cells: Vec<u64> = vec![42, 42, 42, 99, 42, 100, 42];
    let mut bytes = vec![0u8; 2 * 1024 * 1024];
    for (i, &c) in cells.iter().enumerate() {
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&c.to_le_bytes());
    }

    let region = Arc::new(Region::from_bytes(0, MemoryTier::L3, &bytes));
    sched.register_region(region);

    let count = sched.count_similar(0, 42, 0).unwrap();
    assert_eq!(count, 4, "should find 4 exact matches for 42");
}

/// Missing region returns an error.
#[test]
fn missing_region_returns_error() {
    let table = Arc::new(KernelTable::new());
    let sched = Scheduler::new(table);
    assert!(sched.scan_eq(999, 0).is_err());
}

/// Kernel table has kernels for the detected CPU.
#[test]
fn kernel_table_is_populated() {
    let table = KernelTable::new();
    assert!(!table.list().is_empty(), "kernel table should have kernels");
    assert!(table.detected_cpu().has_avx2() || table.detected_cpu().name() == "scalar");
}

/// MDL schema selection picks the right type.
#[test]
fn mdl_selects_f64_for_pure_floats() {
    use tensorvault::schema::schema_select;
    let chosen = schema_select(1000, 1000, 0, 0, 0);
    assert_eq!(chosen.name(), "f64");
}

/// MDL picks variant for mixed data.
#[test]
fn mdl_picks_variant_for_mixed() {
    use tensorvault::schema::schema_select;
    let chosen = schema_select(1000, 500, 500, 0, 0);
    assert_eq!(chosen.name(), "variant");
}
