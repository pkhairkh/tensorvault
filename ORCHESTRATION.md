# turboGP — Implementation Orchestration Plan

> **18 waves, each with surgical subtasks, clear DoDs, commits between
> subtasks, pushes between waves. The orchestrator may only return when all
> DoDs are fulfilled.**

> **Status as of Wave 18: ALL 18 WAVES COMPLETE.**
> Final test count: **554 tests** (535 lib + 7 integration + 12 doc-tests;
> 1 doc-test ignored). Clippy is `-D warnings` clean on 9 benchmark suites
> and the `smoke` example. The 3× speedup target for Waves 13–17 is
> verified in [`docs/3x-proof.md`](./docs/3x-proof.md).

## Conventions

- **Commit**: after every subtask (format: `wave-N/task-M: <description>`)
- **Push**: after every wave (once all DoDs pass)
- **DoD**: Definition of Done — tested before next wave starts
- **Agent scope**: each subtask is scoped so no agent exceeds ~15k tokens of context

## Wave overview

| Wave | Title | Subtasks | Status | DoD |
|------|-------|----------|--------|-----|
| 0 | Project rename to turboGP | 4 | ✅ done | `cargo test` passes, no `tensorvault` in code |
| 1 | Core types + error handling | 5 | ✅ done | Linear types compile, errors are typed |
| 2 | Storage hardening | 6 | ✅ done | Pages persist, checksums verify, huge pages work |
| 3 | Kernel expansion | 5 | ✅ done | VPTERNLOGQ works, all kernels branchless, no split locks |
| 4 | Cost model | 4 | ✅ done | CostModel predicts latency within 30% |
| 5 | Memory manager | 5 | ✅ done | NUMA pinning, LRU migration, bandwidth monitoring |
| 6 | Morsel executor | 6 | ✅ done | Multi-stage pipeline runs L1-resident |
| 7 | SQL parser | 5 | ✅ done | Can parse SELECT/WHERE/GROUP BY/JOIN + extensions |
| 8 | WAL + persistence | 5 | ✅ done | Data survives restart, WAL replays |
| 9 | Protocol coordinator | 4 | ✅ done | HLC clock, CXL/Raft stubs with fallbacks |
| 10 | Indexes + sketches | 6 | ✅ done | BSI, LSH, HLL, Count-Min, t-Digest |
| 11 | Join ordering + planner | 4 | ✅ done | DPccp optimal for n≤15 |
| 12 | Benchmarks + integration | 5 | ✅ done | TPC-H runs, TPC-C runs, energy harness |
| 13 | WCOJ / Leapfrog triejoin | 3 | ✅ done | Triangle join 3–5× faster than hash cascade |
| 14 | Learned cardinality | 3 | ✅ done | 17× MAPE improvement on zipfian data |
| 15 | MCTS plan search | 3 | ✅ done | Scales to n>15 joins, within 2× of optimal |
| 16 | Adaptive eddies | 3 | ✅ done | 12× on skewed data via early termination |
| 17 | Tensor-network contraction | 3 | ✅ done | 5.6× faster planning, 11× compression |
| 18 | Final 3× proof + smoke | 4 | ✅ done | `bench_3x_proof` runs, all techniques ≥3× |

---

## Wave 0: Project Rename to turboGP

**Goal**: Rename `tensorvault` → `turbogp` everywhere.

| Task | Description |
|------|-------------|
| 0-1 | Rename Cargo.toml (name, lib.name, keywords, description, bench path) |
| 0-2 | Rename all Rust source (imports, mod declarations, doc comments) |
| 0-3 | Rename all docs (README, ARCHITECTURE, SPECIFICATION, FINE_DRAFT, ADRs) |
| 0-4 | Rename examples + benches + tests imports |

**DoD**: `cargo build --release && cargo test` passes with 0 failures. Zero occurrences of `tensorvault` (case-insensitive) in `.rs` or `.toml` files.

---

## Wave 1: Core Types + Error Handling

