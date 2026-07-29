# Benchmarks

> Analysis of standard benchmarks (TPC-C, TPC-H) and the engine's expected
> performance on each.

## Documents

| Document | Lines | What it covers |
|----------|-------|----------------|
| **[tpcc-analysis.md](./tpcc-analysis.md)** | ~700 | TPC-C spec, bottleneck analysis, concurrency control comparison, world records, the math of New-Order, the path to beating it |
| **[tpcc-math.md](./tpcc-math.md)** | ~400 | Rigorous mathematical companion: per-transaction cost model, throughput under each CC protocol, the 12.86 tpmC/warehouse spec ceiling, the $/tpmC equation |

## The key findings

### TPC-C (the win path)

- **Spec ceiling**: 12.86 tpmC/warehouse — the hard limit
- **Our path**: one 16 TB DRAM box → 160K warehouses → 2.06 B tpmC
- **Energy**: ~20,000 tpmC/mJ vs PolarDB's ~1,750 tpmC/mJ = **11× better**
- **Cost**: ~$0.22/tpmC vs PolarDB's $0.11/tpmC (but 1 node vs 2,340)

### TPC-H (the accepted loss)

- We lose to DuckDB by 1.2–1.5× — structurally
- DuckDB's type-stable columns are more compact than 64-bit-everywhere
- No amount of kernel tuning closes this gap
- We accept the loss and focus on schema-fluid workloads instead

## The 5 levers for TPC-C

1. AVX-512 hash indexes (kill the 68% index traversal overhead)
2. Per-thread epoch batching (kill the centralized mutex)
3. Branchless SIMD validation (kill the 30% OCC overhead)
4. CXL-attached cap-backed DRAM log (37× tail latency win)
5. Deterministic partitioning for the 88% single-partition txns
