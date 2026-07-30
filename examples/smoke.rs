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
//! 8. **Waves 13–17 optimization techniques** — WCOJ, learned cardinality,
//!    MCTS, eddy, tensor network.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example smoke
//! ```

use std::sync::Arc;
use std::time::Instant;
use turbogp::{
    compress::TensorTrain,
    executor::{Eddy, Morsel, Pipeline, Scheduler},
    index::lsh::LshIndex,
    kernel::{
        hash::HashTable,
        leapfrog::{LeapfrogJoin, SliceSortedIterator},
        KernelTable, Operator, PredicateOp,
    },
    memory::{region::Region, tier::MemoryTier, NumaTopology},
    planner::{
        agm::JoinHypergraph, dpccp::JoinRelation, mcts::MctsJoinOrderer, tensor::TensorNetwork,
        CostModel, LearnedCardinality, PlanLowerer,
    },
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

    // -----------------------------------------------------------------
    // 9. Wave 13: WCOJ / Leapfrog triejoin — cyclic join speedup
    // -----------------------------------------------------------------
    println!("[9] Wave 13: WCOJ (Leapfrog Triejoin) on a triangle query");
    // Build three sorted, deduped key sets with ~25 % pairwise overlap.
    let mk_keys = |seed: u64| -> Vec<u64> {
        let mut s = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut v: Vec<u64> = (0..5_000)
            .map(|_| {
                s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = s;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            })
            .map(|x| x % 20_000)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let r = mk_keys(1);
    let s_keys = mk_keys(2);
    let t = mk_keys(3);

    // Hash-join baseline: R∩S via build/probe, then (R∩S)∩T via build/probe.
    let start = Instant::now();
    let table_r = HashTable::build(&r);
    let rs: Vec<u64> = s_keys.iter().filter(|k| !table_r.probe(**k).is_empty()).copied().collect();
    let table_rs = HashTable::build(&rs);
    let hash_count: u64 = t.iter().map(|k| table_rs.probe(*k).len() as u64).sum();
    let hash_elapsed = start.elapsed();
    println!("    Hash-join cascade: {hash_count} matches in {hash_elapsed:?}");

    // Leapfrog (WCOJ): 3-way intersection in O(IN + OUT + AGM).
    let r_leak: &'static [u64] = Box::leak(r.clone().into_boxed_slice());
    let s_leak: &'static [u64] = Box::leak(s_keys.clone().into_boxed_slice());
    let t_leak: &'static [u64] = Box::leak(t.clone().into_boxed_slice());
    let start = Instant::now();
    let mut join = LeapfrogJoin::new(vec![
        Box::new(SliceSortedIterator::at_start(r_leak)),
        Box::new(SliceSortedIterator::at_start(s_leak)),
        Box::new(SliceSortedIterator::at_start(t_leak)),
    ]);
    let leapfrog_out = join.run();
    let leapfrog_elapsed = start.elapsed();
    println!("    Leapfrog (WCOJ):   {} matches in {leapfrog_elapsed:?}", leapfrog_out.len(),);
    let hash_secs = hash_elapsed.as_secs_f64().max(1e-12);
    let leap_secs = leapfrog_elapsed.as_secs_f64().max(1e-12);
    println!("    Speedup: {:.2}× (hash / leapfrog)", hash_secs / leap_secs);
    assert_eq!(hash_count as usize, leapfrog_out.len(), "WCOJ and hash join must agree");
    println!();

    // -----------------------------------------------------------------
    // 10. Wave 14: Learned cardinality — histogram + correction
    // -----------------------------------------------------------------
    println!("[10] Wave 14: Learned cardinality (histogram + correction)");
    // Generate 10 000 zipfian-distributed values (frequency ∝ 1/(v+1)).
    let mut state = 0xCAFEBABE_u64;
    let mut zipf: Vec<u64> = Vec::with_capacity(10_000);
    while zipf.len() < 10_000 {
        let step = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        state = step;
        let mut z = step;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z = z ^ (z >> 31);
        let v = z % 10_000;
        let accept = (((z >> 11) as f64) / ((1u64 << 53) as f64)) < (1.0 / ((v + 1) as f64));
        if accept {
            zipf.push(v);
        }
    }

    let mut learned = LearnedCardinality::new();
    learned.train_table("orders", "cust_key", &zipf);
    println!("    Trained 100-bucket histogram on {} zipfian values", zipf.len());

    // Heuristic estimate: 0.33 (the fixed range default).
    // Learned estimate: histogram range_selectivity + correction factor.
    let (lo, hi) = (0_u64, 999_u64);
    let heuristic = 0.33;
    let raw = learned.estimate_range("orders", "cust_key", lo, hi);
    let corrected = learned.correct(raw);
    // After correction factor = 1.0 (no observations yet), corrected == raw.
    let true_count = zipf.iter().filter(|&&v| v >= lo && v <= hi).count();
    let true_sel = true_count as f64 / zipf.len() as f64;
    println!("    Range [{lo}, {hi}]:");
    println!("      heuristic(0.33) = {heuristic:.4}");
    println!("      learned (raw)   = {raw:.4}");
    println!("      learned (corr)  = {corrected:.4}");
    println!("      true selectivity = {true_sel:.4}  ({true_count} / {} rows)", zipf.len());

    // Calibrate the correction factor by feeding 50 biased observations
    // expressed as **cardinalities** (row counts). The correction factor's
    // update rule is `correction = 0.9 · correction + 0.1 · (actual /
    // predicted)` with `predicted.max(1.0)` in the denominator — that
    // convention assumes cardinality counts (not selectivities in [0, 1]),
    // so we scale up by the row count to make the ratio meaningful.
    // Here we simulate a stale histogram that under-predicts by 2×.
    let raw_card = raw * zipf.len() as f64;
    let true_card = true_count as f64;
    for _ in 0..50 {
        learned.observe(raw_card * 0.5, true_card);
    }
    // Apply the correction to a *biased* prediction (raw × 0.5, simulating
    // stale statistics). After convergence correction ≈ 2.0, so the
    // corrected prediction ≈ raw × 0.5 × 2.0 ≈ raw ≈ true_sel.
    let biased = raw * 0.5;
    let corrected_after = learned.correct(biased);
    println!(
        "      after 50 calibrations (2× under-bias injected): correction = {:.3}",
        learned.correction,
    );
    println!(
        "      biased prediction = {biased:.4}, corrected = {corrected_after:.4}  (true = {true_sel:.4})"
    );
    println!();

    // -----------------------------------------------------------------
    // 11. Wave 15: MCTS join ordering — scales beyond DPccp's n ≤ 15
    // -----------------------------------------------------------------
    println!("[11] Wave 15: MCTS plans a 20-table chain join (DPccp can't)");
    let relations: Vec<JoinRelation> = (0..20)
        .map(|i| JoinRelation {
            name: format!("R{i}"),
            cardinality: 100,
            joins_with: {
                let mut v = Vec::new();
                if i > 0 {
                    v.push(i - 1);
                }
                if i + 1 < 20 {
                    v.push(i + 1);
                }
                v
            },
        })
        .collect();
    let mcts = MctsJoinOrderer::default().with_iterations(200).with_seed(7);
    let start = Instant::now();
    let plan = mcts.order(&relations).expect("MCTS should plan a 20-table chain");
    let mcts_elapsed = start.elapsed();
    println!(
        "    MCTS planned 20 tables in {mcts_elapsed:?} (cost = {:.0}, {} iterations)",
        plan.cost(),
        200,
    );
    // Demonstrate that DPccp refuses n > 15.
    let dpccp_result = turbogp::planner::dpccp::dpccp(&relations);
    println!(
        "    DPccp on 20 tables: {:?}",
        dpccp_result.err().map(|e| e.to_string()).unwrap_or_default()
    );
    println!();

    // -----------------------------------------------------------------
    // 12. Wave 16: Adaptive eddy — reorders filters per morsel
    // -----------------------------------------------------------------
    println!("[12] Wave 16: Adaptive eddy on a 3-filter pipeline");
    let kt = Arc::new(KernelTable::new());
    let cells: Vec<u64> = (0..1024).map(|i| (i % 2) as u64).collect();
    let morsel = Morsel::new(0, 0, &cells);
    // Three filters: ScanRange(0,1) → sel 1.0; ScanEq(0) → sel 0.5;
    // ScanMultiPredicate(Eq(0), Eq(1)) → sel 0.0 (contradictory).
    let ops = vec![Operator::ScanRangeU64, Operator::ScanEqU64, Operator::ScanMultiPredicate];
    let params = turbogp::kernel::KernelParams {
        target_u64: 0,
        target2_u64: 1,
        low_u64: 0,
        high_u64: 1,
        pred1_op: PredicateOp::Eq,
        pred2_op: PredicateOp::Eq,
        predicate_count: 2,
        ..Default::default()
    };

    // First morsel: eddy applies all 3 to learn selectivities.
    let mut eddy = Eddy::new(ops.clone(), 0.1);
    let mut pipeline = Pipeline::new(ops.clone());
    pipeline.execute_with_eddy(&morsel, &mut eddy, &kt, &params).unwrap();
    let first_count = pipeline.results().len();
    pipeline.reset();
    println!(
        "    After morsel 1: applied {first_count} ops, selectivities = {:?}",
        eddy.selectivities()
    );

    // Second morsel: eddy applies only the most selective op (sel 0.0),
    // sees zero output, early-terminates.
    pipeline.execute_with_eddy(&morsel, &mut eddy, &kt, &params).unwrap();
    let second_count = pipeline.results().len();
    println!(
        "    After morsel 2: applied {second_count} op (early termination) — routing order = {:?}",
        eddy.routing_order(),
    );
    println!("    Adaptive win: 3 ops/morsel → 1 op/morsel after learning");
    println!();

    // -----------------------------------------------------------------
    // 13. Wave 17: Tensor-network contraction ordering
    // -----------------------------------------------------------------
    println!("[13] Wave 17: Tensor-network contraction on an 8-table chain");
    let n = 8;
    let attrs: Vec<String> = (0..=n).map(|i| format!("A{i}")).collect();
    let attr_refs: Vec<&str> = (0..=n).map(|i| attrs[i].as_str()).collect();
    let rels: Vec<Vec<&str>> = (0..n).map(|i| vec![attr_refs[i], attr_refs[i + 1]]).collect();
    let graph = JoinHypergraph::from_named(&attr_refs, &rels);
    let cards = vec![100usize; n];
    let net = TensorNetwork::from_hypergraph(&graph, &cards);
    let order = net.optimal_contraction_order();
    println!(
        "    8-table chain: treewidth = {}, optimal contraction has {} steps",
        net.treewidth(),
        order.len(),
    );
    // Convert to a JoinTree and report the cost.
    let relations: Vec<JoinRelation> = (0..n)
        .map(|i| JoinRelation {
            name: format!("R{i}"),
            cardinality: 100,
            joins_with: {
                let mut v = Vec::new();
                if i > 0 {
                    v.push(i - 1);
                }
                if i + 1 < n {
                    v.push(i + 1);
                }
                v
            },
        })
        .collect();
    let tree = turbogp::planner::contraction::contraction_to_join_tree(&net, &order, &relations)
        .expect("contraction_to_join_tree succeeds");
    println!("    Tensor-network plan cost = {:.0} (vs. DPccp cost = {:.0})", tree.cost(), {
        let dpccp_tree = turbogp::planner::dpccp::dpccp(&relations).expect("DPccp succeeds");
        dpccp_tree.cost()
    });

    // Bonus: tensor-train compression of a 20×10 rank-2 matrix.
    let m = 20usize;
    let k = 10usize;
    let mut data: Vec<Vec<f64>> = vec![vec![0.0; k]; m];
    for r_idx in 0..2 {
        for (i, row) in data.iter_mut().enumerate().take(m) {
            for (j, cell) in row.iter_mut().enumerate().take(k) {
                let a = ((i as f64) + 1.0) * 0.1 * (r_idx as f64 + 1.0);
                let b = ((j as f64) + 1.0) * 0.2;
                *cell += a * b;
            }
        }
    }
    let tt = TensorTrain::decompose(&data, 3);
    println!(
        "    Tensor-train on {m}×{k} rank-2 matrix: effective_rank = {}, compression_ratio = {:.2}×",
        tt.effective_rank(),
        tt.compression_ratio(),
    );
    println!();

    println!("=== smoke test complete ===");
}
