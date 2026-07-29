# Benchmarking Problems

> Problems related to benchmarking the engine: TPC-H (where we lose), TPC-C
> (where we can win on $/tpmC), and custom benchmarks for the engine's unique
> capabilities (schema-fluid analytics, similarity joins, approximate queries).
>
> **Research source**: `docs/tpcc_analysis.md`, `docs/tpcc_math.md`.

---

## P-08-01: TPC-H benchmark (expected loss) 🟡

**Layer**: Benchmarking
**Status**: 🟡 partial (analysis done in `docs/tpcc_analysis.md`; no implementation)
**Math**: none
**Effort**: L
**Impact**: high (for credibility)

### Problem

TPC-H is the standard OLAP benchmark. We expect to **lose** to DuckDB by
1.2–1.5× because:
- DuckDB's type-stable columns are more compact than 64-bit-everywhere
- DuckDB's executor is 20 years more mature
- Our kernel table doesn't yet cover all TPC-H operators

But we should run it anyway to:
1. Establish a baseline
2. Identify where we're close (and could win with optimization)
3. Provide an honest comparison for papers

### Open questions

- Which TPC-H queries are we closest on? (Likely Q1 and Q6 — simple scans.)
- Can we win on any query? (Maybe Q9 with leapfrog join.)

### Success criteria

- A `benches/tpch/` directory with all 22 TPC-H queries.
- A benchmark report comparing turboGP vs DuckDB at SF=1, SF=10, SF=100.
- Honest documentation of where we lose and why.

---

## P-08-02: TPC-C benchmark (the win path) 🔴

**Layer**: Benchmarking
**Status**: 🔴 open (analysis in `docs/tpcc_math.md`; no implementation)
**Math**: III (queueing theory for OLTP), IV (knapsack for memory placement)
**Effort**: XL
**Impact**: critical

### Problem

TPC-C is the standard OLTP benchmark. Per `docs/tpcc_math.md`, the path to
winning is **consolidation**: one fat 16 TB DRAM box matching PolarDB's
2,340-node cluster.

