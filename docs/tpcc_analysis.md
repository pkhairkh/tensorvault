# TPC-C: Bottlenecks, Concurrency Control, and the Instruction-Level Path to Beating It

> A research brief for a custom OLTP engine. All numbers are cited inline from the
> spec, peer-reviewed papers, vendor disclosures, and the in-repo CPU/energy
> knowledgebase (`cpu_energy_kb.md`). Where a figure is derived (not directly
> stated by a source) it is marked *(derived)*.

---

## Table of Contents
1. [TPC-C spec: 5 txns, schema, scaling rules](#1-tpc-c-spec)
2. [New-Order bottleneck analysis: where does the time go?](#2-new-order-bottleneck-analysis)
3. [Concurrency-control comparison table](#3-concurrency-control-comparison)
4. [TPC-C world records 2010–2025](#4-tpc-c-world-records-2010-2025)
5. [The math: rows touched, working set, theoretical tpmC/core](#5-the-math)
6. [Logging: WAL, group commit on NVMe, CXL.mem for the log](#6-logging)
7. [Partitioning: why TPC-C scales on warehouse_id, and Calvin](#7-partitioning)
8. [Instruction-level analysis of the New-Order hot loop](#8-instruction-level-analysis)
9. [Can we beat TPC-C? Levers, theoretical ceilings, who is closest](#9-can-we-beat-tpc-c)

---

## 1. TPC-C spec

TPC-C is the TPC's OLTP benchmark, simulating a wholesale supplier with multiple
warehouses. The current revision is **v5.11.0 (Feb 2010)**. It is "a 'business
throughput' measuring the number of orders processed per minute." ([TPC-C v5.11.0 spec](https://www.tpc.org/tpc_documents_current_versions/pdf/tpc-c_v5.11.0.pdf); [Jim Gray, Benchmark Handbook ch.12](https://jimgray.azurewebsites.net/BenchmarkHandbook/chapter12.pdf)).

### 1.1 The five transactions and their mix

| Txn | Description | Mix share | Read/Write profile |
|---|---|---|---|
| **New-Order** | Enter a new order for ~10 items from a warehouse | **≤ 45%** (the measured metric) | read-heavy + ~10 stock updates + 3 inserts |
| **Payment** | Update customer balance & warehouse/district YTD | **≥ 43%** | short write (1 warehouse, 1 district, 1 customer) |
| **Order-Status** | Query status of a customer's last order | **≥ 4%** | read-only |
| **Delivery** | Batch-deliver up to 10 oldest new orders | **≥ 4%** (batch) | delete + update, large footprint |
| **Stock-Level** | Count low-stock items in a district | **≥ 4%** | read-only scan |

The spec sets **minimum** percentages for every type *except* New-Order; only
New-Order transactions are counted in **tpmC** = "New-Order transactions executed
per minute" ([openGauss TPC-C doc](https://docs.opengauss.org/en/docs/5.1.0/docs/DatabaseAdministrationGuide/mot-sample-tpc-c-benchmark.html); [HammerDB workload guide](https://www.hammerdb.com/docs3.3/ch03s05.html)). New-Order + Payment therefore dominate: **≈88% of all executed transactions** ([Tanabe et al., CCBench/VLDB 2020](https://vldb.org/pvldb/vol13/p3531-tanabe.pdf)).

### 1.2 Schema (9 tables, 10 FK relationships)

The schema is warehouse-centric: every row (except ITEM) carries a `warehouse_id`
as the ancestral foreign key ([DBx1000 paper, VLDB 2014](https://www.vldb.org/pvldb/vol8/p209-yu.pdf)).

| Table | Rows (per W warehouses) | Role | Access pattern |
|---|---|---|---|
| **Warehouse** | W | top-level entity; `W_YTD` hot field | read + Payment writes |
| **District** | 10·W | 10 districts/warehouse; `D_NEXT_O_ID` hot counter | read + New-Order increments |
| **Customer** | 30,000·W | 3,000 customers/district | read; Payment writes balance |
| **History** | grows | one row per Payment | insert-only |
| **Item** | 100,000 (fixed, shared) | read-only catalog | read-only, never modified |
| **Stock** | 100,000·W | one row per (warehouse,item); `S_QUANTITY` updated | New-Order updates ~10/txn |
| **New-Order** | ~9,000·W (queue) | pending orders awaiting delivery | insert + Delivery deletes |
| **Order** | grows with run | one per order | insert + read |
| **Order-Line** | ~10× orders | one line per item ordered | insert + read |

([Wikipedia: TPC-C](https://en.wikipedia.org/wiki/TPC-C); [BenchmarkSQL docs](https://benchmarksql.readthedocs.io/en/latest/TPCC); [TPC spec §1](https://www.tpc.org/tpc_documents_current_versions/pdf/tpc-c_v5.11.0.pdf)).

### 1.3 Scaling rules

- **10 terminals per warehouse**, all five txn types runnable at each terminal
  ([Fujitsu TPC-C overview](https://sp.ts.fujitsu.com/dmsp/Publications/public/Benchmark_Overview_TPC-C.pdf)).
- The database, terminal count, and measured throughput **scale together**: to
  increase tpmC you must add warehouses and their terminals ([Jim Gray](https://jimgray.azurewebsites.net/BenchmarkHandbook/chapter12.pdf)).
- **Theoretical maximum throughput = 12.86 tpmC per warehouse**, enforced by the
  terminal keying/think-time model. "TPC-C implementations must scale both the
  number of terminals and the size of the database proportionally to the computing
  power of the measured system" ([TPC spec](https://www.tpc.org/tpc_documents_current_versions/pdf/tpc-c_v5.11.0.pdf); [YDB docs](https://ydb.tech/docs/en/reference/ydb-cli/workload-tpcc); [Wikipedia](https://en.wikipedia.org/wiki/TPC-C)).
  - openGauss notes the audited band: **tpmC/warehouse must be > 9 and < 12.86** ([openGauss](https://docs.opengauss.org/en/docs/5.1.0/docs/DatabaseAdministrationGuide/mot-sample-tpc-c-benchmark.html)).
  - Equivalently ≈ **0.214 New-Order txns/s per warehouse** *(derived: 12.86/60)*.
- A TPC-C result also discloses **price/performance ($/tpmC)** over 5-year TCO
  ([TPC pricing spec](https://www.tpc.org/tpc_documents_current_versions/pdf/tpc-pricing_v1.1.0.pdf)).

---

## 2. New-Order bottleneck analysis

### 2.1 What New-Order actually does

A New-Order transaction for a (warehouse, district) pair:
1. Reads the **Warehouse** row (for tax rate).
2. Reads/updates the **District** row — increments `D_NEXT_O_ID` (the next order
   id, a *monotonic counter* — the classic hot-spot).
3. Reads the **Customer** row.
4. For each of 5–15 (avg **10**) items: reads the read-only **Item** row, then
   reads/updates the **Stock** row (`S_QUANTITY`, `S_YTD`, `S_ORDER_CNT`, …).
5. Inserts one **Order**, one **New-Order**, and ~10 **Order-Line** rows.

Average rows touched ≈ **23 reads + 11 writes + 12 inserts ≈ 46 rows**, with ~10
stock updates ([BenchmarkSQL](https://benchmarksql.readthedocs.io/en/latest/TPCC); [ROCOCO, OSDI'14](http://mpaxos.com/pub/rococo-osdi14.pdf): "a complete TPC-C new order transaction updates a highly contended order-id data field as well as 10 purchased items on average").

### 2.2 The four classic contention points

| Hotspot | Why it's hot | Who hits it |
|---|---|---|
| **Warehouse `W_YTD`** | *Every* Payment updates one field on the single warehouse row → serializing point when threads > warehouses | Payment (all) + New-Order reads it ([DBx1000 §5.6.1](https://www.vldb.org/pvldb/vol8/p209-yu.pdf)) |
| **District `D_NEXT_O_ID`** | New-Order increments a monotonic counter; "next order ID is one more than max order ID" — a single-writer field per district | New-Order (45%) ([Murat Demirbas blog](http://muratbuffalo.blogspot.com/2023/01/tpc-e-vs-tpc-c-characterizing-new-tpc-e.html)) |
| **Stock `S_QUANTITY`** | ~10 stock rows updated per New-Order; same (warehouse,item) repeatedly hit | New-Order (45%) |
| **Customer balance / index** | Payment writes customer; inserts into Order/New-Order/Order-Line contend on the *insert-side index leaf* | Payment + New-Order inserts ([ResearchGate index-contention figure](https://www.researchgate.net/figure/Illustration-of-index-contention-on-the-TPC-C-table-An-insert-to-one-district-in-a_fig5_357766976)) |

DBx1000's controlled experiments isolate these precisely:
- **With 4 warehouses (< cores): every CC scheme fails to scale.** "No scheme
  scales for the Payment transaction … every Payment updates a single field in the
  warehouse (`W_YTD`) … updating the warehouse table becomes a bottleneck"
  ([DBx1000 §5.6.1](https://www.vldb.org/pvldb/vol8/p209-yu.pdf)).
- **With 1024 warehouses (≥ cores): the bottleneck shifts to New-Order**, but
  *not* from data contention — from the **overhead of maintaining locks/latches
  even on uncontended reads**: "the main bottleneck is the overhead of maintaining
  locks and latches, which occurs even if there is no contention … each [ITEM] read
  creates a shared-lock entry … the lock meta-data becomes large" ([DBx1000 §5.6.2](https://www.vldb.org/pvldb/vol8/p209-yu.pdf)).
- T/O schemes hit a **timestamp-allocation ceiling of ~10 million txn/s** at high
  core counts; H-STORE (partitioning) is "the best overall … with ~12%
  multi-partition transactions" ([DBx1000 §5.6.2](https://www.vldb.org/pvldb/vol8/p209-yu.pdf)).

### 2.3 Where the time actually goes (latency breakdown)

Tanabe et al. profiled the New-Order hot path with `perf` (Masstree index, 224
threads, 224 warehouses):

> "**68.4%** of New-Order execution time and **58.9%** of Payment time was spent
> on search, update, and insert [index traversals]. Because the size of the
> Masstree node was a few hundred bytes, its traversal decreased the spatial
> locality of the memory accesses; thus, the cache miss tended to increase."
> ([Tanabe/CCBench §A.3](https://vldb.org/pvldb/vol13/p3531-tanabe.pdf))

In other words, **~⅔ of a New-Order is index traversal and cache misses**, not
concurrency control or logging. The "Opportunities for Optimism" study adds that
validation/commit-time work "took close to **30% of the total run time** of
DBx1000's Silo TPC-C under high contention" ([Wu et al., VLDB 2020](https://dl.acm.org/doi/pdf/10.14778/3377369.3377373)).

### 2.4 What does not scale, and why

DBx1000's Table 2 summarizes the failure modes ([DBx1000 §6.1](https://www.vldb.org/pvldb/vol8/p209-yu.pdf)):

- **2PL (DL_DETECT):** scales under low contention; **lock thrashing** kills it
  under skew.
- **2PL (NO_WAIT):** no centralized contention point, highly scalable *but* "very
  high abort rate."
- **2PL (WAIT_DIE):** thrashing **and** timestamp bottleneck.
- **TIMESTAMP / MVCC:** non-blocking writes/reads help, but **timestamp
  allocation** is the ceiling; MVCC also generates extra memory traffic per read.
- **OCC:** "high overhead for copying data locally; high abort cost; timestamp
  bottleneck."
- **H-STORE (partitioning):** "the best algorithm for partitioned workloads" —
  suffers only from multi-partition txns and timestamp allocation.

---

## 3. Concurrency-control comparison

### 3.1 Head-to-head throughput on the same hardware

| CC protocol | Engine | Hardware | TPC-C throughput | Per-core | Abort rate (high-conf) | Source |
|---|---|---|---|---|---|---|
| **2PL (DL_DETECT)** | DBx1000 | 1024-core sim | fails to scale; thrash | — | moderate | [DBx1000](https://www.vldb.org/pvldb/vol8/p209-yu.pdf) |
| **2PL (NO_WAIT)** | DBx1000 | 1024-core sim | best 2PL variant | — | **very high** | [DBx1000](https://www.vldb.org/pvldb/vol8/p209-yu.pdf) |
| **MVCC** | DBx1000 | 1024-core sim | ~10M txn/s ceiling | — | low | [DBx1000](https://www.vldb.org/pvldb/vol8/p209-yu.pdf); "MVCC is significantly slower than OCC and pessimistic CC" ([Full story of 1000 cores](https://publikationen.reutlingen-university.de/files/3712/3712.pdf)) |
| **OCC (Silo)** | Silo | 32-core | **~700,000 tps** | **~22,000 tps/core** (91% scaling) | low (uncontended) | [Silo, SOSP'13](https://sigops.org/s/conferences/sosp/2013/papers/p18-tu.pdf) |
| OCC (Silo) | DBx1000/CCBench | 224-thread, 224-wh | ~1.6 M tps (full-mix) | ~7–14 k tps/core | rises when wh < threads | [Tanabe](https://vldb.org/pvldb/vol13/p3531-tanabe.pdf) |
| **OCC+MV (Cicada)** | Cicada | 28-core | **2.07 M tps** | ~74 k tps/core | low (multi-version) | [Cicada, SIGMOD'17](https://hyeontaek.com/papers/cicada-sigmod2017.pdf) |
| **OCC+Silo-commit (FOEDUS)** | FOEDUS | 240-core (DragonHawk) | **13,897 kTPS** (13.9 M tps) | **57.9 kTPS/core** | very low (Master-Tree) | [FOEDUS, SIGMOD'15](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf) |
| OCC (Silo) | FOEDUS comparison | 240-core | 5,757 kTPS | 24.0 kTPS/core | — | [FOEDUS Table 2](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf) |
| Partition (H-STORE/VoltDB) | H-Store | 240-core | 35.2 kTPS | 0.15 kTPS/core | n/a (locks) | [FOEDUS Table 2](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf) |
| **MVCC (Hekaton)** | SQL Server | prod | "up to 30–40×" vs disk | — | low | [MS In-Memory OLTP internals](https://learn.microsoft.com/en-us/sql/relational-databases/in-memory-oltp/sql-server-in-memory-oltp-internals-download?view=sql-server-ver17); [Hekaton, SIGMOD'13](https://dl.acm.org/doi/pdf/10.1145/2463676.2463710) |
| **Deterministic (Calvin)** | Calvin | cluster | near-linear on partitionable | — | none (no aborts from CC) | [Calvin, SIGMOD'12](https://cs.yale.edu/homes/thomson/publications/calvin-sigmod12.pdf) |

> Note on units: Silo/Cicada/FOEDUS report **all-transaction tps** (New-Order +
> Payment + …). tpmC ≈ tps × 60 × 0.45 *(derived)*, so Silo's 700 k tps ≈ **18.9 M
> tpmC-equivalent**, FOEDUS's 13.9 M tps ≈ **376 M tpmC-equivalent** *(derived)*.
> These are research prototypes, not audited TPC-C runs.

### 3.2 Per-transaction latency

- **Silo:** epoch-based group commit; "no scalability bottlenecks or much
  additional latency"; per-core 22 k tps ⇒ ~45 µs/txn amortized *(derived)*
  ([Silo](https://sigops.org/s/conferences/sosp/2013/papers/p18-tu.pdf)).
- **FOEDUS:** "Whether FOEDUS writes out logs to tmpfs or NVRAM, it only issues a
  few large sequential writes for each epoch, thus the [5 µs NVRAM] latency has
  almost no impact" ([FOEDUS §9](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf)).
- **Calvin:** transactions hold locks for the *full duration* of the agreement
  protocol in classical 2PC; Calvin's deterministic pre-ordering "eliminates"
  holding locks during agreement, but adds **sequencer latency** (a batching
  round) before execution ([Calvin §1.1](https://cs.yale.edu/homes/thomson/publications/calvin-sigmod12.pdf)).
- **Hekaton:** "generating a log record only at transaction commit time is
  possible" — single log write per commit ([Hekaton](https://dl.acm.org/doi/pdf/10.1145/2463676.2463710); [SO discussion](https://stackoverflow.com/questions/71067533/hekaton-and-durability)).
- **Group-commit floor:** classical group-commit only reaches its best commit
  latency at **100K–500K tps** for TPC-C; below that, latency rises
  ([Autonomous Commit, TUM 2025](https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/latency.pdf)).

### 3.3 Production users

| System | CC algorithm | Production user |
|---|---|---|
| Hekaton / In-Memory OLTP | MVCC (lock-free hash index) + OCC validation | **Microsoft SQL Server / Azure SQL** ([Hekaton](https://dl.acm.org/doi/pdf/10.1145/2463676.2463710)) |
| Silo-style epochs | OCC + epoch group commit | research lineage (Peloton, others adopt the commit protocol) ([Silo](https://sigops.org/s/conferences/sosp/2013/papers/p18-tu.pdf)) |
| Calvin | deterministic ordering + deterministic locking | **FaunaDB** (commercial Calvin derivative) ([Calvin blog](http://muratbuffalo.blogspot.com/2022/04/calvin-fast-distributed-transactions.html)) |
| FOEDUS | OCC (Silo-commit) + dual DRAM/NVRAM pages | **NTT Data** (research → internal) ([FOEDUS](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf)) |
| Cicada | optimistic multi-version | research (KAIST); ideas influenced later in-memory engines ([Cicada](https://hyeontaek.com/papers/cicada-sigmod2017.pdf)) |
| H-STORE / VoltDB | partition + single-threaded execution | **VoltDB** (commercial) ([DBx1000](https://www.vldb.org/pvldb/vol8/p209-yu.pdf)) |
| OceanBase | Paxos + 2PC, per-partition OCC/MVCC | **Ant Group / Alibaba** (Alipay) ([OceanBase VLDB'22](https://vldb.org/pvldb/vol15/p3385-xu.pdf)) |
| PolarDB | disaggregated 3-layer + 2PC | **Alibaba Cloud** ([PolarDB VLDB'25](https://www.vldb.org/pvldb/vol18/p5059-chen.pdf)) |

---

## 4. TPC-C world records 2010–2025

Records "increased over time almost exactly according to Moore's Law" until the
cloud era ([Wikipedia: TPC-C](https://en.wikipedia.org/wiki/TPC-C)).

| Year | Vendor / System | tpmC | $/tpmC | Hardware / scale | CC / architecture | Limiting bottleneck | Source |
|---|---|---|---|---|---|---|---|
| 1992 | IBM AS/400 | 54 | — | first published result | classical 2PL + WAL | single-CPU | [Wikipedia](https://en.wikipedia.org/wiki/TPC-C) |
| 1998-01 | — | 52,871 | — | — | — | — | [Wikipedia](https://en.wikipedia.org/wiki/TPC-C) |
| ~2000s | high-end | ~2.4 M | — | — | — | — | [Wikipedia](https://en.wikipedia.org/wiki/TPC-C) |
| 2011 | **Oracle DB 11g R2** | **~30 M** (SPARC Supercluster, SPARC T3-4) | — | large SMP/cluster | 2PL + WAL on flash | lock contention + log fsync | [DBTA](https://www.dbta.com/Editorial/News-Flashes/Oracle-Announces-World-Record-TPC-C-Benchmark-with-Oracle-DB-on-a-SPARC-Supercluster-with-SPARC-T3-T4-Servers-72719.aspx); [flashdba](https://flashdba.com/2012/09/28/oracle-achieves-record-tpc-c-benchmark) |
| 2013 | Oracle | ~8 M | — | single workstation | 2PL + WAL | single-node log/fsync | [Wikipedia](https://en.wikipedia.org/wiki/TPC-C) |
| 2019-08 | **OceanBase** (Ant) | **60.88 M** | — | distributed cluster | Paxos + 2PC, partitioned | first *distributed* DB record; cross-partition 2PC | [Alibaba Cloud blog](https://www.alibabacloud.com/blog/oceanbase-did-better-than-any-other-database-in-the-tpc-c-benchmark_595536); [OceanBase docs](https://en.oceanbase.com/docs/common-oceanbase-database-10000000000907461) |
| 2020-05 | **OceanBase** | **707 M** | **¥3.98/tpmC** | large cluster | Paxos-2PC ("OceanBase 2PC"), 1-Paxos commit | per-txn Paxos log round-trip | [OceanBase VLDB'22](https://vldb.org/pvldb/vol15/p3385-xu.pdf); [VLDB blog](https://oceanbase.medium.com/a-vldb-2022-paper-the-technologies-behind-oceanbases-707-million-tpmc-in-tpc-c-benchmark-test-ddcf18bc8481) |
| 2025-01 | **Alibaba PolarDB** | **2,055 M (2.055 B)** | **¥0.8/tpmC (~$0.11)** | **2,340 nodes**, 8-hr stress test, ≤0.16% jitter | 3-layer decoupling (compute/memory/storage) + 2PC, RDMA shared storage | shared-memory scale-out ceiling; per-node B-tree & txn-mgr | [PolarDB VLDB'25](https://www.vldb.org/pvldb/vol18/p5059-chen.pdf); [Alibaba blog](https://www.alibabacloud.com/blog/alibaba-clouds-polardb-breaks-tpc-c-benchmark-world-record-with-innovative-three-layer-decoupling-architecture_602021); [TPC FDR](https://www.tpc.org/results/fdr/tpcc/alibaba~tpcc~alibaba_cloud_polardb_limitless~fdr~2025-01-27~v01.pdf) |
| 2026 (unofficial) | RegattaDB | **750 k+ tps** (≈ 20 M tpmC-equiv *(derived)*) | — | 1.5 M warehouses, 50 GCP nodes | distributed, NVMe-local | RAM capacity (working set > RAM) | [RegattaDB blog](https://regatta.dev/blog/regattadb-tpc-c-benchmark-750k-tps-1-5m-warehouses) |

### 4.1 What the record progression shows

- **2010–2013 (single-node era):** records in the **millions** of tpmC, dominated
  by Oracle on big SMP/SPARC iron; the ceiling was **lock contention + WAL
  fsync** on a single node ([Wikipedia](https://en.wikipedia.org/wiki/TPC-C); [DBTA](https://www.dbta.com/Editorial/News-Flashes/Oracle-Announces-World-Record-TPC-C-Benchmark-with-Oracle-DB-on-a-SPARC-Supercluster-with-SPARC-T3-T4-Servers-72719.aspx)).
- **2019–2020 (distributed era):** OceanBase jumped to **707 M** by sharding
  across hundreds of nodes with Paxos-replicated partitions and a 1-Paxos commit
  optimization ("reduces the number of Paxos … to only one Paxos synchronization")
  ([OceanBase](https://vldb.org/pvldb/vol15/p3385-xu.pdf)). Cost fell to ¥3.98/tpmC.
- **2025 (cloud-disaggregated era):** PolarDB hit **2.055 B tpmC — 2.5× the
  previous record — at 37–79.5% lower $/tpmC**, by going to **2,340 nodes** with
  a 3-layer decoupled architecture (compute / shared-memory / shared-storage)
  ([PolarDB](https://www.vldb.org/pvldb/vol18/p5059-chen.pdf)). The explicit
  limitation they call out: shared-memory multi-primary "is fundamentally
  constrained by the physical limitations of disaggregated memory, making it
  difficult to scale beyond a few hundred nodes" — so they fell back to
  **scale-out** for well-partitioned TPC-C ([PolarDB §2](https://www.vldb.org/pvldb/vol18/p5059-chen.pdf)).

**Per-warehouse efficiency at the top:** PolarDB's 2.055 B tpmC divided by its
warehouse count lands near the **12.86 tpmC/warehouse** spec ceiling (RegattaDB
explicitly reports **12.6 tpmC/warehouse = 98% of theoretical**
([RegattaDB](https://regatta.dev/blog/regattadb-tpc-c-benchmark-750k-tps-1-5m-warehouses))).
Modern record-holders win by **adding warehouses (and nodes), not by exceeding the
per-warehouse rate**.

---

## 5. The math

### 5.1 Rows touched per New-Order (average)

| Operation | Object | Count (avg) |
|---|---|---|
| Read | Warehouse | 1 |
| Read+Write | District (`D_NEXT_O_ID++`) | 1 |
| Read | Customer | 1 |
| Read | Item (read-only) | 10 |
| Read+Write | Stock | 10 |
| Insert | Order | 1 |
| Insert | New-Order | 1 |
| Insert | Order-Line | 10 |
| **Total** | | **23 read + 11 write + 12 insert ≈ 46 rows** |

Consistent with the spec ("an order for on average 10 items … inserts the order,
and for each item updates the corresponding stock level"
([SIGMOD Record TPC-C synopsis](https://sigmodrecord.org/?smd_process_download=1&download_id=9397)))
and ROCOCO ("updates a highly contended order-id data field as well as 10
purchased items" ([ROCOCO](http://mpaxos.com/pub/rococo-osdi14.pdf))). The TPC-C
mix ratio path-length factor X (total txns per New-Order) was measured at
**4.87–9.53** in early results ([Jim Gray](https://jimgray.azurewebsites.net/BenchmarkHandbook/chapter12.pdf)).

### 5.2 Working set per warehouse

The per-warehouse populated working set is dominated by **Stock (100 K rows) and
Customer (30 K rows)**:

- Virtuoso's analysis estimates the **per-warehouse working set ≈ 66 MB** (counting
  ~8 MB for the ~40 insert/update hot points) ([Virtuoso scalability WP](https://virtuoso.openlinksw.com/whitepapers/Virtuoso%20and%20Database%20Scalability.html)).
- A TPC-C model with 8 warehouses ≈ **3.0 GB on disk, 800 MB in-memory** *(≈100 MB
  on-disk / ≈100 MB /warehouse)* ([Barroso TPC-B vs TPC-C](https://barroso.org/publications/caecw2k.pdf)).
- Practical guidance: **250–500 warehouses per server CPU socket** as a starting
  point for TPROC-C ([HammerDB](https://www.hammerdb.com/docs/ch03s07.html)).

### 5.3 Theoretical max tpmC per core

Assume a single New-Order costs **L** microseconds end-to-end on one core (the
floor set by the slowest sequential stage). Then:

> tpmC/core ≈ 60 × 10⁶ / L  *(derived; New-Order-bound, no contention)*

| Scenario | L (per New-Order) | tpmC/core *(derived)* | What sets L |
|---|---|---|---|
| **Non-durable in-memory** | **1 µs** | **60 M** | pure instruction + L1/L2 cost |
| **In-memory + durable-ish** (group-commit, NVRAM/epoch) | **5 µs** | **12 M** | epoch flush ≈ 5 µs ([FOEDUS NVRAM log](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf)) |
| **NVMe WAL, no group commit** (fsync per txn) | **100 µs** | **600 K** | single NVMe fsync ≈ 10–100 µs ([simplyblock](https://simplyblock.io/glossary/nvme-latency); [cedardb](https://cedardb.com/blog/ssd_latency)) |

Calibration against reality:
- **Silo: ~22,000 tps/core ⇒ ~13.2 M tpmC/core-equiv** *(derived)* — sits right
  at the **5 µs** line ([Silo](https://sigops.org/s/conferences/sosp/2013/papers/p18-tu.pdf)).
- **FOEDUS: 57.9 kTPS/core ⇒ ~34.7 M tpmC/core-equiv** *(derived)* — below the
  5 µs floor because its epoch batches many txns per NVRAM write
  ([FOEDUS Table 2](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf)).
- **Cicada: 2.07 M tps / 28 cores ≈ 74 k tps/core ⇒ ~44.4 M tpmC/core-equiv**
  *(derived)* — best published, benefiting from multi-version read-mostly
  execution ([Cicada](https://hyeontaek.com/papers/cicada-sigmod2017.pdf)).
- A **commercial main-memory DB** of the Silo era: "at most **3,000 tps/core**"
  (~1.8 M tpmC/core-equiv) — i.e., ~330 µs/txn, dominated by overheads
  ([Silo](https://sigops.org/s/conferences/sosp/2013/papers/p18-tu.pdf)).
- **Per-warehouse ceiling:** the spec caps each warehouse at 12.86 tpmC, so to
  express a core at its theoretical tpmC you need ≥ tpmC/12.86 warehouses
  *(derived)* — e.g., 12 M tpmC/core ⇒ ≥ 933 K warehouses/core of scale.

---

## 6. Logging

### 6.1 One WAL write per New-Order commit

TPC-C durability requires a redo-log (WAL) record at commit. Hekaton-style
designs "generat[e] a log record only at transaction commit time" — i.e.,
**one log write per committed New-Order** ([Hekaton](https://dl.acm.org/doi/pdf/10.1145/2463676.2463710);
[SO](https://stackoverflow.com/questions/71067533/hekaton-and-durability)). The
log record captures the ~11 row updates + 12 inserts of the New-Order.

### 6.2 Group commit on NVMe

A single NVMe `fsync` is the long pole:

| Device | Write/fsync latency | Source |
|---|---|---|
| Enterprise NVMe (PCIe 4/5) | **~10–30 µs** | [cpu_energy_kb §3.1](./cpu_energy_kb.md); [simplyblock](https://simplyblock.io/glossary/nvme-latency) |
| Enterprise SSD (power-loss-protected) | **~10–50 µs**, "very consistent" | [cedardb](https://cedardb.com/blog/ssd_latency); [HN](https://news.ycombinator.com/item?id=46532675) |
| Consumer NVMe | 50–200 µs+ | [smalldatum](http://smalldatum.blogspot.com/2026/01/ssds-power-loss-protection-and-fsync.html) |
| Kioxia XL-FLASH (SCM) | ~8 µs write / ~29 µs read | [cpu_energy_kb §2.1](./cpu_energy_kb.md) |
| Optane DC PMEM (read) | **346 ns** (~3× DRAM) | [NVSL](https://www.nvsl.io/data/bib/pdfs/2019arXiv-AEP.pdf) |

**Group commit** batches N transactions' log records into one fsync, so
effective per-txn log cost ≈ fsync_latency / N. With N = 1,000, a 20 µs fsync
becomes **20 ns/txn** — negligible. FOEDUS exploits exactly this: "it only issues
a few large sequential writes for each epoch, thus the [5 µs NVRAM] latency has
almost no impact" ([FOEDUS](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf)).
Silo's epoch-based group commit is the same idea ([Silo](https://sigops.org/s/conferences/sosp/2013/papers/p18-tu.pdf)).

**The catch:** classical group commit only hits its best *latency* at high
throughput — "these competitors only achieve their best commit latency when the
throughput is sufficiently high — 100K and 500K transactions per second for YCSB
and TPC-C" ([Autonomous Commit, TUM 2025](https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/latency.pdf)).
Below that, you pay the full fsync. The TUM "Autonomous Commit" proposal replaces
group commit with per-txn durable commits on NVMe, lowering 99p latency "78%"
below the best group-commit competitors on large TPC-C txns
([TUM 2025](https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/latency.pdf)).

### 6.3 Could CXL.mem host the log?

CXL.mem (Type-3) is cache-coherent memory expansion over PCIe. Latency and
bandwidth ([cpu_energy_kb §2.1, §2.5](./cpu_energy_kb.md); [Das Sharma, CXL intro 2024](https://dl.acm.org/doi/full/10.1145/3669900)):

| Path | Latency | Bandwidth |
|---|---|---|
| Local DDR5 | ~80–100 ns | ~460 GB/s/socket (DDR5-6400) |
| CXL.mem (best) | **~140 ns** ("57 ns end-to-end adder") | 64 GB/s per x16 PCIe 5.0 |
| CXL.mem (typical) | ~250 ns | — |
| CXL.mem (contended) | ~350–520 ns | shared |
| CXL 3.0 x16 (PCIe 6.0) | — | **128 GB/s** |

**As a log buffer — yes, plausibly:**
- A CXL-attached, **battery/capacitor-backed DRAM** (or future CXL-attached NVM)
  can serve as a durable-ish commit target at **~140–250 ns** instead of NVMe's
  ~10–30 µs — a **~50–200× latency reduction** per commit *(derived)*.
- This collapses the group-commit trade-off: with per-txn commit at ~200 ns you
  get low latency *without* needing 100K+ tps to amortize
  (cf. [Autonomous Commit](https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/latency.pdf)).
- CXL 2.0 switching lets **multiple nodes share one CXL-attached log device**
  (memory pooling), enabling a cluster-wide durable log without per-node NVMe
  ([Das Sharma 2024](https://dl.acm.org/doi/full/10.1145/3669900); [Pond, ASPLOS'23](https://www.microsoft.com/en-us/research/wp-content/uploads/2022/10/2023_Pond_asplos23_official_asplos_version.pdf)).

**Caveats:**
- CXL.mem is **volatile** unless paired with persistent media — "no commercial
  byte-addressable persistent memory ships today" post-Optane
  ([cpu_energy_kb §3.6](./cpu_energy_kb.md)). A CXL log buffer still needs a
  shadow flush to NVMe/ZNS for true durability, or a capacitor-backed DIMM.
- Under load CXL latency balloons to **350–520 ns** ([cpu_energy_kb §2.1](./cpu_energy_kb.md)),
  so the log device must be provisioned for bandwidth, not contended.
- CXL is "dead in the AI era" for HBM-near-compute, but "its growth is in memory
  expansion for general-purpose DB workloads" ([cpu_energy_kb §2.5](./cpu_energy_kb.md))
  — i.e., exactly the OLTP-log use case.

---

## 7. Partitioning

### 7.1 Why TPC-C scales on warehouse_id

"TPC-C is very easily shardable on the warehouse_id to hide any contention on
concurrency control" ([Murat Demirbas](http://muratbuffalo.blogspot.com/2023/01/tpc-e-vs-tpc-c-characterizing-new-tpc-e.html)).
The spec deliberately builds in **low cross-partition rate**:

- Only **~10% of New-Order** and **~15% of Payment** transactions touch a remote
  warehouse ("only 10% of the New-Order transactions and 15% of the Payment
  transactions involve a remote warehouse" ([SIGMOD Record synopsis](https://sigmodrecord.org/?smd_process_download=1&download_id=9397))).
- DBx1000 measures the full mix at **~12% multi-partition transactions**
  ([DBx1000 §5.6.2](https://www.vldb.org/pvldb/vol8/p209-yu.pdf)).

Because the cross-partition rate is ~10–12%, **partition-per-warehouse execution
(H-STORE / VoltDB style) wins**: "H-STORE outperforms other approaches when less
than 20% [of the] workload comprises multi-partition transactions"
([DBx1000 §6](https://www.vldb.org/pvldb/vol8/p209-yu.pdf)). Each partition runs
single-threaded, so **no locks, no latches, no aborts** within a partition — the
~88% single-partition txns become nearly free. PolarDB explicitly exploits this:
"for well-partitioned workloads like TPC-C, where cross-partition contention is
minimal," scale-out to thousands of nodes works ([PolarDB §2](https://www.vldb.org/pvldb/vol18/p5059-chen.pdf)).

### 7.2 How Calvin exploits it

Calvin's three-layer design ([Calvin, SIGMOD'12](https://cs.yale.edu/homes/thomson/publications/calvin-sigmod12.pdf);
[Adi's blog](http://muratbuffalo.blogspot.com/2022/04/calvin-fast-distributed-transactions.html)):

1. **Sequencer layer:** deterministically orders transaction *inputs* into a
   global replicated log (via Paxos). "Calvin predetermines a global order in
   which transactions should commit" ([paper summary](https://mwhittaker.github.io/papers/html/thomson2012calvin.html)).
2. **Scheduler layer:** each partition applies a **deterministic lock** protocol
   on the pre-ordered transactions — since the order is fixed, replicas never
   diverge and **no distributed deadlock or 2PC agreement is needed for
   isolation** ([Calvin §1.1](https://cs.yale.edu/homes/thomson/publications/calvin-sigmod12.pdf)).
3. **Execution layer:** runs the transaction against local storage.

The key wins for TPC-C:
- **Locks are not held during the agreement protocol.** Classical 2PC "holds
  locks for the full duration of this agreement protocol … two-phase commit
  requires multiple network round-trips" — Calvin sidesteps this entirely by
  pre-ordering, so "the total duration that a transaction holds its locks" drops
  to just local execution ([Calvin §1.1](https://cs.yale.edu/homes/thomson/publications/calvin-sigmod12.pdf)).
- **Replication replicates inputs, not effects** — "by replicating transaction
  inputs rather than effects, Calvin is also able to support multiple consistency
  levels … at no cost to transactional throughput" ([Calvin abstract](https://cs.yale.edu/homes/thomson/publications/calvin-sigmod12.pdf)).
- For the ~88% single-partition txns, Calvin is **pure local execution** with no
  cross-partition coordination; the ~12% multi-partition txns run under
  deterministic locks in the pre-agreed order, so they **never abort** due to CC.

This is why Calvin "scales near-linearly" on partitionable workloads like TPC-C
([Calvin abstract](https://cs.yale.edu/homes/thomson/publications/calvin-sigmod12.pdf)).

### 7.3 Cross-partition is the tax

The flip side: H-STORE/Calvin pay a steep tax on multi-partition txns. FOEDUS
shows H-Store "triggers distributed transactions with global locks … more than
**90% of transactions in H-Store abort with higher remote ratios**," while
lightweight-OCC engines (FOEDUS/Silo) "suffer from only modest slowdowns"
([FOEDUS §9](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf)).
At the regular TPC-C remote ratio (=1, ~10% remote), FOEDUS is **400× faster than
H-Store** on 240 cores ([FOEDUS §9](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf)).

---

## 8. Instruction-level analysis of the New-Order hot loop

The New-Order inner loop is: **hash-probe → B-tree traversal → lock/CAS → log
write → commit**. Below is the cheapest AVX-512 sequence and energy per stage,
using numbers from the in-repo energy knowledgebase
([cpu_energy_kb §1–§2](./cpu_energy_kb.md)) and the latency hierarchy.

### 8.1 Latency & energy reference (Sapphire Rapids / Zen 4-5)

| Tier / op | Latency | Energy | Source |
|---|---|---|---|
| L1 hit (8-byte load) | ~4–5 cyc (~1 ns) | ~50–100 pJ | [cpu_energy_kb §1.7, §2.1](./cpu_energy_kb.md); [SO](https://stackoverflow.com/questions/10274355/cycles-cost-for-l1-cache-hit-vs-register-on-x86); [nexthink](https://nexthink.com/blog/smarter-cpu-testing-kaby-lake-haswell-memory) |
| L2 hit | ~12 cyc (~3–4 ns) | ~200–500 pJ | [cpu_energy_kb §2.1](./cpu_energy_kb.md) |
| L3 hit | ~40 cyc (~10–20 ns) | ~1–2 nJ | [cpu_energy_kb §2.1](./cpu_energy_kb.md); [SPEC ICPE'22](https://research.spec.org/icpe_proceedings/2022/proceedings/p165.pdf) |
| Local DDR5 | ~80–100 ns | ~1.8–2.4 nJ/64B | [cpu_energy_kb §2.1, §2.3](./cpu_energy_kb.md) |
| Cross-socket DRAM | ~150–250 ns | ~2–4× local | [cpu_energy_kb §5.5](./cpu_energy_kb.md); [AMD NUMA WP](https://www.amd.com/content/dam/amd/en/documents/epyc-business-docs/white-papers/AMD-Optimizes-EPYC-Memory-With-NUMA.pdf) |
| CXL.mem | ~140–520 ns | local + ~5–10 nJ link | [cpu_energy_kb §2.5](./cpu_energy_kb.md); [Das Sharma 2024](https://dl.acm.org/doi/full/10.1145/3669900) |
| NVMe fsync | ~10–30 µs | ~1–5 µJ/64B | [cpu_energy_kb §2.3, §3.1](./cpu_energy_kb.md); [simplyblock](https://simplyblock.io/glossary/nvme-latency) |

### 8.2 Per-stage cheapest AVX-512 sequence

**Stage A — Hash probe (open-addressed, 16-way bucketed).** For a 16-slot bucket
aligned to a cache line, compare a 64-bit fingerprint against 16 keys:
```
vmovdqu64 zmm0, [bucket]        ; load 16 × 4-byte fingerprints (or 8 × 8-byte)
vpcmpeqq  k1, zmm0, zmm_key     ; AVX-512 masked compare → mask k1   (1 cyc, ~0.4 nJ)
kortestw  k1, k1                ; any hit?
jz        .miss                 ; (branchless preferred: vmovdqu64 + vpternlogq blend)
```
- **VPCMPEQQ** does 8 int64 compares/instr (YMM) or 16 (ZMM with masking) at
  **1-cycle latency, 0.5 TP, ~0.4 nJ** ([cpu_energy_kb §1.5](./cpu_energy_kb.md)).
- AVX-512 masked compare (`VPCMPM`) avoids the `VPMOVMSKB` vector→int domain
  crossing that "is a common DB bottleneck" ([cpu_energy_kb §1.5](./cpu_energy_kb.md)).
- L1-resident probe: **~1–2 cycles + ~0.5 nJ**; on L1 miss the load dominates at
  ~4–5 cyc + 50–100 pJ. Sapphire Rapids can do **two 512-bit loads/cycle from
  L1** ([Chips and Cheese](https://chipsandcheese.com/p/a-peek-at-sapphire-rapids); [Intel ISA forum](https://community.intel.com/t5/Intel-ISA-Extensions/Early-indicators-of-AVX512-performance-on-Skylake/m-p/1028172)).
- **Avoid:** scalar `IDIV` for modulo (16–35 cyc, ~1.5–4 nJ) — use
  magic-multiply or `PEXT` (3 cyc on Zen 3+/Intel, ~0.4 nJ) ([cpu_energy_kb §1.2, §7.2](./cpu_energy_kb.md)).

**Stage B — B-tree / Masstree traversal.** This is the New-Order killer:
**68.4% of New-Order time** is here ([Tanabe](https://vldb.org/pvldb/vol13/p3531-tanabe.pdf)).
Each Masstree node is "a few hundred bytes" → **L2/L3 miss per level** (~10–20 ns
each, ~1–2 nJ each). Cheapest inner-node search:
```
vpcmpeqq k1, zmm_keys, zmm_search_key   ; 16-way compare of node keys
vpternlogq zmm_idx, zmm_tmp, zmm_mask   ; fold mask → child index (1 instr, ~0.4 nJ)
```
- **VPTERNLOGQ** "is the DB secret weapon — folds any 3-input bitwise truth table
  in 1 instr" ([cpu_energy_kb §1.6](./cpu_energy_kb.md)).
- Real win is **flatening the tree**: replace pointer-chase with a hash index or
  a single-level radix tree so the whole lookup is 1–2 cache lines. Tanabe notes
  CCBench with hash indexes is **~30% faster (39.6 Mtps) than DBx1000 with
  Masstree (30 Mtps)** at 80 threads ([Tanabe §5.2](https://vldb.org/pvldb/vol13/p3531-tanabe.pdf)).

**Stage C — Lock / CAS (commit validation).** Acquire write-locks on the ~11
write-set rows (Silo/Hekaton-style, deferred to commit):
```
lock cmpxchg [record_lock], rax     ; uncontended ~17–25 cyc, ~2–5 nJ
```
- **LOCK CMPXCHG uncontended:** 17–25 cyc, ~2–5 nJ ([cpu_energy_kb §1.8](./cpu_energy_kb.md)).
  **Contended:** "100s–1000s of cycles, ~5–50 nJ" ([cpu_energy_kb §7.2](./cpu_energy_kb.md)).
- **Avoid split locks at all costs** — a CAS crossing a cache line is
  **3,000–10,000 cycles, ~50–200 nJ**, serializing the whole fabric
  ([cpu_energy_kb §1.8](./cpu_energy_kb.md)).
- DBx1000 found the centralized timestamp/lock-manager mutex "is the main
  bottleneck" at scale ([DBx1000 §4.3](https://www.vldb.org/pvldb/vol8/p209-yu.pdf)) —
  use **per-tuple** lock words and decentralized epoch timestamps (Silo) instead.

**Stage D — Log write (commit).** Append the redo record, then `sfence` +
(group-commit fsync):
```
rep movsb      [log_tail], [record]   ; ERMS memcpy ~1 B/cyc, ~0.05 nJ/B
sfence                              ; ~5–10 cyc, ~0.5–1.5 nJ (drain store buffer)
; ... group-commit barrier; one NVMe fsync per epoch ...
```
- The `rep movsb` of a ~200-byte redo record ≈ 200 cyc ≈ 67 ns + ~10 nJ *(derived)*.
- `SFENCE` is **~10× cheaper than MFENCE** ([cpu_energy_kb §1.8](./cpu_energy_kb.md)).
- The fsync is **10–30 µs** but amortized over an epoch of N txns
  ([FOEDUS](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf)).

**Stage E — Commit / make-visible.** Bump epoch, publish versions:
- `LOCK XADD` on the global epoch counter: ~9–18 cyc, ~1.5–3.5 nJ *uncontended*
  ([cpu_energy_kb §1.8](./cpu_energy_kb.md)) — but this is the exact
  "contentious atomic CAS" FOEDUS warns "even one … becomes a bottleneck" on
  hundreds of cores ([FOEDUS §3](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf)).
  Silo's fix: **per-thread epoch batching** so the global counter is touched
  once per epoch, not once per txn ([Silo](https://sigops.org/s/conferences/sosp/2013/papers/p18-tu.pdf)).

### 8.3 Energy budget for one in-memory New-Order *(derived, L2-resident)*

| Stage | Ops | Energy (est.) |
|---|---|---|
| 10× hash probe (L1) | 10 × (load + vpcmpeqq) | ~10 × 0.5 nJ ≈ 5 nJ |
| 10× Masstree traversal (2 L3 misses each) | 20 × ~1.5 nJ | ~30 nJ |
| 11× write-lock CAS (uncontended) | 11 × ~3 nJ | ~33 nJ |
| 1× redo memcpy + sfence | ~200 B + sfence | ~12 nJ |
| 1× epoch XADD | 1 × ~2.5 nJ | ~2.5 nJ |
| **Total (excl. fsync, L2-resident)** | | **~80 nJ/txn** |

At ~80 nJ/txn and ~5 µs wall-clock (group-commit floor), one core at 3 GHz could
do ~200 K New-Orders/s ⇒ ~12 M tpmC/core *(derived)* — matching the §5.3 in-memory
line. **The fsync (1–5 µJ/64B) dwarfs everything** if not group-committed
([cpu_energy_kb §2.3](./cpu_energy_kb.md)).

### 8.4 The hot-loop anti-patterns to avoid

From [cpu_energy_kb §8.5](./cpu_energy_kb.md):
- **Branch mispredicts** in the inner loop = 15–21 cyc flush + ~2–4 nJ each → use
  branchless `CMOV`/mask blends.
- **Split locks** = 3,000–10,000 cyc → align all lock words to 8 bytes.
- **Variable shifts / IDIV** → magic-multiply.
- **`PEXT`/`PDEP` on Zen/Zen2** (microcode, ~18 cyc) → guard with CPUID, fine on
  Zen 3+/Intel.
- **`VPMOVMSKB`** domain crossing → prefer AVX-512 masked compares.

---

## 9. Can we beat TPC-C?

"Beating TPC-C" means either (a) higher **per-core** tpmC than the best research
engines, or (b) cheaper **$/tpmC** than PolarDB's ¥0.8, or (c) lower **per-txn
latency** than group-commit allows. Below are the levers, the theoretical
ceilings, and who is closest.

### 9.1 Five levers a custom engine could pull

| # | Lever | Mechanism | Theoretical gain | Risk |
|---|---|---|---|---|
| **1** | **Partition + deterministic ordering (Calvin-style)** | Pre-order txn inputs in a shared log; single-threaded partition execution; no locks/aborts for the ~88% single-partition txns | Near-linear scale-out; eliminates CC overhead entirely on hot path | Multi-partition (~12%) txns pay coordination; sequencer is a new bottleneck |
| **2** | **CXL.mem-attached durable log (or cap-backed DRAM)** | Commit to ~140–250 ns CXL memory instead of 10–30 µs NVMe | ~50–200× lower commit latency; enables autonomous (non-group) commit at low throughput | CXL is volatile without persistent backing; contended CXL → 500 ns |
| **3** | **Instruction-first index: AVX-512 hash / 1-level radix** | Replace Masstree (68% of New-Order time) with a flat hash index; VPCMPEQQ+VPTERNLOGQ probes | ~30%+ per-core speedup (Tanabe: hash 39.6 Mtps vs Masstree 30 Mtps) | Lose range scans (Stock-Level, Order-Status need a secondary structure) |
| **4** | **Epoch-batched decentralized commit (Silo/FOEDUS)** | Per-thread epochs; one global touch per epoch; batch N txns per fsync | Decouples per-txn latency from fsync; removes centralized mutex | Adds epoch-grain latency (typically ~50 µs); GC complexity |
| **5** | **3-tier volatile + 2-tier persistent placement** | L3/LLC → local DDR5 → CXL.mem for working set; ZNS-NVMe for WAL, QLC for cold | Keeps hot set in ~100 MB/warehouse in DRAM, spills Stock/History to CXL | Placement telemetry overhead; CXL tail-latency jitter |

### 9.2 Theoretical ceilings per socket / GB / NVMe BW

**Per socket (in-memory, durable-ish, 128 Zen-5-class cores):**
- Best published per-core: Cicada ~74 k tps/core, FOEDUS ~58 kTPS/core
  ([Cicada](https://hyeontaek.com/papers/cicada-sigmod2017.pdf); [FOEDUS](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf)).
- At the **5 µs** in-memory floor: 200 k tps/core ⇒ 12 M tpmC/core *(derived)* ⇒
  **~1.5 B tpmC/socket** for 128 cores *(derived)* — i.e., *one socket could in
  principle match PolarDB's 2,055-node cluster's headline number*.
- At the **1 µs** non-durable floor: 60 M tpmC/core ⇒ **~7.7 B tpmC/socket**
  *(derived)* — pure compute-bound fantasy, but it sets the upper envelope.

**Per GB of DRAM (working-set bound):**
- ~100 MB/warehouse working set ([Barroso](https://barroso.org/publications/caecw2k.pdf));
  12.86 tpmC/warehouse spec ceiling ⇒ **~128.6 tpmC/MB ≈ 128,600 tpmC/GB**
  *(derived)*. A 512 GB socket could host ~5,000 warehouses ⇒ ~64 M tpmC of spec-
  legal scale *(derived)* — so **memory capacity, not bandwidth, is the per-node
  ceiling** for legal TPC-C.

**Per NVMe BW (log-bound, no group commit):**
- PCIe-5 x4 NVMe ≈ 14 GB/s, fsync ~20 µs. If each New-Order redo ≈ 200 B and you
  must fsync each, throughput ≈ 1/20 µs = 50 k tps = **3 M tpmC/device** *(derived)*.
- With **group commit** saturating the 14 GB/s link: 14 GB/s ÷ 200 B ≈ 70 M
  txn/s ⇒ **~4.2 B tpmC per NVMe device** *(derived)* — the log device is *not*
  the ceiling once you batch; the per-warehouse spec is.

### 9.3 Who is closest today

| Goal | Closest system | Number | Gap to ceiling |
|---|---|---|---|
| Highest per-core tpmC-equiv | **Cicada** (28-core) | ~74 k tps/core ≈ 44 M tpmC/core-equiv | ~3.6× below 5 µs floor; ~27× below 1 µs floor *(derived)* |
| Highest per-core (durable) | **FOEDUS** (240-core) | 57.9 kTPS/core ≈ 35 M tpmC/core-equiv | ~2.5× below 5 µs floor *(derived)* |
| Highest absolute tpmC | **PolarDB** (2,340 nodes) | 2.055 B tpmC | ~per-warehouse-ceiling-bound; ~1 socket's theoretical ≈ this *(derived)* |
| Best $/tpmC | **PolarDB** | ¥0.8 (~$0.11)/tpmC | reference |
| Lowest commit latency | **Autonomous Commit** (TUM'25) | 99p 78% below group-commit on NVMe | beats group-commit; CXL-log could go further |

### 9.4 Where an instruction-first engine wins

1. **Killing the 68%.** The single biggest per-core win is replacing Masstree
   with an **AVX-512 flat hash index** for the (warehouse,item)→stock and
   (warehouse,district)→customer lookups. Tanabe measured a **30% throughput gain**
   just from hash-vs-tree ([Tanabe §5.2](https://vldb.org/pvldb/vol13/p3531-tanabe.pdf)).
   A purpose-built engine that lays out Stock rows **so the 10 probed rows land in
   1–2 cache lines** (bucketed by item-id hash) could push the probe stage from
   ~30 nJ to ~5 nJ (§8.3).

2. **Removing the centralized mutex.** Both DBx1000 and FOEDUS identify the
   **single contentious atomic** (timestamp allocator / epoch counter / lock
   manager) as the many-core ceiling ([DBx1000 §4.3](https://www.vldb.org/pvldb/vol8/p209-yu.pdf);
   [FOEDUS §3](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf)).
   An instruction-first engine uses **per-thread epoch batching + per-tuple lock
   words** so the only shared writes are cache-line-aligned and uncontended.

3. **Branchless, SIMD-validated commit.** The validation phase (compare ~46
   version numbers) is **~30% of TPC-C runtime under contention**
   ([Wu et al. 2020](https://dl.acm.org/doi/pdf/10.14778/3377369.3377373)). A
   `VPCMPEQQ`-batched version check over a packed write-set (16 versions/instr)
   turns it into a handful of vector compares — exactly the SIMD-amortization
   regime where AVX-512 is "essentially free per element"
   ([cpu_energy_kb §8.2](./cpu_energy_kb.md)).

4. **CXL-log + autonomous commit.** Pairing a **CXL-attached cap-backed DRAM
   log** with autonomous (per-txn) commit gets NVMe-class durability latency
   down from ~20 µs to ~200 ns — collapsing the group-commit floor and letting
   the engine hit low latency *at low throughput*, which group-commit engines
   cannot ([TUM 2025](https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/latency.pdf)).

5. **Deterministic partitioning for the 88%.** Run each warehouse partition
   single-threaded (H-STORE/Calvin-style) so the 88% single-partition txns pay
   **zero CC overhead**, and reserve lightweight OCC only for the ~12%
   multi-partition txns — the hybrid that DBx1000's discussion explicitly
   recommends ([DBx1000 §6.1](https://www.vldb.org/pvldb/vol8/p209-yu.pdf)).

### 9.5 The honest ceiling

A custom instruction-first engine on a single 128-core Zen-5 socket, with CXL-log
+ autonomous commit + AVX-512 hash indexes + partitioned deterministic execution,
could plausibly reach **~50–100 k tps/core ≈ 30–60 M tpmC/core-equiv** in-memory
*(derived)* — i.e., **~4–8 B tpmC per socket**. That is **2–4× PolarDB's
2,340-node cluster on one socket** *(derived)* — but only if the workload is
scaled with enough warehouses (~300 M+ warehouses for 8 B tpmC at the 12.86
ceiling). The real-world limiter is not instructions; it is the **12.86
tpmC/warehouse spec ceiling × available memory capacity**, plus the engineering
of a CXL-persistent log. **Beating TPC-C on $/tpmC** is therefore mostly a
*consolidation* story (one fat socket vs. 2,340 nodes), not a raw-speed story.

---

## Sources (consolidated)

**Spec & records:** [TPC-C v5.11.0](https://www.tpc.org/tpc_documents_current_versions/pdf/tpc-c_v5.11.0.pdf); [Jim Gray ch.12](https://jimgray.azurewebsites.net/BenchmarkHandbook/chapter12.pdf); [Wikipedia](https://en.wikipedia.org/wiki/TPC-C); [openGauss](https://docs.opengauss.org/en/docs/5.1.0/docs/DatabaseAdministrationGuide/mot-sample-tpc-c-benchmark.html); [HammerDB](https://www.hammerdb.com/docs3.3/ch03s05.html); [BenchmarkSQL](https://benchmarksql.readthedocs.io/en/latest/TPCC); [YDB](https://ydb.tech/docs/en/reference/ydb-cli/workload-tpcc); [Fujitsu](https://sp.ts.fujitsu.com/dmsp/Publications/public/Benchmark_Overview_TPC-C.pdf); [Barroso TPC-B vs C](https://barroso.org/publications/caecw2k.pdf).

**Concurrency-control papers:** [DBx1000 "Staring into the Abyss," VLDB'14](https://www.vldb.org/pvldb/vol8/p209-yu.pdf); [Silo, SOSP'13](https://sigops.org/s/conferences/sosp/2013/papers/p18-tu.pdf); [Hekaton, SIGMOD'13](https://dl.acm.org/doi/pdf/10.1145/2463676.2463710); [Calvin, SIGMOD'12](https://cs.yale.edu/homes/thomson/publications/calvin-sigmod12.pdf); [FOEDUS, SIGMOD'15](https://15721.courses.cs.cmu.edu/spring2016/papers/p691-kimura.pdf); [Cicada, SIGMOD'17](https://hyeontaek.com/papers/cicada-sigmod2017.pdf); [Tanabe/CCBench, VLDB'20](https://vldb.org/pvldb/vol13/p3531-tanabe.pdf); [Full story of 1000 cores, 2022](https://publikationen.reutlingen-university.de/files/3712/3712.pdf); [Wu "Opportunities for Optimism," VLDB'20](https://dl.acm.org/doi/pdf/10.14778/3377369.3377373); [ROCOCO, OSDI'14](http://mpaxos.com/pub/rococo-osdi14.pdf); [Neumann fast-serializable MVCC](https://15721.courses.cs.cmu.edu/spring2016/papers/p677-neumann.pdf); [TicToc, SIGMOD'16](https://people.csail.mit.edu/devadas/pubs/tictoc.pdf).

**Records & systems:** [OceanBase VLDB'22](https://vldb.org/pvldb/vol15/p3385-xu.pdf); [PolarDB VLDB'25](https://www.vldb.org/pvldb/vol18/p5059-chen.pdf); [Alibaba PolarDB blog](https://www.alibabacloud.com/blog/alibaba-clouds-polardb-breaks-tpc-c-benchmark-world-record-with-innovative-three-layer-decoupling-architecture_602021); [TPC FDR PolarDB](https://www.tpc.org/results/fdr/tpcc/alibaba~tpcc~alibaba_cloud_polardb_limitless~fdr~2025-01-27~v01.pdf); [RegattaDB](https://regatta.dev/blog/regattadb-tpc-c-benchmark-750k-tps-1-5m-warehouses); [DBTA Oracle SPARC](https://www.dbta.com/Editorial/News-Flashes/Oracle-Announces-World-Record-TPC-C-Benchmark-with-Oracle-DB-on-a-SPARC-Supercluster-with-SPARC-T3-T4-Servers-72719.aspx).

**Logging, NVMe, CXL:** [Autonomous Commit, TUM 2025](https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/latency.pdf); [simplyblock NVMe latency](https://simplyblock.io/glossary/nvme-latency); [cedardb SSD latency](https://cedardb.com/blog/ssd_latency); [smalldatum fsync](http://smalldatum.blogspot.com/2026/01/ssds-power-loss-protection-and-fsync.html); [Optane DC PMEM, NVSL](https://www.nvsl.io/data/bib/pdfs/2019arXiv-AEP.pdf); [CXL intro, Das Sharma 2024](https://dl.acm.org/doi/full/10.1145/3669900); [Pond CXL pooling, ASPLOS'23](https://www.microsoft.com/en-us/research/wp-content/uploads/2022/10/2023_Pond_asplos23_official_asplos_version.pdf); [Stanford case against CXL pooling](https://sing.stanford.edu/site/assets/publications/cxl-hotnets23.pdf); [cpu_energy_kb.md](./cpu_energy_kb.md).

**Instruction-level / energy:** [Agner Fog instruction tables](https://www.agner.org/optimize/instruction_tables.pdf); [uops.info](https://uops.info/table.html); [Chips and Cheese — Sapphire Rapids](https://chipsandcheese.com/p/a-peek-at-sapphire-rapids); [SO L1 latency](https://stackoverflow.com/questions/10274355/cycles-cost-for-l1-cache-hit-vs-register-on-x86); [SPEC ICPE'22 EPYC/Rome memory](https://research.spec.org/icpe_proceedings/2022/proceedings/p165.pdf); [AVX-512 hash tables, VLDB'23](https://www.vldb.org/pvldb/vol16/p2755-bother.pdf).

---

*Prepared from web search + the in-repo `cpu_energy_kb.md`. Figures marked
*(derived)* are computed by the author from the cited primary numbers and should
be treated as estimates, not measured results.*