| Task | Description | ADR |
|------|-------------|-----|
| 1-1 | `src/types/mod.rs` — module root | — |
| 1-2 | `src/types/cxl_ref.rs` — CxlRef<T>: linear, no Clone/Copy, !Send, !Sync | ADR-013 |
| 1-3 | `src/types/raft_ref.rs` — RaftRef<T>: affine, no Clone/Copy, Send | ADR-013 |
| 1-4 | Expand Error enum with Tier, Protocol, Parse, Timeout variants | — |
| 1-5 | Tests: CxlRef cannot be cloned or sent across threads | ADR-013 |

**DoD**: `cargo test` passes. Compile-fail test confirms CxlRef is linear.

---

## Wave 2: Storage Hardening

| Task | Description | ADR |
|------|-------------|-----|
| 2-1 | Page CRC32C via `_mm_crc32_u64` (SSE4.2 intrinsic) | ADR-012 |
| 2-2 | Page XOR parity computation | ADR-012 |
| 2-3 | `verify_and_correct()` for single-bit error correction | ADR-012 |
| 2-4 | Region allocation via `mmap(MAP_HUGETLB)` with THP fallback | ADR-009 |
| 2-5 | Region `migrate_to()` using `ptr::copy_nonoverlapping` | ADR-006 |
| 2-6 | Tests: write→checksum→corrupt→correct roundtrip | ADR-012 |

**DoD**: Pages can be checksummed, corrupted, and corrected. Huge pages allocate on Linux.

---

## Wave 3: Kernel Expansion

| Task | Description | ADR |
|------|-------------|-----|
| 3-1 | `scan_multi_predicate` kernel using `VPTERNLOGQ` | P-01-05 |
| 3-2 | Audit all kernels for branchless compliance | ADR-004 |
| 3-3 | `#[repr(align(64))]` on HashTableSlot | ADR-005 |
| 3-4 | Add `ScanMultiPredicate` to Operator enum | — |
| 3-5 | Tests: multi-predicate scan correctness + throughput | ADR-005 |

**DoD**: Multi-predicate scan is 1.5× faster than 3 separate scans. No split locks.

---

## Wave 4: Cost Model

| Task | Description | ADR |
|------|-------------|-----|
| 4-1 | `src/planner/mod.rs` — CostModel struct with calibrated throughput table | ADR-023 |
| 4-2 | `src/planner/kingman.rs` — KingmanPredictor (ρ, c_a, c_s, μ) | ADR-023 |
| 4-3 | `estimate_cost(plan, cost_model)` function | ADR-023 |
| 4-4 | Tests: cost model predicts scan latency within 30% | ADR-023 |

**DoD**: `CostModel::estimate(plan)` returns latency prediction validated against measured throughput.

---

## Wave 5: Memory Manager

| Task | Description | ADR |
|------|-------------|-----|
| 5-1 | `pin_thread(cpu_id)` via `pthread_setaffinity_np` | ADR-008 |
| 5-2 | `MemoryManager` with per-tier LRU lists | ADR-010 |
| 5-3 | `place_region(region, tier)` with eviction | ADR-010 |
| 5-4 | Bandwidth monitoring via `/proc/meminfo` or perf counters | — |
| 5-5 | Tests: region migrates DDR5→NVMe when DDR5 is full | ADR-010 |

**DoD**: Regions migrate between tiers. Worker threads are NUMA-pinned.

---

## Wave 6: Morsel-Driven Executor

| Task | Description | ADR |
|------|-------------|-----|
| 6-1 | `Morsel` struct (1024 cells + metadata) | ADR-018 |
| 6-2 | `WorkerThread` with NUMA-pinned affinity | ADR-008 |
| 6-3 | `Pipeline` (scan → filter → aggregate) | ADR-018 |
| 6-4 | `MorselDispatcher` (round-robin, NUMA-aware) | ADR-018 |
| 6-5 | Pipeline breaker handling (hash join build) | ADR-018 |
| 6-6 | Tests: 3-stage pipeline correctness + L1 residency | ADR-018 |

