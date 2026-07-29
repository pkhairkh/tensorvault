//! End-to-end smoke test of the instruction-first engine.

use std::sync::Arc;
use std::time::Instant;
use tensorvault::{
    executor::Scheduler,
    kernel::{CpuTarget, KernelTable, Operator},
    memory::{region::Region, tier::MemoryTier, NumaTopology},
    protocol::{CxlCoordinator, RaftCoordinator},
    storage::PAGE_CELLS,
};

fn main() {
    println!("=== TensorVault instruction-first engine — smoke test ===\n");

    // 1. Detect CPU.
    let table = KernelTable::new();
    println!("Detected CPU: {}", table.detected_cpu().name());
    println!("Registered kernels: {}", table.list().len());
    println!();

    // 2. Detect NUMA topology.
    let topo = NumaTopology::detect();
    print!("{}", topo.dump());

    // 3. Detect CXL.
    let cxl = CxlCoordinator::new();
    println!("CXL available: {}", cxl.is_available());
    if cxl.is_available() {
        println!("  Single-rack commit latency: ~{} ns", cxl.commit(0));
    }
    println!();

    // 4. Create a Raft coordinator for cross-rack.
    let raft = RaftCoordinator::new(3, 0);
    println!(
        "Raft cluster: {} nodes, quorum {}, commit ~{} ns",
        raft.cluster_size,
        raft.quorum(),
        raft.commit(0)
    );
    println!();

    // 5. Create a scheduler.
    let scheduler = Scheduler::new(Arc::new(table));

    // 6. Create a region with u64 cells. A region is 2 MB = 262144 u64 cells.
    let cell_count = 262_144; // exactly one region
    let mut bytes = vec![0u8; cell_count * 8];
    for i in 0..cell_count {
        let v = (i % 1000) as u64;
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&v.to_le_bytes());
    }
    let region = Arc::new(Region::from_bytes(0, MemoryTier::L3, &bytes));
    scheduler.register_region(region);
    println!(
        "Registered region 0: {} cells, tier={}",
        cell_count,
        MemoryTier::L3
    );
    println!();

    // 7. Run scan_eq: count cells equal to 42.
    let start = Instant::now();
    let count = scheduler.scan_eq(0, 42).unwrap();
    let elapsed = start.elapsed();
    println!(
        "scan_eq(target=42): count={}, {:?} ({:.0} M cells/sec)",
        count,
        elapsed,
        cell_count as f64 / elapsed.as_secs_f64() / 1_000_000.0
    );

    // 8. Run sum_f64: treat cells as f64 and sum.
    // First, refill the region with f64 values.
    let mut bytes = vec![0u8; cell_count * 8];
    for i in 0..cell_count {
        let v = (i as f64) + 1.0;
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&v.to_bits().to_le_bytes());
    }
    let region = Arc::new(Region::from_bytes(0, MemoryTier::L3, &bytes));
    let sched2 = Scheduler::new(Arc::new(KernelTable::new()));
    sched2.register_region(region);

    let start = Instant::now();
    let sum = sched2.sum_f64(0).unwrap();
    let elapsed = start.elapsed();
    let expected: f64 = (1..=cell_count).map(|i| i as f64).sum();
    println!(
        "sum_f64: sum={:.0} (expected {:.0}), {:?} ({:.0} M cells/sec)",
        sum,
        expected,
        elapsed,
        cell_count as f64 / elapsed.as_secs_f64() / 1_000_000.0
    );

    // 9. Run count_similar: Hamming distance.
    let start = Instant::now();
    let count = sched2.count_similar(0, 1u64, 0).unwrap();
    let elapsed = start.elapsed();
    println!(
        "count_similar(target=1, d=0): count={}, {:?} ({:.0} M cells/sec)",
        count,
        elapsed,
        cell_count as f64 / elapsed.as_secs_f64() / 1_000_000.0
    );

    // 10. Show page geometry.
    println!();
    println!("Storage geometry:");
    println!("  Page size: 4 KB ({} cells)", PAGE_CELLS);
    println!("  Region size: 2 MB ({} pages)", 2 * 1024 * 1024 / 4096);
    println!("  Tablet size: 2 GB ({} regions)", 1024);

    // 11. Show kernel table.
    println!();
    println!("Kernel table ({} entries):", sched2.kernel_table.list().len());
    for (op, cpu, tier, name) in sched2.kernel_table.list() {
        println!("  {:?} / {} / {} → {}", op, cpu.name(), tier.name(), name);
    }

    println!();
    println!("=== smoke test complete ===");
}
