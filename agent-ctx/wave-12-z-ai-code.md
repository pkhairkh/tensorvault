# Wave 12 — Benchmarks + Final Integration

**Agent**: z-ai-code
**Date**: 2026-07-30
**Status**: Complete
**Baseline**: 348 tests (340 lib + 7 integration + 1 doc-test)
**After Wave 12**: 348 tests (no new tests added — this wave is benchmarks
+ docs + smoke-example only).

## Tasks Completed

### 12-1: `benches/bench_kernels.rs` — kernel throughput benchmarks

Created a criterion benchmark file that measures throughput for all five
key kernels at 1 M cells:

| Group        | Bench       | Operator              | Result (Zen 5 dev host, --quick) |
|--------------|-------------|-----------------------|----------------------------------|
| `scan_eq`    | `1M_cells`  | `ScanEqU64`           | 2.83 G cells/sec                 |
| `scan_range` | `1M_cells`  | `ScanRangeU64`        | 1.53 G cells/sec                 |
| `sum_f64`    | `1M_cells`  | `AggregateSumF64`     | 2.10 G cells/sec                 |
| `hamming`    | `1M_cells`  | `SimilarityHamming`   | 13.2 G cells/sec                 |
| `hash_probe` | `1M_cells`  | `HashProbe` (HashMap) | 17.0 M probes/sec (prototype)    |

All benchmarks use `Throughput::Elements(1_000_000)` so the report shows
cells/sec, the canonical unit for vectorized scan kernels (ADR-023).

**Implementation note**: `bench_scan_range` builds a `KernelInvocation`
once and re-executes it via `Scheduler::execute_invocation` each
iteration. This mirrors how a real query runs: the plan lowerer produces
a static invocation list, and the scheduler dispatches them.

### 12-2: `benches/bench_tpch.rs` — TPC-H Q1 and Q6

Created three TPC-H-shaped benchmarks on 100 K synthetic lineitem rows:

| Group                   | Bench         | Query                                                            | Result (dev host, --quick) |
|-------------------------|---------------|------------------------------------------------------------------|----------------------------|
| `tpch_q1`               | `100K_rows`   | `SUM(l_quantity) GROUP BY l_returnflag` (3 partitions)           | 63 M rows/sec              |
| `tpch_q6`               | `100K_rows`   | `SUM(l_extendedprice) WHERE l_quantity < 24 AND l_discount ∈ [5,7]` | 98 M rows/sec              |
| `tpch_multi_predicate`  | `100K_rows`   | 3-predicate fused scan on `l_quantity` (`VPTERNLOGQ`, ADR-004)   | 3.34 G rows/sec            |

Per ADR-021, turboGP accepts a 1.2–1.5× structural loss on TPC-H. The Q1
and Q6 queries are the two where turboGP comes closest to DuckDB (pure
aggregation + range filter). The synthetic data uses simple modulo
arithmetic — data **shape** matches TPC-H (uniform `l_quantity`, narrow
`l_discount`), but absolute numbers are not TPC-H-canonical. Documented
in the bench source and in `docs/benchmark-results.md` §2.2.

The third benchmark (`tpch_multi_predicate`) exercises the fused
`VPTERNLOGQ` scan kernel — the kernel that benefits most from turboGP's
instruction-first design. A generic vectorized executor would issue three
separate comparisons; turboGP fuses them into one 3-input bit op.

### 12-3: `benches/bench_sketches.rs` — sketch performance

Created three sketch-update benchmarks:

| Group        | Bench                          | Structure         | N       | Result (dev host, --quick) |
|--------------|--------------------------------|-------------------|---------|----------------------------|
| `hll`        | `100K_adds_then_estimate`      | HyperLogLog p=14  | 100 000 | 312 M adds/sec             |
| `count_min`  | `100K_adds_then_estimate`      | CountMin d=5,w=1024 | 100 000 | 17.7 M adds/sec          |
| `tdigest`    | `10K_adds_then_quantile`       | TDigest max=100   | 10 000  | 5.7 M adds/sec             |

Smaller N for t-Digest because its `add` is O(log n) (sorted insert +
occasional compress); kept at 10 K so total wall time stays reasonable.

These numbers determine whether the engine can maintain per-column
statistics inline (on every INSERT) or must sample. HLL at 312 M adds/sec
→ inline is cheap. t-Digest at 5.7 M adds/sec → would need 1% sampling
on a 1 M inserts/sec OLTP stream.

### 12-4: `docs/benchmark-results.md` — methodology + template

Created a 5-section doc:

1. **Methodology** — why criterion, bench commands, measurement discipline
   (CPU pinning, turbo boost, hyperthreading, warm-up time), what is
   measured vs. not, energy-reporting recipe per ADR-022.
2. **Benchmark inventory** — table of all four bench binaries with what
   each measures and why it matters. Per-bench expected-throughput table
   (Zen 5 / AVX-512 / L3-resident, from ADR-023).
3. **Result-recording template** — copy-paste tables for Hardware A and
   Hardware B, plus a DuckDB comparison table for the ADR-021 1.2–1.5×
   loss claim verification.
4. **Cross-references** — links to ADR-021 (TPC-H loss), ADR-022 (RAPL
   energy), ADR-023 (cost model calibration), and the existing
   `tpcc-analysis.md` / `tpcc-math.md` documents.
