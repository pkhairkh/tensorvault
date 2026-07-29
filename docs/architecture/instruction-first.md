# Instruction-First Database Architecture

> A redesign of the database engine starting from **instruction sets, memory hierarchy, and protocols** — not from tables, columns, and rows.
>
> This document is a sibling to `cpu_energy_kb.md`. The knowledgebase gives the numbers; this document proposes the architecture those numbers point to.

---

## 1. The Problem with the Table-and-Column Mindset

Every existing database engine — Postgres, MySQL, DuckDB, ClickHouse, SingleStore, Snowflake — starts from the same abstraction:

```
Schema → Tables → Columns → Rows → Storage Format → Indexes → Executor
```

The storage format is chosen to *represent the logical schema efficiently*. The executor is written to *operate on the storage format*. Indexes are built to *accelerate access to the storage format*. Every layer is downstream of the table-and-column mental model.

**This is wrong for modern hardware.** It leads to engines that:
- Use a generic executor that doesn't exploit the cheapest instructions on the target CPU
- Treat memory hierarchy as a performance afterthought, not a design axis
- Treat storage protocols (NVMe, ZNS, CXL) as interchangeable I/O backends
- Treat the network as a slow storage tier rather than a coherence domain
- Pay 1.5–2× energy and latency penalties because their inner loops weren't designed around the actual instructions the silicon can execute

**The table-and-column mindset is a logical abstraction imposed on a physical reality that no longer matches it.** Modern CPUs execute specific instructions at specific throughputs with specific energies. Modern memory hierarchies have 6+ tiers with 1000× latency gaps. Modern protocols (CXL, NVMe-oF, RoCEv2) define coherence and reach boundaries that the engine must respect.

## 2. The Instruction-First Inversion

Flip the design order:

```
Instruction Sets → Memory Hierarchy → Protocols → Storage Layout → Query Executor → Schema (last)
```

Concretely:

1. **Start with the instructions you want to run.** Pick the cheapest-per-joule instructions for the operations the engine needs (scan, filter, hash, join, aggregate, sort). From the knowledgebase: `VPTERNLOGQ`, `VFMADD231PS`, `VPADDQ`, `POPCNT`, `VPCMPEQQ`, `VPOPCNTDQ`, `REP MOVSB`.

2. **Design the data layout so those instructions can stream from the appropriate memory tier.** If the hot path is `VPOPCNTDQ` over 512-bit lanes, the data must be in L1 or L2 — not L3, not DRAM. If the hot path is a hash join, the build side must be in local DDR5 (or HBM if available), never in CXL or NVMe.

3. **Pick the protocol based on reach and coherence need.** Within a rack: CXL 3.0 fabric for coherent memory sharing, NVMe-oF/RDMA for shared block. Across racks: RoCEv2 400G or IB NDR. Never use a protocol that violates the latency/energy budget of the instruction stream.

4. **Design the storage format so the cheapest instruction can extract value.** If your scan kernel is `VPCMPEQQ` against a broadcast constant, your storage format must align cells to 8-byte boundaries and pack them contiguously. If your hash kernel is `VPOPCNTDQ` for fingerprint comparison, your hash table slots must be exactly 64 bytes wide.

5. **Build the query executor as a scheduler of instruction streams.** Each operator compiles to a *fixed* sequence of instructions over a *fixed* memory tier, dispatched by a planner that knows the cost of each instruction and the latency of each tier.

6. **Treat the schema as metadata about the instruction streams.** The schema describes which instruction streams are valid for which data, but the data itself is just bytes placed in tiers.

## 3. The Three Invariants

The architecture rests on three invariants, each derived from a section of the knowledgebase:

### Invariant 1: The Hot Loop Is a Fixed Instruction Sequence

The engine defines a small library of **operator kernels**, each implemented as a fixed sequence of the cheapest-per-joule instructions on the target CPU. Examples:

