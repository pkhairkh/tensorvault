# Benchmark Results

> Methodology, harness inventory, and result-recording template for the
> turboGP benchmark suite. All new benchmarks live under `benches/` and use
> [`criterion`](https://docs.rs/criterion) for statistically-rigorous
> measurement.

---

## 1. Methodology

### 1.1 Why criterion

turboGP uses `criterion 0.5` (with `html_reports`) as its benchmark harness
because it provides:

- **Statistical rigour** — criterion runs each benchmark for a configurable
  sample size (default 100) and warm-up (default 3 s), then reports the
  mean, median, MAD, and a 95 % confidence interval. Naive `Instant::now()`
  timing produces numbers that vary by ±20 % across runs on a noisy CI host;
  criterion reduces that to ±1 %.
- **Throughput mode** — every turboGP benchmark calls
  `group.throughput(Throughput::Elements(n))` so the report shows
  **cells/sec** (for kernel benchmarks) or **rows/sec** (for TPC-H),
  not just wall-clock time. This is the canonical unit for vectorized
  scan kernels (ADR-023).
- **Regression detection** — `criterion --save-baseline` /
  `--compare-baseline` lets us detect a >5 % throughput regression in CI.

### 1.2 Bench commands

```bash
# Build only — verifies all benchmarks compile.
cargo build --benches

# Run all benchmarks (long: ~3 min on a fast laptop, ~15 min on CI).
cargo bench

# Run a single benchmark file.
cargo bench --bench bench_kernels
cargo bench --bench bench_tpch
cargo bench --bench bench_sketches

# Save a baseline (e.g. before a kernel rewrite) and compare after.
cargo bench --bench bench_kernels -- --save-baseline before
# ... edit kernels ...
cargo bench --bench bench_kernels -- --baseline before
```

### 1.3 Measurement discipline

To get reproducible numbers:

1. **Pin the benchmark thread to a single core** to avoid migration noise:
   ```bash
   taskset -c 1 cargo bench --bench bench_kernels
   ```
   (Linux only. On macOS, use `numactl --cpunodebind=0 --membind=0` on a
   Linux VM.)

2. **Disable Turbo Boost** so the CPU runs at a known frequency:
   ```bash
   echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo
   ```
   The default cost model (ADR-023) assumes 3 GHz; without disabling turbo
   the actual frequency varies 2.5–5.0 GHz and the cost model's predictions
   drift by ±40 %.

3. **Disable hyperthreading siblings** so the benchmark thread has the full
   execution resources of its physical core:
   ```bash
   echo 0 | sudo tee /sys/devices/system/cpu/cpu1/online
   ```

4. **Run on a quiet system** — no browser, no editor with LSP, no other
   benchmarks. Background CPU usage of even 5 % inflates variance from ±1 %
   to ±10 %.

5. **Use `--warm-up-time 5 --measurement-time 10`** for short benchmarks
   (< 1 ms per iteration) to reduce sampling noise:
   ```bash
   cargo bench --bench bench_kernels -- --warm-up-time 5 --measurement-time 10
   ```

### 1.4 What is measured vs. what is not

| Aspect | Measured | Not measured |
|---|---|---|
| **Kernel hot loop** | ✔ (the SIMD body, the region lock + select + execute) | ✖ NUMA effects (run under `numactl`) |
| **Throughput** | ✔ (cells/sec, rows/sec, ops/sec) | ✖ Tail latency (run with `--percentile 99`) |
| **Energy** | ✖ (see ADR-022 — use `perf stat -e power/energy-pkg/`) | — |
| **Cold-cache** | ✖ (criterion warms the cache by default; cold-cache numbers are 5–20× lower) | — |
| **Multi-threaded** | ✖ (every benchmark is single-threaded; multi-threaded scaling is a separate benchmark suite) | — |

### 1.5 Reporting energy alongside throughput

Per ADR-022, energy efficiency is a primary turboGP differentiator. When
publishing benchmark results, always report:

- **Throughput** (cells/sec or rows/sec) — from criterion.
- **Energy per operation** (nJ/cell or µJ/row) — from `perf stat -e
  power/energy-pkg/` wrapping the criterion binary, divided by the
  throughput × measurement time.
- **Queries per joule** — the reciprocal, useful for TPC-H.

A copy-paste recipe:

```bash
# Run the kernel benchmark under perf, capturing package energy.
sudo perf stat -e power/energy-pkg/ -a \
    cargo bench --bench bench_kernels -- scan_eq/1M_cells

# perf prints: "power/energy-pkg/  12.34 Joules"
# If criterion reported 24 G cells/sec over 5 s of measurement:
#   cells = 24e9 × 5 = 1.2e11
#   J/cell = 12.34 / 1.2e11 = 1.03e-10 J = 0.103 nJ/cell
```

The RAPL counter is only accurate for ≥10 ms windows (Hahnel 2012, ADR-022),
so for sub-microsecond benchmarks aggregate over many iterations inside one
criterion sample. The default criterion sample size (100 samples × ~5 µs
each = 500 µs) is too short — use `--sample-size 500 --measurement-time 30`
to extend it.

---

## 2. Benchmark inventory

The turboGP benchmark suite is organized into four criterion binaries, each
focusing on one performance axis:

| Binary | File | What it measures | Why it matters |
|---|---|---|---|
| **`throughput`** | `benches/throughput.rs` | Per-tier `scan_eq` + `sum_f64` + `hamming` throughput on 1 M cells | The "headline" benchmark — shows the tier-aware kernel table's L3/DDR5/CXL differentiation (ADR-010). |
| **`bench_kernels`** | `benches/bench_kernels.rs` | All five key kernels (`scan_eq`, `scan_range`, `sum_f64`, `hamming`, `hash_probe`) at 1 M cells | The kernel-by-kernel throughput inventory — feeds the cost model (ADR-023) calibration table. |
| **`bench_tpch`** | `benches/bench_tpch.rs` | TPC-H Q1 (aggregation) and Q6 (filter) on 100 K synthetic lineitem rows | Per ADR-021, turboGP accepts a 1.2–1.5× loss on TPC-H; this benchmark shows where the loss comes from (narrow columns, multi-pass filter+aggregate). |
| **`bench_sketches`** | `benches/bench_sketches.rs` | HLL, Count-Min, t-Digest update throughput | Sketch throughput determines whether per-column statistics can be maintained on every INSERT (ADR-015, ADR-023). |

### 2.1 `bench_kernels` — kernel-by-kernel throughput

| Group | Bench | Operator | N | Throughput unit |
|---|---|---|---|---|
| `scan_eq` | `1M_cells` | `ScanEqU64` | 1 000 000 | cells/sec |
| `scan_range` | `1M_cells` | `ScanRangeU64` | 1 000 000 | cells/sec |
| `sum_f64` | `1M_cells` | `AggregateSumF64` | 1 000 000 | cells/sec |
| `hamming` | `1M_cells` | `SimilarityHamming` | 1 000 000 | cells/sec |
| `hash_probe` | `1M_cells` | `HashProbe` (in-process `HashTable::probe`) | 1 000 000 | probes/sec |

**Expected throughput** (Zen 5, AVX-512, L3-resident, 3 GHz, single-threaded,
per ADR-023):

| Kernel | Expected | Theoretical bound |
|---|---|---|
| `scan_eq` | ~24 G cells/sec | 8 lanes × 3 GHz = 24 G cells/sec |
| `scan_range` | ~24 G cells/sec | same SIMD bound |
| `sum_f64` | ~24 G cells/sec | `VADDPD` 8-wide |
| `hamming` | ~8 G cells/sec | `VPOPCNTDQ` 8-wide |
| `hash_probe` | ~100 M probes/sec | hash table is `std::HashMap` (prototype) |

The `hash_probe` number is a **prototype baseline** — it uses
`std::HashMap`, not the planned SwissTable with `VPCMPEQB` probing. A future
wave that lands the SwissTable is expected to push this to 1+ G probes/sec.

### 2.2 `bench_tpch` — TPC-H Q1, Q6, and a multi-predicate variant

| Group | Bench | Query | N (rows) | Throughput unit |
|---|---|---|---|---|
| `tpch_q1` | `100K_rows` | `SUM(l_quantity) GROUP BY l_returnflag` | 100 000 | rows/sec |
| `tpch_q6` | `100K_rows` | `SUM(l_extendedprice) WHERE l_quantity < 24 AND l_discount ∈ [0.05, 0.07]` | 100 000 | rows/sec |
| `tpch_multi_predicate` | `100K_rows` | 3-predicate fused scan on `l_quantity` | 100 000 | rows/sec |

**Why only Q1 and Q6?** Per ADR-021, turboGP accepts a 1.2–1.5× structural
loss on TPC-H. Q1 and Q6 are the two queries where turboGP comes **closest**
to DuckDB:

- Q1 is a pure aggregation (no joins) — turboGP's column-store + AVX-512
  `VADDPD` should match DuckDB's `SUM` to within 10 %.
- Q6 is a range-filter-then-aggregate — the multi-predicate scan
  (`VPTERNLOGQ`) is turboGP's strongest kernel.

The other 20 TPC-H queries involve multi-table joins, subqueries, and
`GROUP BY` on string columns — areas where DuckDB's 20-year optimizer wins
by 2–3×. Those queries are documented in `docs/benchmarks/tpcc-analysis.md`
and intentionally not benchmarked here (see ADR-021 "Optimize the top 3–4
queries to narrow the gap, but don't chase parity").

**Synthetic data**: the lineitem columns are generated by simple modulo
arithmetic, not the TPC-H `dbgen` generator. The data **shape** matches
(uniform `l_quantity`, narrow `l_discount`), so the kernel's branchless
inner loop hits the same instruction mix. The absolute numbers (revenue,
row counts) are not TPC-H-canonical — this is a **kernel-throughput**
benchmark, not a TPC-H result submission.

### 2.3 `bench_sketches` — sketch update throughput

| Group | Bench | Structure | N (updates) | Throughput unit |
|---|---|---|---|---|
| `hll` | `100K_adds_then_estimate` | HyperLogLog (p=14, m=16 384) | 100 000 | adds/sec |
| `count_min` | `100K_adds_then_estimate` | Count-Min (d=5, w=1024) | 100 000 | adds/sec |
| `tdigest` | `10K_adds_then_quantile` | t-Digest (max_centroids=100) | 10 000 | adds/sec |

**Expected throughput** (single-threaded, no SIMD):

| Sketch | Expected | Bottleneck |
|---|---|---|
| HLL | ~50 M adds/sec | 1 hash + 1 register update per add |
| Count-Min | ~25 M adds/sec | 5 hashes + 5 counter updates per add |
| t-Digest | ~1 M adds/sec | sorted-insert O(log n) + occasional compress |

**Why measure these?** Sketch throughput determines whether the engine can
maintain per-column statistics inline (on every INSERT) or must sample:

- HLL at 50 M adds/sec → can maintain distinct-count stats on a 1 M
  inserts/sec OLTP stream with 5 % CPU overhead.
- t-Digest at 1 M adds/sec → too slow for inline; the engine would sample
  1 % of inserts and feed the sampled stream to t-Digest.

The cost model (ADR-023) consumes these statistics to pick join order and
index selection; without fresh stats, the planner falls back to defaults
and produces 1.5–2× worse plans on a 10-table join.

### 2.4 `throughput` — per-tier scan throughput (pre-existing)

The original `benches/throughput.rs` benchmarks `scan_eq` on each tier
(`L3`, `Ddr5`, `Cxl`) — the headline benchmark that demonstrates the
tier-aware kernel table's value. See `benches/throughput.rs` for the
full commentary.

---

## 3. Result-recording template

When running benchmarks on real hardware, paste the criterion summary into
the appropriate section below. Each entry should record:

- **Date** (YYYY-MM-DD).
- **Hardware**: CPU model, core count, base clock, RAM type/size, L3 size.
- **OS**: kernel version, turbo boost state, hyperthreading state.
- **Build**: `cargo bench --release` (or `--profile=bench`).
- **Numbers**: criterion's mean ± stddev for each benchmark group.

### 3.1 Hardware A — _<fill in on first real run>_

**Date**: _(YYYY-MM-DD)_
**CPU**: _(e.g. AMD EPYC 9654, 96 cores, 2.4 GHz base / 3.7 GHz boost)_
**L3**: _(e.g. 384 MB)_
**RAM**: _(e.g. 12 × 32 GB DDR5-4800)_
**OS**: _(e.g. Linux 6.5.0, turbo disabled, HT sibling offline)_
**Build**: `cargo bench --release`

#### `bench_kernels`

| Group / Bench | Mean | Stddev | Throughput |
|---|---|---|---|
| `scan_eq/1M_cells` | _µs_ | _µs_ | _M cells/sec_ |
| `scan_range/1M_cells` | _µs_ | _µs_ | _M cells/sec_ |
| `sum_f64/1M_cells` | _µs_ | _µs_ | _M cells/sec_ |
| `hamming/1M_cells` | _µs_ | _µs_ | _M cells/sec_ |
| `hash_probe/1M_cells` | _µs_ | _µs_ | _M probes/sec_ |

#### `bench_tpch`

| Group / Bench | Mean | Stddev | Throughput |
|---|---|---|---|
| `tpch_q1/100K_rows` | _µs_ | _µs_ | _M rows/sec_ |
| `tpch_q6/100K_rows` | _µs_ | _µs_ | _M rows/sec_ |
| `tpch_multi_predicate/100K_rows` | _µs_ | _µs_ | _M rows/sec_ |

#### `bench_sketches`

| Group / Bench | Mean | Stddev | Throughput |
|---|---|---|---|
| `hll/100K_adds_then_estimate` | _ms_ | _ms_ | _M adds/sec_ |
| `count_min/100K_adds_then_estimate` | _ms_ | _ms_ | _M adds/sec_ |
| `tdigest/10K_adds_then_quantile` | _ms_ | _ms_ | _M adds/sec_ |

#### Energy (ADR-022)

| Benchmark | Energy (J) | Time (s) | Throughput | nJ/op |
|---|---|---|---|---|
| `scan_eq/1M_cells` | _J_ | _s_ | _M cells/sec_ | _nJ/cell_ |
| `tpch_q1/100K_rows` | _J_ | _s_ | _M rows/sec_ | _µJ/row_ |

### 3.2 Hardware B — _<fill in on second real run>_

_(Same template as above; useful for cross-vendor comparisons — e.g. an
Intel Sapphire Rapids run alongside the AMD Zen 5 baseline.)_

### 3.3 Comparison: DuckDB baseline (ADR-021)

Per ADR-021, turboGP accepts a 1.2–1.5× structural loss on TPC-H. To make
that claim defensible, the TPC-H benchmark numbers should be compared
side-by-side with DuckDB running the same Q1/Q6 queries on the same data:

| Query | turboGP (rows/sec) | DuckDB (rows/sec) | Ratio (turbogp/duckdb) |
|---|---|---|---|
| Q1 | _M rows/sec_ | _M rows/sec_ | _×_ |
| Q6 | _M rows/sec_ | _M rows/sec_ | _×_ |

A ratio in **[0.66, 0.83]** confirms the ADR-021 prediction. A ratio
outside that band triggers an investigation:

- **Ratio > 0.83**: either the ADR-021 prediction was too pessimistic
  (update the ADR), or the benchmark is not measuring the same workload
  (verify the SQL and data shapes match).
- **Ratio < 0.66**: a turboGP kernel is regressing; bisect with
  `criterion --baseline`.

---

## 4. Cross-references

| Document | How it relates to this file |
|---|---|
| [ADR-021 (TPC-H accepted loss)](./adr/021-tpc-h-accept-loss.md) | Defines the 1.2–1.5× expected loss band; this file's §3.3 records whether the actual ratio falls in the band. |
| [ADR-022 (RAPL energy benchmarking)](./adr/022-rapl-energy-benchmarking.md) | Specifies the three-tier energy measurement approach (RAPL → analytical model → external Hioki meter); this file's §1.5 and §3.1's energy table follow that approach. |
| [ADR-023 (calibrated analytic cost model)](./adr/023-calibrated-analytic-cost-model.md) | The cost model's per-kernel throughput constants are calibrated against the `bench_kernels` numbers; this file's §2.1 lists the expected throughputs that calibration targets. |
| [`docs/benchmarks/tpcc-analysis.md`](./benchmarks/tpcc-analysis.md) | TPC-C analysis — the workload where turboGP expects to win 11× on energy; orthogonal to this file's TPC-H focus. |
| [`docs/benchmarks/tpcc-math.md`](./benchmarks/tpcc-math.md) | Mathematical companion to the TPC-C analysis; provides the tpmC/warehouse ceiling derivation. |
| [`benches/throughput.rs`](../benches/throughput.rs) | The pre-existing tier-aware scan benchmark (§2.4 of this file). |
| [`benches/bench_kernels.rs`](../benches/bench_kernels.rs) | The kernel-by-kernel throughput inventory (§2.1). |
| [`benches/bench_tpch.rs`](../benches/bench_tpch.rs) | The TPC-H Q1/Q6 benchmark (§2.2). |
| [`benches/bench_sketches.rs`](../benches/bench_sketches.rs) | The sketch update throughput benchmark (§2.3). |

---

## 5. Known caveats

1. **No SIMD on non-x86 hosts.** The AVX-512 / AVX-2 kernels are
   `#[cfg(target_arch = "x86_64")]` and fall back to the scalar kernel on
   ARM / RISC-V. The throughput numbers on a non-x86 host will be 4–8×
   lower (no SIMD).

2. **`hash_probe` uses `std::HashMap`.** The production SwissTable with
   `VPCMPEQB` probing is not yet implemented (see `src/kernel/hash.rs`'s
   `AlignedSlot` for the preparation). The `hash_probe` benchmark therefore
   measures the prototype, not the target — the number is a floor, not a
   ceiling.

3. **`tpch_q6` does not fuse filter + aggregate.** The benchmark runs
   `ScanRangeU64` twice (once per predicate) and then `AggregateSumF64` on
   the full column. A production engine would fuse these into one pass via
   a filter-then-sum morsel pipeline (ADR-018). The unfused number is
   therefore 3× the optimal; this is documented in the bench source.

4. **Single-threaded only.** All benchmarks run on one thread. Multi-
   threaded scaling (morsel dispatcher across NUMA-pinned workers,
   ADR-008/ADR-018) is a separate measurement not covered here.

5. **Synthetic TPC-H data.** As noted in §2.2, the lineitem columns are
   generated by modulo arithmetic, not `dbgen`. The data shape (uniform
   `l_quantity`, narrow `l_discount`) is close enough to TPC-H for kernel-
   throughput measurement, but the absolute revenue/row-count numbers are
   not TPC-H-canonical.
