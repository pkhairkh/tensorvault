# Storage Format Problems

> Problems related to the instruction-shaped storage format: 4 KB pages,
> 2 MB regions, 2 GB tablets, the WAL, and compression.
>
> **Research source**: `docs/instruction_first_architecture.md`,
> `docs/research/info_theory_for_db.md` (compression, ECC, ANS).

---

## P-03-01: 4 KB page format 🟢

**Layer**: Storage format
**Status**: 🟢 solved (implemented in `src/storage/page.rs`)
**Math**: none
**Effort**: —
**Impact**: high

### Problem (solved)

The fundamental I/O unit is a 4 KB page:
- 4 KB matches the OS page size and x86 TLB granularity
- 4 KB = 64×64-byte cache lines = 512 u64 cells (504 after 64-byte header)
- Scanning a 4 KB page with `VPCMPEQQ` takes ~64 cycles, fitting in L1

### Resolution

`Page` struct in `src/storage/page.rs` with:
- 64-byte header (page type, tier hint, homogeneity mask, row count, checksum, predecessor/successor)
- 4032 bytes of cell data (504 u64 cells)
- xxh3 checksum for corruption detection

---

## P-03-02: 2 MB region and 2 GB tablet format 🟢

**Layer**: Storage format
**Status**: 🟢 solved (implemented in `src/storage/tablet.rs`)
**Math**: none
**Effort**: —
**Impact**: high

### Problem (solved)

Pages are grouped into 2 MB regions (huge page granularity, unit of
migration) and 2 GB tablets (NUMA placement unit).

### Resolution

`Region` (2 MB = 512 pages) and `Tablet` (2 GB = 1024 regions) structs
implemented. Region carries access statistics for the migration policy.

---

## P-03-03: Column compression (ANS / arithmetic coding) 🔴

**Layer**: Storage format
**Status**: 🔴 open
**Math**: I (information theory — ANS, rate-distortion)
**Effort**: L (3–6 months)
**Impact**: high

### Problem

This is **Enhancement 1** from `docs/math_enhancements.md`. Columns should
be compressed with Asymmetric Numeral Systems (ANS, Duda 2009) for
entropy-optimal compression with SIMD-decodable format.

The plan:
- 8 interleaved ANS streams per column
- Decode via `VPGATHERDD` (8 parallel 32-bit table lookups per cycle)
- ~2× compression over zstd on real column data
- ~5 G cells/sec decode throughput

### Open questions

- How do we build per-column static frequency tables at load time?
- Should the codec be per-page or per-region? (Per-page allows random access;
  per-region gives better compression ratios.)
- How does ANS interact with the page header (which must be uncompressed for
  random access)?

### Success criteria

- A `DecodeAnsAvx512` kernel in the kernel table.
- Pages carry a "compressed" flag in the header.
- The scheduler transparently decodes ANS pages before running scan kernels.
- Benchmark: 2× compression ratio, 5 G cells/sec decode.

---

## P-03-04: Lossy compression via rate-distortion 🔴

**Layer**: Storage format
**Status**: 🔴 open
**Math**: I (rate-distortion theory)
**Effort**: XL (6+ months)
**Impact**: medium

### Problem

For approximate queries, we can tolerate bounded error in column data.
Rate-distortion theory (Shannon 1948; see `docs/research/info_theory_for_db.md`
§1) gives the minimum bitrate for a given max distortion:

$$
R(D) = \min_{p(\hat{x}|x): E[d(x,\hat{x})] \le D} I(X; \hat{X})
$$

For example, a `price` column with values in [0, 1000] and a 1% error
tolerance can be compressed to ~6 bits/value (vs 64 bits for f64).

### Open questions

- How do we expose this to the user? (Column-level `COMPRESSION LOSSY ε=0.01`?)
- How does the planner know which columns can be read lossy-ly?
- Multi-resolution: store high-precision in L3, low-precision in CXL?

### Success criteria

- A `LossyColumn` type with a declared ε.
- The scan kernel returns approximate results with `(ε, δ)` guarantees.
- Benchmark: 8× compression on float columns with 1% error.

---

## P-03-05: ZNS-aware WAL 🟡

**Layer**: Storage format
**Status**: 🟡 partial (WAL exists, but not ZNS-aware)
**Math**: none
**Effort**: M
**Impact**: high

### Problem

The WAL (`src/storage/wal.rs`) appends to a regular file. On a ZNS SSD,
this works but pays GC tail latency. A ZNS-aware WAL would:

1. Allocate zones explicitly (`ioctl(BLKOPENZONE)`)
2. Write sequentially within a zone
3. Finish a zone when full (`ioctl(BLKFINISHZONE)`)
4. Never overwrite — old zones are reset in bulk

### Open questions

- How do we detect a ZNS device? (`ioctl(BLKGETZONESZ)`)
- How do we handle the zone-finish boundary mid-transaction?

### Success criteria

- `Wal::open()` detects ZNS and uses zone-aware I/O.
- Benchmark: p99 fsync latency < 30 µs on ZNS vs ~100 µs on conventional NVMe.

---

## P-03-06: LSM-tree compaction 🔴

**Layer**: Storage format
**Status**: 🔴 open
**Math**: none
**Effort**: L
**Impact**: high

### Problem

The WAL handles appends, but we need an LSM-tree (or similar) for
mutable data. Updates go to a memtable; when full, flush to an SSTable on
NVMe; background compaction merges SSTables.