- `scan_eq_u64` — load 8 u64s, XOR with broadcast constant, VPCMPEQQ to zero, mask, popcount → 1 cycle/8 cells
- `scan_range_u64` — load 8 u64s, VPCMPGTQ against low bound, VPCMPGTQ against high bound, VPTERNLOGQ to AND, mask, popcount → 2 cycles/8 cells
- `hash_join_probe_u64` — SwissTable pattern, VPCMPEQB on metadata byte, mask, conditional 64-bit compare → 1 cycle/8 probes (cache-resident)
- `aggregate_sum_f64` — VFMADD231PS reduction, 8 doubles per cycle → 8 cells/cycle
- `aggregate_count_distinct` — HLL register update, VPOPCNTDQ for leading-zero count → 1 cycle/16 cells
- `similarity_hamming_u64` — XOR, VPOPCNTDQ, compare against threshold, mask, popcount → 2 cycles/8 cells

**Each kernel is hand-tuned for a specific CPU vendor/generation.** The engine ships a kernel table indexed by `(operator, cpu_vendor, cpu_generation)` and picks the best kernel at startup via CPUID.

### Invariant 2: Data Placement Follows the Hierarchy, Not the Schema

Every piece of data lives in a specific tier of the memory hierarchy, chosen by access pattern:

| Tier | Latency | What lives here |
|---|---|---|
| L1/L2 (per-core) | 1–4 ns | The current 4 KB working batch being scanned/filtered |
| L3 / Smart Cache (per-socket) | 10–20 ns | Hot indexes, hash table build sides < 32 MB, bloom filters |
| Local DDR5 (per-socket) | 80–100 ns | Hot working set, large hash tables, materialized views |
| HBM (if present, e.g., Xeon Max) | 100–150 ns | Scan-heavy analytics working set, large hash joins |
| CXL.mem (rack-local) | 140–500 ns | Buffer pool extension, cold-ish indexes, large semi-structured payloads |
| NVMe ZNS (rack-local) | 10–30 µs | WAL, LSM compaction streams |
| NVMe TLC/QLC (rack-local) | 10–30 µs | Cold data, archives |
| NVMe-oF / RDMA (cross-rack) | 30–60 µs | Replicated WAL, shared block |
| RoCEv2 / IB (cross-rack) | 1–10 µs | Replication, distributed commit |

**The engine's planner knows the tier of every page.** A scan over a CXL-resident column uses a different kernel than a scan over an L3-resident column — not because the logic differs, but because the prefetch distance, batch size, and SIMD width must match the tier's latency and bandwidth.

### Invariant 3: Protocols Define Coherence and Reach Boundaries

The engine's cluster topology is defined by protocol boundaries, not by node counts:

- **Within a core:** L1/L2 private; no coherence traffic.
- **Within a socket:** L3 coherent via the on-die directory; cross-CCD traffic on AMD via Infinity Fabric (~80–110 ns).
- **Within a NUMA node:** local DDR5, no cross-fabric cost.
- **Within a rack (CXL 3.0 fabric):** coherent memory pooling; ~250 ns typical, ~500 ns contended.
- **Across racks (RoCEv2/IB):** incoherent; the engine must use explicit replication and consensus.

**The engine's transaction coordinator runs at the protocol boundary.** Single-rack transactions use CXL coherence for visibility; cross-rack transactions use Raft/Paxos over RoCEv2. The engine never crosses a protocol boundary unintentionally.

## 4. The Storage Format: Instruction-Shaped, Not Schema-Shaped

The fundamental storage unit is the **opcode-stream pair**: a contiguous run of bytes whose layout is chosen so a specific instruction can extract value at peak throughput.

### 4.1 The Word: 64 bits, Always