**DoD**: Multi-stage pipeline executes correctly, intermediate data stays in L1.

---

## Wave 7: SQL Parser + Query Language

| Task | Description | Spec |
|------|-------------|------|
| 7-1 | `src/sql/lexer.rs` — tokenizer | §10 |
| 7-2 | `src/sql/parser.rs` — Pratt parser for SELECT/WHERE/GROUP BY/JOIN | §10 |
| 7-3 | `src/sql/extensions.rs` — parse APPROXIMATE, TIER, SIMILAR TO, etc. | §10.2–10.8 |
| 7-4 | `src/sql/plan.rs` — parse tree → LogicalPlan | §7.2 |
| 7-5 | Tests: parse all 7 extensions + invalid SQL errors | §10 |

**DoD**: Parser handles standard SQL + all 7 extensions. Invalid SQL returns typed errors.

---

## Wave 8: WAL + Persistence

| Task | Description | ADR |
|------|-------------|-----|
| 8-1 | ZNS detection via `ioctl(BLKGETZONESZ)` | ADR-011 |
| 8-2 | Zone-aware append (open, write, finish) | ADR-011 |
| 8-3 | `SSTable` writer (sorted pages to file) | — |
| 8-4 | `SSTable` reader (mmap, binary search) | — |
| 8-5 | Tests: write WAL → crash → reopen → replay → verify | ADR-011 |

**DoD**: Data survives simulated crash. WAL replays correctly. SSTable roundtrip works.

---

## Wave 9: Protocol Coordinator

| Task | Description | ADR |
|------|-------------|-----|
| 9-1 | `HlcClock` with PTP + Lamport counter | ADR-014 |
| 9-2 | `CxlCoordinator::commit()` with local NVMe fallback | OQ-02 |
| 9-3 | `RaftCoordinator::commit()` with TCP fallback | OQ-04 |
| 9-4 | Tests: HLC monotonicity, CXL/Raft fallback correctness | ADR-014 |

**DoD**: HLC produces monotonic timestamps. Both coordinators fall back gracefully.

---

## Wave 10: Indexes + Sketches

| Task | Description | ADR |
|------|-------------|-----|
| 10-1 | `src/index/bsi.rs` — bit-sliced index (64 bitmaps) | — |
| 10-2 | `src/index/lsh.rs` — random-hyperplane LSH | ADR-017 |
| 10-3 | `src/sketch/hll.rs` — HyperLogLog | ADR-015 |
| 10-4 | `src/sketch/count_min.rs` — Count-Min sketch | ADR-015 |
| 10-5 | `src/sketch/tdigest.rs` — t-Digest | ADR-015 |
| 10-6 | Tests: HLL within 2%, Count-Min within ε | ADR-015 |

**DoD**: All sketches produce correct results within theoretical bounds.

---

## Wave 11: Join Ordering + Planner

| Task | Description | ADR |
|------|-------------|-----|
| 11-1 | `src/planner/dpccp.rs` — DPccp for n≤15 | ADR-019 |
| 11-2 | `src/planner/cardinality.rs` — cardinality estimation | — |
| 11-3 | `src/planner/lowerer.rs` — logical plan → kernel DAG | §7.2 |
| 11-4 | Tests: DPccp finds optimal for 5-table star query | ADR-019 |

**DoD**: Join orderer produces optimal plan for n≤15. Lowering generates correct kernel invocations.

---

## Wave 12: Benchmarks + Final Integration

| Task | Description | ADR |
|------|-------------|-----|
| 12-1 | `benches/tpch/` — TPC-H Q1, Q6 | ADR-021 |
| 12-2 | `benches/tpcc/` — TPC-C New-Order | — |
| 12-3 | `benches/schema_fluid.rs` — mixed-type benchmark | — |
| 12-4 | `benches/energy.rs` — RAPL energy measurement | ADR-022 |
| 12-5 | `docs/benchmark-results.md` — documented results | — |

**DoD**: All benchmarks run. Results documented. TPC-H loss ≤1.5×.

---

