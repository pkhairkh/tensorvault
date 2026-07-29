# Query Syntax Approach

> The SQL surface we're building toward. Every extension is grounded in a
> mathematical guarantee (from the 5 pillars) and maps to a specific kernel
> in the kernel table.
>
> **Design principle**: the syntax exposes the engine's unique capabilities
> (tier-awareness, approximate execution, similarity search, protocol
> boundaries) without forcing the user to understand the implementation.
> Each extension is opt-in; standard SQL works unchanged.

---

## Design philosophy

1. **Standard SQL is the baseline.** Every query that runs on Postgres/DuckDB
   runs on turboGP with no changes. The extensions are opt-in.

2. **Every extension has a mathematical guarantee.** `APPROXIMATE WITHIN ε
   CONFIDENCE 1-δ` is backed by Hoeffding's inequality. `TIER L3` is backed
   by the memory hierarchy model. `SIMILAR TO` is backed by LSH theory.

3. **The syntax names the capability, not the implementation.** Users say
   `APPROXIMATE`, not "use a Count-Min sketch." The planner picks the
   technique whose theorem matches the requested guarantee.

4. **Extensions compose.** `APPROXIMATE`, `TIER`, `SIMILAR TO`, `CONSISTENCY`
   can all appear in the same query.

---

## The 9 syntax extensions

### Q-01: Approximate queries with (ε, δ) guarantees 🟡

**Status**: 🟡 partial (math in `docs/research/probability_sketching_for_db.md`; parser not implemented)
**Math**: III (Hoeffding, McDiarmid, PAC)
**Effort**: L
**Impact**: critical

#### Syntax

```sql
-- Approximate aggregate with error bound
SELECT AVG(price) APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99 FROM sales;

-- Approximate count distinct
SELECT COUNT(DISTINCT user_id) APPROXIMATE WITHIN 0.02 CONFIDENCE 0.95 FROM events;

-- Approximate with sample size hint
SELECT SUM(amount) APPROXIMATE WITHIN 0.05 CONFIDENCE 0.99
SAMPLE SIZE 10000 FROM transactions;
```

#### Semantics

- `WITHIN ε`: the returned value is within ε of the true value
- `CONFIDENCE 1-δ`: with probability at least 1-δ
- `SAMPLE SIZE n`: hint to the planner (otherwise computed from ε, δ)

The guarantee is: P(|returned - true| ≤ ε) ≥ 1-δ

#### Math

For an average of n i.i.d. samples in [a, b], Hoeffding gives:

$$
n \ge \frac{(b-a)^2}{2\varepsilon^2} \ln \frac{1}{\delta}
$$

For COUNT DISTINCT, HyperLogLog gives RSE = 1.04/√m, so:

$$
m \ge \frac{1.04^2}{\varepsilon^2}
$$

#### Planner behavior

1. Parse the (ε, δ) from the query
2. For each aggregate, pick the minimal-cost sketch whose theorem guarantees (ε, δ)
3. Propagate (ε, δ) through the DAG (see P-05-04)
4. If no sketch can meet the guarantee, error or fall back to exact

#### Open problems

- How do we handle correlated subqueries? (P-05-04)
- How do we verify the guarantee empirically? (P-05-11)

---

### Q-02: Tier hints 🔴

**Status**: 🔴 open
**Math**: none (engineering)
**Effort**: M
**Impact**: high

#### Syntax

```sql
-- Force a scan to use the L3 tier (hot data)
SELECT * FROM hot_table TIER L3 WHERE user_id = 42;

-- Allow the planner to use CXL for cold data
SELECT * FROM cold_table TIER CXL WHERE date < '2024-01-01';

-- Default: planner picks the tier
SELECT * FROM auto_table WHERE user_id = 42;
```

#### Semantics

- `TIER L3`: the scan must use L3-resident data; error if the data isn't there
- `TIER CXL`: prefer CXL (acceptable for cold data)
- `TIER DDR5`: prefer local DRAM
- `TIER NVME`: explicitly read from NVMe (cold storage scan)
- No hint: planner picks based on cost model (P-05-03)

#### Planner behavior