The compaction must be:
- **Tier-aware**: read from NVMe, write to NVMe, but keep the merge working
  set in L3
- **ZNS-friendly**: each SSTable level is a set of zones
- **AVX-512-accelerated**: the merge step is a sorted-merge that can use
  `VPCMPEQQ` for key comparison

### Open questions

- Should we use a leveled LSM (like RocksDB) or a tiered LSM (like Cassandra)?
- How do we integrate compaction with the memory manager's bandwidth budget?

### Success criteria

- An `LsmTree` struct with `put`, `get`, `delete`.
- Background compaction that doesn't stall foreground writes.
- Benchmark: 100K writes/sec sustained, < 1 ms p99 read latency.

---

## P-03-07: Erasure-coded WAL replication 🔴

**Layer**: Storage format
**Status**: 🔴 open
**Math**: I (ECC — Reed-Solomon, RaptorQ)
**Effort**: L
**Impact**: high

### Problem

The WAL is replicated across racks for durability. Currently (planned), we
use Raft, which replicates the full log to a quorum. This is 3× write
amplification.

Erasure coding (Reed-Solomon or RaptorQ — see `docs/research/info_theory_for_db.md`
§3) can achieve the same durability with lower overhead:
- RS(10, 4): 10 data shards + 4 parity shards = 1.4× overhead (vs 3× for
  Raft replication)
- RaptorQ: rateless, can recover from any 10 of 14+ shards

### Open questions

- What's the right (n, k) for WAL durability?
- How do we handle the parity computation without blocking writes?
  (Compute parity in the DPU? On a separate core?)

### Success criteria

- A `ErasedWal` that stripes WAL records across N nodes with K parity shards.
- Benchmark: 1.4× write overhead for the same durability as 3× Raft.

---

## P-03-08: Page checksum and corruption recovery 🟡

**Layer**: Storage format
**Status**: 🟡 partial (xxh3 checksum exists, no recovery)
**Math**: I (ECC)
**Effort**: M
**Impact**: medium

### Problem

Each page has an xxh3 checksum (`Page::update_checksum()`). If a page is
corrupted, we detect it but can't recover. We should use an error-correcting
code (e.g., a simple Hamming code or a CRC + parity) that can correct
single-bit errors.

### Open questions

- Is single-bit correction worth the overhead? (NAND SSDs already do internal
  ECC.)
- Should we use LDPC (like SSDs) or a simpler code?

### Success criteria

- `Page::verify_and_correct()` that fixes single-bit errors.
- Benchmark: < 5% overhead on page read.

---

## P-03-09: Variable-length cell support 🔴

**Layer**: Storage format
**Status**: 🔴 open
**Math**: none
**Effort**: M
**Impact**: high

### Problem

The current format assumes every cell is exactly 8 bytes (u64). This is
great for SIMD but wasteful for small types (bool = 1 bit, i8 = 1 byte).

We need a way to store variable-length cells without breaking the SIMD
invariant. Options:
1. **Bit-packing**: pack N small values into one u64 (e.g., 8×i8 per u64)
2. **Sidecar format**: store small values in a separate bit-packed column,
   keep the main column as u64 indices
3. **Dictionary encoding**: replace small values with dictionary indices

### Open questions

- Which strategy preserves the 8-byte SIMD invariant?
- How does the planner know which strategy is in use?

### Success criteria

- A `BitPackedColumn` type for i8/i16/bool columns.
- The scan kernel works on bit-packed columns via `VPSRLD` + `VPCMPEQB`.

---

## P-03-10: Schema-on-read column encoding 🔴

**Layer**: Storage format
**Status**: 🔴 open
**Math**: I (MDL), V (category theory — functorial migration)
**Effort**: L
**Impact**: high

### Problem

For semi-structured data (JSON, logs), columns are stored as raw 64-bit
words (NaN-boxed). The schema (type interpretation) is discovered at query
time by the MDL selector (`src/schema/mdl.rs`).

But MDL is currently a one-shot decision at query start. We need:
1. **Streaming MDL**: re-evaluate the type interpretation per batch as data
   evolves
2. **Multi-resolution MDL**: different interpretations for different tiers
   (high-precision in L3, lossy in CXL)
3. **Provenance tracking**: which interpretation was used for each result?

### Open questions

- How do we handle queries that span multiple interpretations?
- Can we use functorial data migration (Spivak) to formalize the
   reinterpretation?

### Success criteria

- A `StreamingMdlSelector` that re-evaluates per batch.
- A `Provenance` type that records the interpretation used per result row.

---

## Summary

| # | Problem | Status | Effort | Impact |
|---|---------|--------|--------|--------|
| 01 | 4 KB page format | 🟢 | — | high |
| 02 | 2 MB region and 2 GB tablet format | 🟢 | — | high |
| 03 | Column compression (ANS / arithmetic coding) | 🔴 | L | high |
| 04 | Lossy compression via rate-distortion | 🔴 | XL | medium |
| 05 | ZNS-aware WAL | 🟡 | M | high |
| 06 | LSM-tree compaction | 🔴 | L | high |
| 07 | Erasure-coded WAL replication | 🔴 | L | high |
| 08 | Page checksum and corruption recovery | 🟡 | M | medium |
| 09 | Variable-length cell support | 🔴 | M | high |
| 10 | Schema-on-read column encoding | 🔴 | L | high |