The theoretical ceiling:
- 12.86 tpmC/warehouse (spec ceiling)
- 16 TB DRAM → 160K warehouses → 2.06 B tpmC
- ~$0.22/tpmC (vs PolarDB's $0.11/tpmC, but on 1 node vs 2,340)

### Open questions

- Can we actually hit 12.86 tpmC/warehouse? (Requires ~1.85 µs/txn — see
  `docs/tpcc_math.md` §2.)
- How do we handle the 10% cross-warehouse transactions? (Calvin-style
  deterministic ordering.)

### Success criteria

- A `benches/tpcc/` directory with the full TPC-C workload.
- A measured tpmC score on a single socket.
- A TPC-C executive summary comparing to PolarDB.

---

## P-08-03: Custom benchmark — schema-fluid analytics 🔴

**Layer**: Benchmarking
**Status**: 🔴 open
**Math**: I (MDL for schema selection)
**Effort**: L
**Impact**: high

### Problem

TPC-H doesn't test the engine's unique capability: schema-fluid analytics
over mixed-type columns. We need a custom benchmark (call it "TPC-Fluid")
that:
1. Uses tables with VARIANT columns (JSON-like, mixed types)
2. Runs queries that aggregate/filter on these columns
3. Measures the overhead of schema-on-read vs fixed schema

### Workload design

```sql
-- Table with mixed-type columns
CREATE TABLE events (
  id BIGINT,
  timestamp BIGINT,
  event_type VARIANT,    -- could be string, int, or null
  payload VARIANT,       -- JSON-like
  user_id VARIANT        -- sometimes int, sometimes string
);

-- Query 1: filter on a mixed-type column
SELECT COUNT(*) FROM events WHERE event_type = 'click';

-- Query 2: aggregate a mixed-type column
SELECT event_type, COUNT(*) FROM events GROUP BY event_type;

-- Query 3: extract and aggregate
SELECT
  JSON_EXTRACT(payload, '$.latency') AS latency,
  AVG(JSON_EXTRACT(payload, '$.latency')) AS avg_latency
FROM events
WHERE event_type = 'request'
GROUP BY JSON_EXTRACT(payload, '$.region');
```

### Open questions

- What's the right data distribution? (Realistic JSON payloads vs synthetic.)
- How do we compare to DuckDB's JSON support?

### Success criteria

- A `benches/tpc_fluid/` directory with 10 queries.
- turboGP at 5–10× DuckDB on schema-fluid queries.

---

## P-08-04: Custom benchmark — similarity joins 🔴

**Layer**: Benchmarking
**Status**: 🔴 open
**Math**: III (LSH, Hamming distance)
**Effort**: M
**Impact**: high

### Problem

No standard benchmark tests similarity joins on arbitrary column types.
We need one to demonstrate the engine's unique capability.

### Workload design

```sql
-- Find pairs of log events with similar payloads
SELECT a.id, b.id, HAMMING_DISTANCE(a.payload, b.payload) AS dist
FROM events a JOIN events b
ON a.payload SIMILAR TO b.payload WITHIN HAMMING DISTANCE 5;

-- Find duplicate images (via perceptual hash)
SELECT a.id, b.id
FROM images a JOIN images b
ON a.phash SIMILAR TO b.phash WITHIN HAMMING DISTANCE 3
WHERE a.id < b.id;

-- Approximate string matching
SELECT a.id, b.id
FROM documents a JOIN documents b
ON a.fingerprint SIMILAR TO b.fingerprint WITHIN HAMMING DISTANCE 10;
```

### Open questions

- What datasets? (Logs, images, documents — all have natural similarity.)
- What's the threshold for "useful" results?

### Success criteria

- A `benches/similarity/` directory with 3 workloads.
- turboGP at 100× DuckDB (which doesn't have native similarity joins).

---

## P-08-05: Custom benchmark — approximate queries 🔴

**Layer**: Benchmarking
**Status**: 🔴 open
**Math**: III (Hoeffding, HLL, Count-Min)
**Effort**: M
**Impact**: medium

### Problem

No standard benchmark tests `(ε, δ)` approximate SQL. We need one to
demonstrate the engine's guarantee propagation.

### Workload design

```sql
-- Query 1: approximate average
SELECT AVG(price) APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99 FROM sales;

-- Query 2: approximate count distinct
SELECT COUNT(DISTINCT user_id) APPROXIMATE WITHIN 0.02 CONFIDENCE 0.95 FROM events;

-- Query 3: approximate quantiles
SELECT PERCENTILE(latency, 0.99) APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99 FROM requests;

-- Query 4: approximate top-k
SELECT user_id, COUNT(*) APPROXIMATE WITHIN 0.05 CONFIDENCE 0.95
FROM events GROUP BY user_id ORDER BY count DESC LIMIT 10;
```

### Validation

For each query:
1. Compute the exact answer
2. Run the approximate query 1000 times
3. Verify: P(|approx - exact| ≤ ε) ≥ 1-δ

### Success criteria

- A `benches/approximate/` directory with 4 queries.
- All queries meet their (ε, δ) guarantee empirically.
- 10× speedup over exact computation.

---

## P-08-06: Energy efficiency benchmark 🔴

**Layer**: Benchmarking
**Status**: 🔴 open
**Math**: I (energy per instruction from `cpu_energy_kb.md`)
**Effort**: M
**Impact**: medium

### Problem

Energy efficiency is a key differentiator (see `docs/tpcc_math.md` — 11×
better than PolarDB). We need a benchmark that measures joules per query.

### Workload

Run TPC-H queries while measuring:
- CPU energy (via RAPL on Intel, estimated on AMD)
- DRAM energy (via RAPL DRAM domain)
- NVMe energy (via SMART power stats)
- Total wall power (via external meter, if available)

Report: joules per query, queries per joule, tpmC per watt.

### Open questions

- How accurate is RAPL for short queries? (< 10 ms — see `cpu_energy_kb.md`
  §2.1, Hahnel et al.)
- Should we use an external meter (Hioki) for ground truth?

### Success criteria

- A `benches/energy/` directory with the energy measurement harness.
- A report comparing turboGP vs DuckDB on joules/query.
- Target: 3–5× lower energy per query on schema-fluid workloads.

---

## P-08-07: Latency tail benchmark (CXL under load) 🔴

**Layer**: Benchmarking
**Status**: 🔴 open
**Math**: III (Kingman's formula)
**Effort**: M
**Impact**: high

### Problem

CXL's variable latency (140–520 ns) means tail latency matters. We need a
benchmark that:
1. Saturates the CXL link
2. Measures p50, p99, p99.9 latency
3. Compares to Kingman's formula predictions (P-02-06)

### Workload

- 1M random reads from a CXL-resident table
- Vary the arrival rate (λ) from 0.1× to 1.0× of service rate (μ)
- Measure the latency distribution at each utilization level

### Success criteria

- A `benches/cxl_latency/` directory with the saturation benchmark.
- Measured p99 within 20% of Kingman's prediction.
- The benchmark reports the utilization at which p99 exceeds 1 µs.

---

## P-08-08: Cross-vendor kernel benchmark matrix 🔴

**Layer**: Benchmarking
**Status**: 🔴 open (same as P-01-07)
**Math**: none
**Effort**: L
**Impact**: high

### Problem

The kernel table claims throughputs (19 G cells/sec, etc.) but these are
unvalidated. We need to measure each kernel on each supported CPU:
- Intel Ice Lake, Sapphire Rapids, Emerald Rapids
- AMD Zen 3, Zen 4, Zen 5
- Apple M2, M3, M4
- AWS Graviton 3, 4
- Ampere One

### Open questions

- Can we use cloud instances for the benchmark matrix?
- How do we handle thermal throttling during sustained benchmarks?

### Success criteria

- A `benches/kernel_matrix/` directory with per-CPU results.
- A CSV with: kernel, CPU, tier, measured throughput, measured energy.
- The kernel table's metadata updated with measured numbers.

---

## Summary

| # | Problem | Status | Math | Effort | Impact |
|---|---------|--------|------|--------|--------|
| 01 | TPC-H benchmark (expected loss) | 🟡 | — | L | high |
| 02 | TPC-C benchmark (the win path) | 🔴 | III+IV | XL | critical |
| 03 | Custom benchmark — schema-fluid | 🔴 | I | L | high |
| 04 | Custom benchmark — similarity joins | 🔴 | III | M | high |
| 05 | Custom benchmark — approximate queries | 🔴 | III | M | medium |
| 06 | Energy efficiency benchmark | 🔴 | I | M | medium |
| 07 | Latency tail benchmark (CXL under load) | 🔴 | III | M | high |
| 08 | Cross-vendor kernel benchmark matrix | 🔴 | — | L | high |