1. If a tier hint is given, validate the data is in that tier (or can be migrated)
2. Select the kernel for `(operator, tier)` from the kernel table
3. If the data isn't in the requested tier, either migrate it or error

#### Open problems

- Should `TIER` be a hard constraint or a soft hint?
- How do we handle joins where one side is L3 and the other is CXL?

---

### Q-03: Similarity search and joins 🔴

**Status**: 🔴 open (kernel exists: `SimilarityHamming`; syntax not implemented)
**Math**: III (LSH — Andoni-Indyk)
**Effort**: L
**Impact**: high

#### Syntax

```sql
-- Find rows similar to a target
SELECT * FROM products
WHERE payload SIMILAR TO x'0123456789ABCDEF' WITHIN HAMMING DISTANCE 5;

-- Similarity join: find pairs of rows within Hamming distance 5
SELECT a.id, b.id, HAMMING_DISTANCE(a.payload, b.payload) AS dist
FROM events a JOIN events b
ON a.payload SIMILAR TO b.payload WITHIN HAMMING DISTANCE 5;

-- Cosine similarity (for vector columns)
SELECT * FROM embeddings
WHERE vec COSINE SIMILAR TO '[0.1, 0.2, 0.3, ...]' THRESHOLD 0.95;
```

#### Semantics

- `SIMILAR TO target WITHIN HAMMING DISTANCE k`: Hamming distance ≤ k
- `SIMILAR TO target WITHIN COSINE THRESHOLD τ`: cosine similarity ≥ τ
- `SIMILAR TO target WITHIN JACCARD THRESHOLD τ`: Jaccard similarity ≥ τ
- The target can be a literal, a column, or a subquery

#### Math

- Hamming: XOR + popcount (uses `VPOPCNTDQ` kernel)
- Cosine: dot product + L2 norms (uses `VFMADD231PS` kernel)
- Jaccard: MinHash sketch + Broder's theorem

For LSH-accelerated search:
- Hash the target into LSH buckets
- Probe the buckets for candidates
- Re-rank candidates with exact distance

#### Planner behavior

1. Parse the similarity metric and threshold
2. If an LSH index exists on the column, use it
3. Otherwise, fall back to brute-force scan with the similarity kernel
4. For joins, use LSH on both sides to find candidate pairs

#### Open problems

- How do we auto-create LSH indexes? (Based on query frequency?)
- What's the right LSH parameter (L, k) for a given threshold?

---

### Q-04: Consistency level selection 🟡

**Status**: 🟡 partial (documented in P-04-06; syntax not implemented)
**Math**: V (sheaf theory for consistency gluing)
**Effort**: M
**Impact**: medium

#### Syntax

```sql
-- Strong consistency (CXL or local, single-rack)
SELECT * FROM orders CONSISTENCY STRONG WHERE id = 42;

-- Read-committed (NVMe with flush)
SELECT * FROM orders CONSISTENCY READ_COMMITTED WHERE date > '2024-01-01';

-- Eventual (cross-region async replica, may be stale)
SELECT * FROM orders CONSISTENCY EVENTUAL WHERE region = 'EU';
```

#### Semantics

- `STRONG`: linearizable; reads see the latest committed write
- `READ_COMMITTED`: snapshot isolation; reads see a committed snapshot
- `EVENTUAL`: may return stale data; suitable for analytics on cross-region
  replicas

#### Planner behavior

1. Map consistency level to protocol/tier:
   - STRONG → CXL (if available) or local DDR5
   - READ_COMMITTED → NVMe with flush
   - EVENTUAL → cross-region async replica
2. If the requested consistency can't be met (e.g., STRONG on cross-region
   data), error or fall back

#### Open problems

- How do we handle mixed consistency in a single query (e.g., STRONG join
  with EVENTUAL)?
- Can we prove the consistency guarantee via sheaf theory (P-05-10)?

---

### Q-05: Protocol-aware transactions 🔴

**Status**: 🔴 open
**Math**: V (linear types for protocol safety)
**Effort**: L
**Impact**: high

#### Syntax