5. **Known caveats** — single-threaded only, no SIMD on non-x86,
   `hash_probe` uses `std::HashMap` (prototype, not SwissTable),
   `tpch_q6` does not fuse filter+aggregate (3× optimal, documented),
   synthetic TPC-H data.

### 12-5: `examples/smoke.rs` — full end-to-end demo

Rewrote the smoke example to walk through the full pipeline in 8
sections:

1. **Hardware detection** — CPUID, NUMA topology.
2. **Protocol coordinators (HLC timestamps)** — CXL single-rack + Raft
   cross-rack, both issuing HLC timestamps. Standalone `HlcClock`
   demonstration showing three strictly-monotonic timestamps.
3. **SQL → plan → lower → execute** — parses
   `SELECT COUNT(*) FROM users WHERE id = 42`, builds a plan via
   `build_plan`, lowers it via `PlanLowerer::lower`, registers a region
   at the derived region_id, patches the scan invocation's `cell_count`
   from the registered region's size, executes via
   `Scheduler::execute_invocation`, prints the final COUNT(*) result.
   Verified: returns 1 (the single cell equal to 42).
4. **HLL count-distinct** — 10 000 distinct hashes, each duplicated 5×.
   Uses `xxh3::xxh3_64` (the engine's standard hash) for proper hash
   dispersion. Verified: estimate = 10035 vs true 10000 = 0.35 % error
   (within the 0.81 % RSE for p=14).
5. **LSH similarity search** — 100 vectors in 2 clusters (50 near
   [1,0,0,0], 50 near [0,1,0,0]). Query for [1,0.005,0,0] returns 50
   candidates, all from cluster 1.
6. **Kernel throughput** — scan_eq / sum_f64 / count_similar on a 2 MB
   region. Reports M cells/sec for each.
7. **Storage geometry** — page/region/tablet sizes.
8. **Kernel table** — full list of all 18 registered kernels.

## Cargo.toml changes

Added three new `[[bench]]` sections:

```toml
[[bench]]
name = "bench_kernels"
harness = false
path = "benches/bench_kernels.rs"

[[bench]]
name = "bench_tpch"
harness = false
path = "benches/bench_tpch.rs"

[[bench]]
name = "bench_sketches"
harness = false
path = "benches/bench_sketches.rs"
```

`harness = false` is required because criterion provides its own `main`.

## DoD Verification

```
$ cargo fmt                                                # clean (only nightly-feature warnings)
$ cargo clippy -- -D warnings                              # Finished, no warnings
$ cargo clippy --all-targets -- -D warnings                # Finished, no warnings
$ cargo test                                               # 340 lib + 7 integration + 1 doc-test = 348 pass
$ cargo build --benches                                    # Finished in 0.37s (dev profile)
$ cargo bench --no-run                                     # Finished in 1m 58s (bench profile, all 4 binaries)
$ cargo run --example smoke                                # Runs successfully, prints full pipeline
```

All four benchmark binaries produce valid throughput numbers under
`cargo bench --quick`:

- `bench_kernels`: 5 benchmarks, 75 µs – 59 ms per iteration.
- `bench_tpch`: 3 benchmarks, 30 µs – 1.6 ms per iteration.
- `bench_sketches`: 3 benchmarks, 310 µs – 5.7 ms per iteration.

## Files Created / Modified

| File | Status | Lines |
|------|--------|-------|
| `benches/bench_kernels.rs` | created | 175 |
| `benches/bench_tpch.rs` | created | 245 |
| `benches/bench_sketches.rs` | created | 110 |
| `docs/benchmark-results.md` | created | 325 |
| `examples/smoke.rs` | rewritten | 250 |
| `Cargo.toml` | modified | +12 (three `[[bench]]` sections) |

## Notes for Future Waves

- The `hash_probe` benchmark measures the `std::HashMap` prototype at
  17 M probes/sec. The planned SwissTable with `VPCMPEQB` probing
  (see `src/kernel/hash.rs`'s `AlignedSlot`) is expected to push this
  to 1+ G probes/sec — a 60× improvement. The benchmark is in place to
  measure that improvement when the SwissTable lands.

- The `tpch_q6` benchmark does not fuse filter + aggregate (three
  separate kernel invocations: ScanRange × 2 + AggregateSum). A
  production engine would fuse these via a filter-then-sum morsel
  pipeline (ADR-018). The current benchmark number is therefore ~3×
  the optimal; this is documented in the bench source and in
  `docs/benchmark-results.md` §5 caveat #3.

- The `docs/benchmark-results.md` template has placeholder tables for
  Hardware A and Hardware B. When the benchmarks are run on real Zen 5
  / Sapphire Rapids hardware, the results should be pasted into those
  tables — they will then serve as the regression baseline for future
  PRs.

- The smoke example demonstrates the SQL → plan → lower → execute
  pipeline end-to-end. The notable wart (documented in the smoke
  source) is that `PlanLowerer` leaves `cell_count = 0` because it
  doesn't know the region's size at lower time; the smoke patches this
  by reading `region.cell_count()` after registration. A future wave
  could teach the lowerer to query the scheduler's region registry and
  fill in `cell_count` automatically.
