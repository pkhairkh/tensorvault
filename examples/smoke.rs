//! End-to-end smoke test of the instruction-first engine.
//!
//! Walks through the full turboGP pipeline:
//!
//! 1. **Hardware detection** — CPUID, NUMA, CXL.
//! 2. **Kernel table** — the moat: per-(CPU, tier) tuned kernels.
//! 3. **Protocol coordinators** — CXL (single-rack) and Raft (cross-rack),
//!    both issuing HLC timestamps.
//! 4. **SQL → plan → lower → execute** — parses
//!    `SELECT COUNT(*) FROM users WHERE id = 42`, builds a logical plan,
//!    lowers it via `PlanLowerer`, and executes the kernel invocations.
//! 5. **HLL count-distinct** — sketches for streaming distinct-count.
//! 6. **LSH similarity search** — locality-sensitive hash index for vector
//!    similarity.
//! 7. **HLC timestamps** — the protocol coordinator's monotonic timestamps.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example smoke
//! ```

use std::sync::Arc;
use std::time::Instant;
use turbogp::{
    executor::Scheduler,
    index::lsh::LshIndex,
    kernel::KernelTable,
    memory::{region::Region, tier::MemoryTier, NumaTopology},
    planner::{CostModel, PlanLowerer},
    protocol::{CxlCoordinator, HlcClock, RaftCoordinator},
    sketch::hll::HyperLogLog,
    sql::{build_plan, parse_with_extensions},
    storage::PAGE_CELLS,
};