```sql
-- Single-rack transaction (CXL-coherent, ~250 ns commit)
BEGIN TRANSACTION SCOPE RACK;
  UPDATE accounts SET balance = balance - 100 WHERE id = 1;
  UPDATE accounts SET balance = balance + 100 WHERE id = 2;
COMMIT;

-- Cross-rack transaction (Raft, ~10 µs commit)
BEGIN TRANSACTION SCOPE REGION;
  -- ... statements that touch multiple racks ...
COMMIT;

-- Cross-region (async, ms-class commit)
BEGIN TRANSACTION SCOPE GLOBAL ASYNC;
  -- ... statements that span regions ...
COMMIT;
```

#### Semantics

- `SCOPE RACK`: all participants must be in one rack; uses CXL coherence
- `SCOPE REGION`: participants can span racks; uses Raft over RoCEv2
- `SCOPE GLOBAL ASYNC`: participants span regions; async replication

#### Planner behavior

1. Analyze the transaction's participant set (which tablets are touched)
2. Pick the smallest scope that contains all participants
3. If the user-requested scope is too small, error

#### Open problems

- How do we detect the participant set before the transaction starts?
- Can we use Calvin's deterministic ordering to avoid 2PC for REGION scope?

---

### Q-06: Sketch-aware aggregations 🔴

**Status**: 🔴 open
**Math**: III (HLL, Count-Min, AMS, t-Digest)
**Effort**: M
**Impact**: medium

#### Syntax

```sql
-- Use a specific sketch for an aggregate
SELECT COUNT(DISTINCT user_id) USING HYPERLOGLOG FROM events;

-- Use Count-Min for heavy hitters
SELECT user_id, COUNT(*) USING COUNT_MIN FROM events
GROUP BY user_id ORDER BY count DESC LIMIT 10;

-- Use t-Digest for quantiles
SELECT PERCENTILE(latency, 0.99) USING T_DIGEST FROM requests;

-- Use AMS for F2 (sum of squares)
SELECT F2(amount) USING AMS FROM transactions;
```

#### Semantics

- `USING <sketch>`: force the planner to use a specific sketch
- Without `USING`, the planner picks based on the query's (ε, δ) requirements

#### Planner behavior