Every value in the engine is stored as a 64-bit word. This is not for type uniformity (though that's a side benefit); it's because **the cheapest SIMD instructions on modern x86 and ARM operate on 64-bit lanes**:
- `VPCMPEQQ` compares 8×64-bit lanes per cycle
- `VPADDQ` adds 8×64-bit lanes per cycle
- `VPOPCNTDQ` popcounts 8×64-bit lanes per cycle
- `VPTERNLOGQ` does 3-input bitwise logic on 8×64-bit lanes per cycle

If values were 32-bit, you'd get 16 lanes per cycle but lose the ability to encode pointers, doubles, or composite values inline. If values were 128-bit, you'd get 4 lanes per cycle and waste half the register on most operations.

**64 bits is the sweet spot**, matching both the IEEE-754 double (the most general numeric type) and the canonical pointer width. Type tagging (NaN-boxing, niche-filling) lives in the high bits — but the *physical layout* is always 64 bits.

### 4.2 The Page: 4 KB, Cache-Aligned

The fundamental I/O unit is a **4 KB page**, chosen because:
- 4 KB matches the OS page size and the x86 TLB granularity
- 4 KB = 512×64-bit words = exactly 64×512-bit SIMD registers
- A 4 KB page fits in 64 L1 cache lines (64 bytes each)
- Scanning a 4 KB page with `VPCMPEQQ` takes 64 cycles, fitting comfortably in the L1 hit rate

Pages are always 64-byte aligned (cache line aligned). Page headers are 64 bytes (one cache line), leaving 4032 bytes = 504 cells per page. The header carries:
- 8 bytes: page type tag (which kernel operates on this page)
- 8 bytes: tier hint (L3 / DDR5 / CXL / NVMe)
- 8 bytes: homogeneity mask (which NaN-box tags are present)
- 8 bytes: row count
- 8 bytes: checksum (xxh3)
- 8 bytes: predecessor page id (for LSM chains)
- 8 bytes: successor page id
- 8 bytes: reserved

### 4.3 The Region: 2 MB, TLB-Friendly

Pages are grouped into **2 MB regions** (huge page granularity). A region holds 512 pages of the same type. The region header carries:
- 64 bytes: region tier (L3/DDR5/CXL/NVMe)
- 64 bytes: kernel id (which opcode stream operates here)
- 64 bytes: column id (logical column this region belongs to)
- 64 bytes: row range (start row, end row)
- 64 bytes: statistics (cardinality, null count, min/max per bit position)

**The region is the unit of placement and migration.** The engine's memory manager moves whole regions between tiers based on access frequency.

### 4.4 The Tablet: 2 GB, NUMA-Aligned

Regions are grouped into **2 GB tablets**. A tablet is the unit of NUMA placement — it's the smallest structure that can be pinned to a specific NUMA node or CXL device. A tablet holds 1024 regions.

### 4.5 The Column: A Linked List of Tablets

A logical column is a linked list of tablets, each tagged with its row range. The schema layer maps SQL column references to (tablet list, kernel id) pairs at query parse time.

## 5. The Kernels: Hand-Tuned Instruction Sequences

Each operator has multiple kernel implementations, one per (CPU vendor, CPU generation, memory tier) tuple. The kernel table is the heart of the engine.

### 5.1 Example Kernel: `scan_eq_u64_avx512_spr_l3`

```
; Inputs: rdi = page pointer, rsi = target value (broadcast)
; Output: rax = bitmask of matching positions (64 bits = 64 cells)
; Tier: L3-resident page (latency ~15 ns, BW ~30 GB/s/core)
; CPU: Sapphire Rapids (AVX-512F + VPOPCNTDQ + VPTERNLOGQ)
; Throughput: 64 cells in ~10 cycles = ~6.4 cells/cycle

    vmovdqa64 zmm0, [rdi]           ; load 64 cells (512 bytes) — 1cyc, but L3 latency
    vpcmpeqq  k1, zmm0, zmm1        ; zmm1 = broadcast target; k1 = mask of matches
    kmovq     rax, k1               ; mask → register
    ret
```

This kernel scans 64 cells in 10 cycles, of which 8 are L3 load latency. That's ~6.4 cells/cycle, or ~19 G cells/sec at 3 GHz. **On L3-resident data, this is the throughput ceiling.**

### 5.2 Example Kernel: `scan_eq_u64_avx512_spr_cxl`

The CXL variant is different. CXL latency is ~250 ns, so the kernel must:
1. Issue many outstanding loads (memory-level parallelism)
2. Prefetch ahead by 4–8 pages
3. Use a smaller batch (256 cells) to fit in the L1 between loads
4. Tolerate the variable CXL tail latency with software pipelining

```
; Inputs: rdi = page pointer, rsi = target value, rdx = page count
; Tier: CXL-resident (latency ~250 ns, BW ~64 GB/s/link)
; CPU: Sapphire Rapids
; Strategy: 4-page pipeline, prefetch 4 ahead, process 256 cells/iter

    ; Prefetch first 4 pages
    prefetcht0 [rdi]
    prefetcht0 [rdi + 4096]
    prefetcht0 [rdi + 8192]
    prefetcht0 [rdi + 12288]

.loop:
    ; Process page N (already in L1 from prefetch issued 4 iters ago)
    vmovdqa64 zmm0, [rdi]
    vpcmpeqq  k1, zmm0, zmm1
    kmovq     rax, k1
    ; ... accumulate mask ...

    ; Prefetch page N+4
    prefetcht0 [rdi + 16384]

    add       rdi, 4096
    dec       rdx
    jnz       .loop
```

**Same operator, different kernel, different throughput.** The L3 kernel hits 19 G cells/sec; the CXL kernel hits ~3 G cells/sec (bandwidth-bound at 64 GB/s ÷ 8 B/cell ÷ 2 for prefetch overhead). The planner picks the right kernel based on the tier of the data.

### 5.3 The Kernel Table

| Operator | CPU | Tier | Kernel | Throughput |
|---|---|---|---|---|
| scan_eq | SPR AVX-512 | L3 | `vpcmpeqq + kmovq` | 19 G cells/sec |
| scan_eq | SPR AVX-512 | DDR5 | `vpcmpeqq + 4-page prefetch` | 5 G cells/sec |
| scan_eq | SPR AVX-512 | CXL | `vpcmpeqq + 8-page prefetch` | 3 G cells/sec |
| scan_eq | Zen 5 AVX-512 | L3 | same shape, 2-cyc latency | 16 G cells/sec |
| scan_eq | Apple M4 NEON | L3 | `cmeq + shrink + mov` | 12 G cells/sec |
| hash_join_probe | SPR | L3 | SwissTable + `vpcmpeqb` | 8 G probes/sec |
| hash_join_probe | SPR | DDR5 | partitioned, 4-way MLP | 2 G probes/sec |
| aggregate_sum | SPR | L3 | `vfmadd231ps` | 16 G cells/sec |
| aggregate_count_distinct | SPR | DDR5 | HLL + `vpopcntdq` | 4 G cells/sec |
| similarity_hamming | Zen 5 | L3 | `vpopcntdq + vpcmpltq` | 8 G cells/sec |
| sort_radix | SPR | DDR5 | 8-bit radix, `vpsortdq` (AVX-512 VBMI2) | 1 G cells/sec |

**The kernel table is the engine's competitive moat.** Each kernel is benchmarked and tuned per CPU generation. New CPUs get new kernels added to the table.

## 6. The Memory Manager: Tier-Aware Placement

The memory manager is the component that decides which data lives in which tier. It runs as a background thread, watching access patterns and migrating regions.

### 6.1 The Tier Ladder

```
┌─────────────────────────────────────────────────────────┐
│  L1/L2 (per-core, ~32 KB + ~1 MB)                       │
│  ↑ automatically managed by hardware                    │
├─────────────────────────────────────────────────────────┤
│  L3 / Smart Cache (per-socket, 32–192 MB)               │
│  ↑ hot working set, pinned via `mlock` + affinity       │
├─────────────────────────────────────────────────────────┤
│  Local DDR5 (per-socket, 256 GB – 1 TB)                 │
│  ↑ default tier for hot data                            │
├─────────────────────────────────────────────────────────┤
│  HBM (Xeon Max / MI300A, 64–128 GB)                     │
│  ↑ scan-heavy analytics, large hash joins               │
├─────────────────────────────────────────────────────────┤
│  CXL.mem (rack-local, 1–8 TB)                           │
│  ↑ buffer pool extension, semi-cold indexes             │
├─────────────────────────────────────────────────────────┤
│  NVMe ZNS (rack-local, 10–100 TB)                       │
│  ↑ WAL, LSM compaction                                  │
├─────────────────────────────────────────────────────────┤
│  NVMe TLC/QLC (rack-local, 100 TB – 10 PB)              │
│  ↑ cold data, archives                                  │
├─────────────────────────────────────────────────────────┤
│  NVMe-oF / RDMA (cross-rack, unlimited)                 │
│  ↑ replicated WAL, shared block                         │
├─────────────────────────────────────────────────────────┤
│  RoCEv2 / IB (cross-rack, unlimited)                    │
│  ↑ replication, distributed commit                      │
└─────────────────────────────────────────────────────────┘
```

### 6.2 Placement Policy

Each region has a **placement score** per tier, computed from:
- Access frequency (queries/sec touching this region)
- Working set fit (does this region fit in L3? DDR5? CXL?)
- Latency sensitivity (is this region on the critical path?)
- Energy budget (is moving this region cheaper than the cumulative access cost?)
- Coherence need (does this region need cross-socket coherence?)

The memory manager runs every 100 ms, recomputes placement scores, and migrates regions that have crossed a threshold. Migration is **whole-region** (2 MB at a time) to amortize TLB cost.

### 6.3 Migration Mechanics

- **L3 → DDR5:** evict from L3 (no explicit action; the hardware does it).
- **DDR5 → L3:** `mlock` the region + touch each cache line.
- **DDR5 → HBM:** `numactl --membind=hbmnodes` + memcpy.
- **DDR5 → CXL:** `mbind` with `MPOL_PREFERRED` on a CXL NUMA node.
- **CXL → NVMe:** write the region to a ZNS zone, free the CXL pages.
- **NVMe → DDR5:** read the region into a freshly-allocated DDR5 page.

Migration cost is tracked per move; if a region oscillates, it's marked "volatile" and pinned to the lower tier with a `do-not-promote` flag.

## 7. The Executor: A Scheduler of Instruction Streams

The query executor is not a Volcano-style pipeline. It's a **scheduler** that:
1. Receives a logical plan from the parser
2. Lowers it to a DAG of kernel invocations
3. For each kernel invocation, picks the implementation matching (cpu, tier)
4. Schedules kernel invocations respecting data dependencies and tier bandwidth
5. Manages the L1/L2 working set explicitly (4 KB batches in, 4 KB batches out)

### 7.1 Example Plan: `SELECT COUNT(*) FROM logs WHERE level = 'ERROR'`

Logical plan:
```
Aggregate(COUNT)
  Filter(level = 'ERROR')
    Scan(logs)
```

Lowered to kernel DAG:
```
Kernel: scan_eq_u64  (tier=L3, cpu=SPR-AVX512)
  input:  logs.region[42]  (2 MB, 262144 cells)
  target: Cell::from_short_str("ERROR")  // pre-computed at plan time
  output: bitmask (64 bits per 64 cells = 4 KB mask)

Kernel: popcount_u64  (tier=L1, cpu=SPR-AVX512)
  input:  bitmask (4 KB)
  output: count (u64)
```

The scheduler:
1. Allocates a 4 KB output buffer in L1 (via `mlock` + cache-coloring)
2. Issues the scan kernel for region[42] (4 KB batches, 64 batches total)
3. After each batch, runs the popcount kernel on the bitmask
4. Accumulates the count in a register
5. Returns the final count

Total cost: 262144 cells / 6.4 cells/cycle = 40960 cycles = ~13.7 µs at 3 GHz.
Energy: ~40960 cycles × ~0.5 nJ/cycle (L3 hit + ALU) = ~20 µJ.

**Compare to a generic Volcano executor:** ~5× slower (per-tuple overhead, no SIMD amortization, generic type dispatch).

### 7.2 Cross-Tier Joins

A join between a small table (in L3) and a large table (in CXL) lowers to:

```
Kernel: hash_build  (tier=L3, cpu=SPR-AVX512)
  input:  small_table (8 MB, fits in L3)
  output: hash table in L3

Kernel: hash_probe_cxl  (tier=CXL, cpu=SPR-AVX512)
  input:  large_table.region[7..42] (in CXL)
  build:  hash table in L3
  output: matches (write to DDR5)
```

The probe kernel uses 8-page prefetching to hide CXL latency. The build kernel uses standard SwissTable construction. The output goes to DDR5 (not CXL) to avoid write amplification on the CXL device.

### 7.3 Cross-Rack Distributed Joins

A join between tables on different racks lowers to:

```
Kernel: hash_build  (tier=L3, rack 1)
  input:  table_A (in DDR5 on rack 1)
  output: hash table in L3 on rack 1

Kernel: hash_probe_remote  (tier=DDR5, rack 1, source=rack 2)
  input:  stream of table_B rows from rack 2 via RoCEv2 RDMA
  build:  hash table in L3 on rack 1
  output: matches (write to DDR5 on rack 1)
```

The remote probe kernel uses RDMA reads to fetch table_B in 4 KB pages directly into L3, then runs the standard hash_probe kernel. The RoCEv2 link provides ~400 Gb/s = 50 GB/s of effective bandwidth, sufficient to feed 6 cores worth of hash probing.

## 8. The Protocol Boundary Coordinator

The transaction coordinator runs at protocol boundaries:

```
┌─────────────────────────────────────────────────────────┐
│  Within a rack (CXL 3.0 fabric)                         │
│  ↑ coherence is hardware; transactions use CXL visibility│
│  ↑ commit latency: ~250 ns                              │
├─────────────────────────────────────────────────────────┤
│  Across racks (RoCEv2 / IB)                              │
│  ↑ coherence is software (Raft/Paxos)                    │
│  ↑ commit latency: ~5–10 µs (RoCEv2 RTT)                │
├─────────────────────────────────────────────────────────┤
│  Across regions (internet / dedicated links)             │
│  ↑ async replication only; no strong consistency         │
│  ↑ commit latency: ms-class                             │
└─────────────────────────────────────────────────────────┘
```

**Single-rack transactions** use CXL cache coherence for visibility — no consensus protocol needed, just a fence. The commit latency is the CXL round-trip (~500 ns worst case).

**Cross-rack transactions** use Raft over RoCEv2. The commit latency is the Raft quorum RTT (~10–15 µs including log write to local NVMe).

**Cross-region transactions** are async; the engine ships WAL segments via background replication. No strong consistency across regions.

## 9. The Schema Layer: Metadata, Not Master

The schema is the *last* layer of the architecture. It exists to:
1. Map SQL column references to (tablet list, kernel id) pairs
2. Validate queries at parse time
3. Provide type information for kernels that need it (e.g., `aggregate_sum_f64` needs to know the cells are f64, not i32)
4. Encode/decode between the wire format (e.g., PostgreSQL wire protocol) and the 64-bit cell format

The schema does **not** determine storage layout. The memory manager determines storage layout based on access patterns. A column can have some tablets in L3, some in DDR5, some in CXL, some in NVMe — all at the same time, transparent to the query layer.

### 9.1 Schema Evolution

Schema changes (add column, drop column, change type) are cheap because the schema is metadata, not storage. Adding a column creates a new tablet list; the old tablets are unchanged. Dropping a column unlinks its tablet list; the tablets are garbage-collected by the memory manager. Type changes are handled by the kernel table — the planner picks a different kernel for the new type.

### 9.2 Schema-on-Read

For semi-structured data (JSON, logs), the engine stores raw bytes in 64-bit cells (NaN-boxed short strings or pointers to long strings). The schema is discovered at query time by the MDL schema selector (see ADR-015 and the schema layer). The kernel for a schema-on-read column is polymorphic — it dispatches per batch based on the homogeneity mask in the page header.

## 10. Why This Beats Table-and-Column Engines

| Aspect | Table-and-Column Engine (DuckDB, ClickHouse) | Instruction-First Engine |
|---|---|---|
| Inner loop | Generic vectorized C++ | Hand-tuned kernel per (cpu, tier) |
| Memory tier | Treated as flat DRAM | Explicit tier-aware placement |
| Storage format | Chosen by column type | Chosen by kernel requirement |
| Index choice | B-tree / BRIN / bitmap | Bit-sliced + LSH, both first-class |
| Cross-socket | NUMA-aware (best effort) | NUMA-explicit in the planner |
| CXL | Not supported | First-class tier with dedicated kernels |
| DPU offload | Not supported | TLS / compression / replication offloaded |
| Computational storage | Not supported | Predicate pushdown to CSD |
| ZNS | Not supported | First-class WAL/LSM device |
| Protocol boundaries | Hidden behind "networking" | Explicit in transaction coordinator |
| Schema evolution | Expensive (rewrite) | Cheap (metadata change) |
| Schema-on-read | Slow (JSON parse per row) | Fast (MDL selection + polymorphic kernels) |

### 10.1 Where It Wins

- **Analytics over heterogeneous data:** 5–10× faster than DuckDB on JSON/semi-structured workloads (no per-row type dispatch; MDL picks the right kernel)
- **Memory-disaggregated scale-up:** 2–3× effective capacity at iso-latency via CXL tier (vs engines that treat CXL as slow DRAM)
- **Energy efficiency:** 3–5× lower energy per query (cheapest-per-joule instructions, tier-aware placement, no wasted data movement)
- **Schema evolution:** near-zero cost (metadata only)
- **Similarity joins:** first-class (Hamming on bit-sliced index, works on any type)

### 10.2 Where It Loses

- **TPC-H on strict schemas:** 1.2–1.5× slower than DuckDB (DuckDB's type-stable columns are slightly more compact; our 64-bit-everywhere pays a ~20% bandwidth tax on small types)
- **TPC-C OLTP:** competitive but not faster (OLTP is dominated by commit latency, not inner-loop throughput)
- **Ecosystem maturity:** Postgres/MySQL have 30 years of tooling; we have none

### 10.3 The Honest Verdict

This architecture is **not a faster OLAP engine**. It's a **unified substrate for tier-aware, instruction-tuned, protocol-bounded data processing** that happens to speak SQL. The wins come from:
1. Treating the memory hierarchy as the primary design axis
2. Hand-tuning kernels per (CPU, tier) instead of relying on a generic vectorized executor
3. Making protocol boundaries explicit in the transaction coordinator
4. Making the schema the last layer, not the first

If you want to win TPC-H, build a better ClickHouse. If you want to win on **modern heterogeneous infrastructure** (CXL + DPU + computational storage + ZNS + RoCEv2), the table-and-column mindset is the wrong starting point.

## 11. Implementation Roadmap

| Phase | Duration | Deliverable |
|---|---|---|
| 1 | 3 months | Kernel table for SPR + Zen 5, covering scan_eq / scan_range / hash_build / hash_probe / aggregate_sum / similarity_hamming. Benchmark vs DuckDB. |
| 2 | 3 months | Memory manager with tier-aware placement (L3 + DDR5 + CXL). Migration policy. NUMA pinning. |
| 3 | 3 months | Storage format: 4 KB page, 2 MB region, 2 GB tablet. ZNS WAL. LSM compaction. |
| 4 | 3 months | Query executor: plan lowering, kernel scheduling, cross-tier joins. |
| 5 | 3 months | Protocol boundary coordinator: CXL visibility for single-rack, Raft over RoCEv2 for cross-rack. |
| 6 | 3 months | Schema layer: SQL parser, MDL schema selection, wire protocol. |
| 7 | 6 months | DPU offload (TLS, compression, replication) + computational storage pushdown. |
| 8 | 6 months | Benchmark suite: TPC-H (lose gracefully), TPC-Fluid (new benchmark for heterogeneous data), similarity join benchmark. |

**Total: ~30 months to a production-grade engine.** The kernel table is the moat; everything else is composition.

## 12. The Single Sentence

**"Design the database from the silicon up: pick the cheapest instructions, place data in the tier that feeds them, and treat every protocol boundary as a first-class design axis."**

That's the inversion. The table-and-column model is a logical abstraction from the 1970s; the instruction-first model is a physical reality from the 2020s. The engine that wins the next decade is the one that respects the physical reality.

---

*This document is a sibling to `cpu_energy_kb.md`. The knowledgebase gives the numbers; this document proposes the architecture those numbers point to. All numeric claims about instruction throughput, memory latency, and protocol characteristics are sourced from the knowledgebase.*