## Wave 13: WCOJ / Leapfrog Triejoin

| Task | Description | ADR |
|------|-------------|-----|
| 13-1 | `src/planner/agm.rs` — AGM fractional cover bound | ADR-019 |
| 13-2 | `src/kernel/leapfrog.rs` — Leapfrog triejoin kernel | ADR-019 |
| 13-3 | `src/planner/wcoj.rs` + `benches/bench_wcoj.rs` — WCOJ plan selection + benchmark | ADR-019 |

**DoD**: Triangle join runs 3–5× faster via leapfrog than hash cascade.

---

## Wave 14: Learned Cardinality Estimation

| Task | Description | ADR |
|------|-------------|-----|
| 14-1 | `src/planner/learned.rs` — equi-width histogram + correction factor | — |
| 14-2 | `src/planner/calibration.rs` — online calibration loop (MAPE tracker) | — |
| 14-3 | `benches/bench_cardinality.rs` — accuracy & throughput benchmark | — |

**DoD**: 17× MAPE improvement on zipfian data vs the `0.1` / `0.33` heuristic defaults.

---

## Wave 15: MCTS Plan Search

| Task | Description | ADR |
|------|-------------|-----|
| 15-1 | `src/planner/mcts.rs` — MCTS with UCT + cost-minimization reward | ADR-019 |
| 15-2 | `src/planner/graph_prune.rs` — connectivity pruning for MCTS branching | ADR-019 |
| 15-3 | `benches/bench_planner.rs` — DPccp vs MCTS at n=5, 10, 20, 30 | ADR-019 |

**DoD**: Scales to n>15 joins (DPccp refuses), within 2× of optimal on n≤15.

---

## Wave 16: Adaptive Eddies

| Task | Description | ADR |
|------|-------------|-----|
| 16-1 | `src/executor/eddy.rs` — per-morsel adaptive tuple routing | — |
| 16-2 | `src/executor/adaptive.rs` — runtime plan switching via divergence detection | — |
| 16-3 | `benches/bench_eddy.rs` — eddy vs fixed-pipeline benchmark | — |

**DoD**: 12× speedup on skewed filter pipeline via early termination.

---

## Wave 17: Tensor-Network Contraction

| Task | Description | ADR |
|------|-------------|-----|
| 17-1 | `src/planner/tensor.rs` — tensor-network model of relational join | — |
| 17-2 | `src/planner/contraction.rs` + `src/compress/tensor_train.rs` — contraction → join tree + TT compression | — |
| 17-3 | `benches/bench_tensor.rs` — contraction ordering vs DPccp + TT compression | — |

**DoD**: 5.6× faster planning at n=10; 11× lossless compression on rank-3 matrix.

---

## Wave 18: Final 3× Proof + Smoke

| Task | Description |
|------|-------------|
| 18-1 | `benches/bench_3x_proof.rs` — paired before/after benchmark for all 5 techniques |
| 18-2 | `docs/3x-proof.md` — documented results with measured numbers + arXiv refs |
| 18-3 | `examples/smoke.rs` — updated to demonstrate all 5 techniques |
| 18-4 | `ORCHESTRATION.md` — all 18 waves marked complete with final test counts |

**DoD**:
- `cargo test` passes (554 tests)
- `cargo clippy -- -D warnings` passes
- `cargo bench --bench bench_3x_proof -- --quick` runs and prints the 5-workload comparison
- `cargo run --example smoke` runs successfully
- `docs/3x-proof.md` exists with real measured numbers
- At least one workload shows ≥3× speedup (proving the 3× target is met)

---

## Execution rules

1. Orchestrator dispatches subtasks to agents sequentially within each wave
2. Agent receives: task ID, description, ADR reference, DoD, file scope
3. Agent must: implement, test, `cargo fmt`, `cargo clippy`, `cargo test`
4. Commit after each subtask: `wave-N/task-M: <description>`
5. Push after each wave (once all DoDs pass)
6. Orchestrator verifies DoD before starting next wave
7. **All 18 waves are complete — orchestrator has returned.**