1. If `USING` is specified, use that sketch
2. Validate the sketch can answer the query (e.g., HLL can't do SUM)
3. If no sketch is specified, pick the minimal-cost one that meets (ε, δ)

#### Open problems

- How do we expose sketch parameters (e.g., HLL precision, Count-Min width)?
- Can we compose sketches (e.g., HLL inside Count-Min)?

---

### Q-07: Memory budget hints 🔴

**Status**: 🔴 open
**Math**: IV (knapsack, LP)
**Effort**: M
**Impact**: medium

#### Syntax

```sql
-- Tell the planner how much memory it can use
SELECT * FROM huge_table MEMORY BUDGET 4 GB WHERE ...;

-- Pin a subquery's working set to L3
SELECT * FROM (
  SELECT * FROM hot_table PIN TIER L3
) WHERE ...;
```

#### Semantics

- `MEMORY BUDGET X`: the query plan must fit in X bytes of memory
- `PIN TIER L3`: the subquery's working set must be in L3

#### Planner behavior

1. If `MEMORY BUDGET` is given, pick plans that fit (e.g., smaller hash
   tables, more spilling to CXL/NVMe)
2. If `PIN TIER` is given, ensure the working set is migrated to that tier
   before execution

#### Open problems

- How do we estimate the memory footprint of a plan?
- How do we handle spilling (to CXL? to NVMe?) when the budget is exceeded?

---

### Q-08: Energy-aware queries 🔴

**Status**: 🔴 open
**Math**: I (energy-per-instruction from `cpu_energy_kb.md`)
**Effort**: M
**Impact**: medium

#### Syntax

```sql
-- Run the query with an energy budget (joules)
SELECT * FROM big_table ENERGY BUDGET 100 JOULES WHERE ...;

-- Prefer energy-efficient execution (slower but greener)
SELECT * FROM big_table PREFER ENERGY EFFICIENT WHERE ...;

-- Report the energy used by the query
SELECT * FROM big_table REPORT ENERGY WHERE ...;
```

#### Semantics

- `ENERGY BUDGET X`: the query must not exceed X joules
- `PREFER ENERGY EFFICIENT`: pick the plan with lowest energy, not lowest latency
- `REPORT ENERGY`: include energy usage in the query result

#### Planner behavior

1. Estimate the energy of each plan (using the per-instruction energy model
   from `cpu_energy_kb.md`)
2. If `ENERGY BUDGET` is given, filter plans that exceed it
3. If `PREFER ENERGY EFFICIENT`, sort by energy instead of latency
4. Measure actual energy via RAPL (if available) and include in the result

#### Open problems

- How accurate is the energy model? (AMD RAPL is a model, not a measurement)
- Can we use DPU offload to reduce host CPU energy?

---

### Q-09: Streaming / continuous queries 🔴

**Status**: 🔴 open
**Math**: V (coalgebra — final coalgebra for streams)
**Effort**: XL
**Impact**: medium

#### Syntax

```sql
-- Continuous query: process new rows as they arrive
CONTINUOUS QUERY q1 AS
  SELECT user_id, COUNT(*) AS events_per_min
  FROM events
  GROUP BY user_id, TUMBLING WINDOW 1 MINUTE
  EMIT TO dashboard;

-- Windowed aggregation
SELECT user_id, AVG(latency) OVER (
  PARTITION BY user_id
  ORDER BY timestamp
  RANGE BETWEEN 1 HOUR PRECEDING AND CURRENT ROW
) FROM events;
```

#### Semantics

- `CONTINUOUS QUERY`: register a query that runs forever, processing new data
- `EMIT TO`: send results to a sink (table, stream, dashboard)
- Window functions: standard SQL window semantics

#### Planner behavior

1. Compile the continuous query to a streaming executor (coalgebraic —
   processes one row at a time)
2. Maintain state (windows, aggregates) in the appropriate tier
3. Emit results when windows close

#### Open problems

- How do we handle late-arriving data (out-of-order events)?
- How do we scale continuous queries across multiple nodes?

---

## Composition example

All extensions compose in a single query:

```sql
CONTINUOUS QUERY realtime_analytics AS
  SELECT
    user_id,
    COUNT(DISTINCT session_id) APPROXIMATE WITHIN 0.02 CONFIDENCE 0.95
      USING HYPERLOGLOG,
    AVG(latency) APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99,
    PERCENTILE(latency, 0.99) USING T_DIGEST
  FROM events
  CONSISTENCY READ_COMMITTED
  TIER CXL
  MEMORY BUDGET 8 GB
  ENERGY BUDGET 50 JOULES
  GROUP BY user_id, TUMBLING WINDOW 1 MINUTE
  EMIT TO dashboard;
```

This query:
1. Runs continuously, processing new events
2. Uses HyperLogLog for distinct count (with 2% error, 95% confidence)
3. Uses Hoeffding-bounded average (1% error, 99% confidence)
4. Uses t-Digest for p99 latency
5. Reads from CXL (cold-ish data)
6. Stays within 8 GB memory and 50 J energy
7. Provides read-committed consistency

The planner compiles this to a DAG of kernels, each with its (ε, δ)
guarantee propagated through the DAG.

---

## Summary

| # | Extension | Status | Math | Effort | Impact |
|---|-----------|--------|------|--------|--------|
| 01 | Approximate queries with (ε, δ) | 🟡 | III | L | critical |
| 02 | Tier hints | 🔴 | — | M | high |
| 03 | Similarity search and joins | 🔴 | III | L | high |
| 04 | Consistency level selection | 🟡 | V | M | medium |
| 05 | Protocol-aware transactions | 🔴 | V | L | high |
| 06 | Sketch-aware aggregations | 🔴 | III | M | medium |
| 07 | Memory budget hints | 🔴 | IV | M | medium |
| 08 | Energy-aware queries | 🔴 | I | M | medium |
| 09 | Streaming / continuous queries | 🔴 | V | XL | medium |

**Total: 9 extensions, 6 open, 3 partial.**

The critical-path extensions are Q-01 (approximate queries), Q-03 (similarity),
and Q-05 (protocol-aware transactions). These three define the engine's unique
value proposition.