fn main() {
    println!("=== turbogp instruction-first engine — smoke test ===\n");

    // -----------------------------------------------------------------
    // 1. Hardware detection
    // -----------------------------------------------------------------
    let table = KernelTable::new();
    println!("[1] Hardware detection");
    println!("    Detected CPU: {}", table.detected_cpu().name());
    println!("    Registered kernels: {}", table.list().len());
    println!();

    let topo = NumaTopology::detect();
    print!("{}", topo.dump());

    // -----------------------------------------------------------------
    // 2. Protocol coordinators (with HLC timestamps)
    // -----------------------------------------------------------------
    println!("[2] Protocol coordinators (HLC timestamps)");

    let mut cxl = CxlCoordinator::new(None);
    println!("    CXL available: {}", cxl.is_available());
    let cxl_ts = cxl.commit(0).unwrap();
    println!("    CXL single-rack commit ts: {}", cxl_ts);

    let mut raft = RaftCoordinator::new_as_leader(3, 0, None);
    let raft_ts = raft.commit(0).unwrap();
    println!(
        "    Raft cluster: {} nodes, quorum {}, leader={}, commit ts={}",
        raft.cluster_size,
        raft.quorum(),
        raft.is_leader(),
        raft_ts,
    );
    println!();

    // Standalone HLC clock demonstration: issue three timestamps and show
    // that they are strictly monotonic. This is what every commit on the
    // engine ultimately calls (whether via CXL or Raft).
    println!("    HLC clock (standalone, three calls):");
    let mut clock = HlcClock::new();
    let mut prev = clock.now();
    println!("      ts[0] = {}", prev);
    for i in 1..=2 {
        let ts = clock.now();
        assert!(ts > prev, "HLC must be monotonic");
        println!("      ts[{i}] = {}  (strictly greater)", ts);
        prev = ts;
    }
    println!();

    // -----------------------------------------------------------------
    // 3. SQL → plan → lower → execute (full pipeline)
    // -----------------------------------------------------------------
    println!("[3] SQL → plan → lower → execute");
    let sql = "SELECT COUNT(*) FROM users WHERE id = 42";
    println!("    SQL: {sql}");

    let (query, ext) = parse_with_extensions(sql).expect("parse_with_extensions");
    println!("    Parsed: {} select item(s), from '{}'", query.select.len(), query.from);
    println!("    Extensions: {:?}", ext);

    let plan = build_plan(&query, &ext);
    println!("    Plan: {:?}", plan.root);

    // Lower the plan to kernel invocations via the cost-aware PlanLowerer.
    let kernel_table = Arc::new(KernelTable::new());
    let lowerer = PlanLowerer::new(CostModel::default(), kernel_table.clone());
    let invocations = lowerer.lower(&plan);
    println!("    Lowered to {} kernel invocation(s):", invocations.len());
    for (i, inv) in invocations.iter().enumerate() {
        println!(
            "      [{i}] {:?} on region {} (tier {}, target={})",
            inv.operator,
            inv.region_id,
            inv.tier.name(),
            inv.params.target_u64,
        );
    }

    // Execute the invocations. We need to register a region for the "users"
    // table at the same region_id the lowerer derived from the table name.
    // The first invocation is the scan; its region_id is the hashed table
    // name. We synthesise a region of 1024 cells where exactly one cell
    // equals 42 (the WHERE predicate target).
    let sched = Scheduler::new(kernel_table);
    let scan_region_id = invocations[0].region_id;
    let users_cells: Vec<u64> =
        (0..1024).map(|i| if i == 42 { 42 } else { (i % 7) as u64 + 100 }).collect();
    let mut bytes = vec![0u8; users_cells.len() * 8];
    for (i, &c) in users_cells.iter().enumerate() {
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&c.to_le_bytes());
    }
    let region = Arc::new(Region::from_bytes(scan_region_id, MemoryTier::L3, &bytes));
    sched.register_region(region);
    println!("    Registered region {scan_region_id} (1024 cells, tier=L3)");

    // Execute the scan invocation. The plan lowerer does not know the
    // region's size at lower time, so it leaves `cell_count = 0`; we patch
    // it to the region's actual cell count before dispatching. The scan's
    // target_u64 (42) is preserved from the lowered invocation.
    let mut scan_inv = invocations[0].clone();
    scan_inv.params.cell_count = sched.get_region(scan_region_id).unwrap().cell_count();
    let scan_result = sched.execute_invocation(&scan_inv).unwrap();
    println!("    Executed scan: count={} (expected 1 — only cell[42] == 42)", scan_result.count,);

    // The aggregate invocation has region_id = 0 (aggregates read from the
    // previous output, not from a region). The scheduler skips it, so we
    // treat the scan's count as the final COUNT(*) result.
    println!("    Final COUNT(*) = {}", scan_result.count);
    println!();

    // -----------------------------------------------------------------
    // 4. HLL count-distinct
    // -----------------------------------------------------------------
    println!("[4] HyperLogLog count-distinct");
    let mut hll = HyperLogLog::new(14); // m = 16 384 registers, RSE ≈ 0.81 %.
    println!("    HLL precision: p={} (m={} registers)", hll.precision(), hll.len());

    // Insert 10 000 distinct hashes, each duplicated 5× (so the true
    // distinct count is exactly 10 000). We use xxh3 (the engine's standard
    // hash) so the hashes have the dispersion HLL expects — a naive
    // multiplicative hash produces 50 %+error.
    let true_distinct = 10_000u64;
    for i in 0..true_distinct {
        let hash = xxhash_rust::xxh3::xxh3_64(&i.to_le_bytes());
        for _ in 0..5 {
            hll.add(hash);
        }
    }
    let estimate = hll.estimate();
    let err = ((estimate - true_distinct as f64) / true_distinct as f64).abs() * 100.0;
    println!(
        "    Inserted {} hashes (5× duplicates of {} distinct)",
        true_distinct * 5,
        true_distinct,
    );
    println!("    HLL estimate: {:.0} (true: {true_distinct}, error: {err:.2}%)", estimate,);
    println!();

    // -----------------------------------------------------------------
    // 5. LSH similarity search
    // -----------------------------------------------------------------
    println!("[5] LSH similarity search");
    let mut lsh = LshIndex::new(4, 8, 4, 0xC0FFEE);
    println!(
        "    LSH index: dim={}, tables={}, hashes_per_table={}",
        lsh.dim(),
        lsh.num_tables(),
        lsh.num_hashes(),
    );

    // Insert 100 vectors. Vectors 0..50 are near [1.0, 0.0, 0.0, 0.0];
    // vectors 50..100 are near [0.0, 1.0, 0.0, 0.0].
    for i in 0..100u64 {
        let v = if i < 50 {
            vec![1.0, (i as f64) * 0.001, 0.0, 0.0]
        } else {
            vec![0.0, 1.0, (i as f64) * 0.001, 0.0]
        };
        lsh.insert(i, &v);
    }
    println!("    Inserted 100 vectors (50 near [1,0,0,0], 50 near [0,1,0,0])");

    // Query for a vector very close to the first cluster.
    let query = vec![1.0, 0.005, 0.0, 0.0];
    let hits = lsh.query(&query);
    println!("    Query for {:?} → {} candidate(s)", query, hits.len());
    if !hits.is_empty() {
        let from_first_cluster = hits.iter().filter(|&&id| id < 50).count();
        println!(
            "      {} from cluster 1 (id < 50), {} from cluster 2 (id ≥ 50)",
            from_first_cluster,
            hits.len() - from_first_cluster,
        );
    }
    println!();

    // -----------------------------------------------------------------
    // 6. Direct kernel throughput demonstration
    // -----------------------------------------------------------------
    println!("[6] Kernel throughput (scan_eq, sum_f64, count_similar)");
    let cell_count = 262_144; // exactly one region (2 MB / 8 B)
    let mut bytes = vec![0u8; cell_count * 8];
    for i in 0..cell_count {
        let v = (i % 1000) as u64;
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&v.to_le_bytes());
    }
    let region = Arc::new(Region::from_bytes(0, MemoryTier::L3, &bytes));
    let sched2 = Scheduler::new(Arc::new(KernelTable::new()));
    sched2.register_region(region);

    let start = Instant::now();
    let count = sched2.scan_eq(0, 42).unwrap();
    let elapsed = start.elapsed();
    println!(
        "    scan_eq(target=42): count={}, {:?} ({:.0} M cells/sec)",
        count,
        elapsed,
        cell_count as f64 / elapsed.as_secs_f64() / 1_000_000.0,
    );

    // sum_f64: refill the region with f64 values.
    let mut bytes = vec![0u8; cell_count * 8];
    for i in 0..cell_count {
        let v = (i as f64) + 1.0;
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&v.to_bits().to_le_bytes());
    }
    let region = Arc::new(Region::from_bytes(0, MemoryTier::L3, &bytes));
    let sched3 = Scheduler::new(Arc::new(KernelTable::new()));
    sched3.register_region(region);

    let start = Instant::now();
    let sum = sched3.sum_f64(0).unwrap();
    let elapsed = start.elapsed();
    let expected: f64 = (1..=cell_count).map(|i| i as f64).sum();
    println!(
        "    sum_f64: sum={:.0} (expected {:.0}), {:?} ({:.0} M cells/sec)",
        sum,
        expected,
        elapsed,
        cell_count as f64 / elapsed.as_secs_f64() / 1_000_000.0,
    );

    let start = Instant::now();
    let count = sched3.count_similar(0, 1u64, 0).unwrap();
    let elapsed = start.elapsed();
    println!(
        "    count_similar(target=1, d=0): count={}, {:?} ({:.0} M cells/sec)",
        count,
        elapsed,
        cell_count as f64 / elapsed.as_secs_f64() / 1_000_000.0,
    );
    println!();

    // -----------------------------------------------------------------
    // 7. Storage geometry
    // -----------------------------------------------------------------
    println!("[7] Storage geometry");
    println!("    Page size: 4 KB ({} cells)", PAGE_CELLS);
    println!("    Region size: 2 MB ({} pages)", 2 * 1024 * 1024 / 4096);
    println!("    Tablet size: 2 GB ({} regions)", 1024);
    println!();

    // -----------------------------------------------------------------
    // 8. Kernel table
    // -----------------------------------------------------------------
    println!("[8] Kernel table ({} entries)", sched3.kernel_table.list().len());
    for (op, cpu, tier, name) in sched3.kernel_table.list() {
        println!("    {:?} / {} / {} → {}", op, cpu.name(), tier.name(), name);
    }
    println!();

    println!("=== smoke test complete ===");
}
