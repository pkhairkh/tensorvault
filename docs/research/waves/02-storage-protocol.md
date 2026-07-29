# Wave 2: Storage Format + Protocol Research

**Engine context:** instruction-first, memory-centric; 64-bit word values; explicit memory tiers; AVX-512 kernel table; WAL on ZNS NVMe; CXL single-rack + Raft cross-rack boundary coordinator.

**Methodology note on energy figures.** Where a measured joules/op value was not directly reported in a paper, I derive it as `TDP_used ÷ throughput` and label it **(est.)**. TDP assumptions: AVX-512 core ≈ 15–20 W under heavy SIMD; single DRAM channel read ≈ 1–2 nJ per 64 B; NVMe ZNS package ≈ 6–8 W at ~2–3 GB/s ⇒ ~3 nJ/B. These are order-of-magnitude, not silicon-precise.

---

# PART A — STORAGE FORMAT PROBLEMS (10)

## P-S-01: Column Compression (ANS / Arithmetic Coding)

### Candidate Solution A: Interleaved rANS with AVX-512 decode
- **Approach:** Encode each column with rANS; interleave 4–8 independent rANS codecs per AVX-512 lane so 32 symbols decode per `vpshufb`/gather sequence (Giesen 2014).
- **Performance:** Scalar rANS ≈ 660–820 MB/s; AVX-512 interleaved + Recoil-style parallel decode reaches **11+ GB/s decode** (Recoil, arXiv:2306.12141). Encode ≈ 2–4 GB/s. Latency dominated by L1 refill of the 256-entry coder table (~20 ns warm).
- **Time to implement:** ~3 months. rANS is well-documented; the engineering is in the SIMD layout and the table-merge for multi-column. Existing reference (ryg_rans, FSE) shortens it.
- **Energy cost:** ≈ **1.5–2 nJ/symbol (est.)** at 10 GB/s × 15 W core. Beating Huffman here because entropy is closer to optimum so fewer bits written downstream.
- **Upside:** Entropy-optimal (within ~0.1% of arithmetic coding), and the SIMD decode is competitive with vectorized Huffman while compressing 8–15% better.
- **Downside:** Encode is the slow path; non-streamable in the strict sense (need a flush frontier); branchy scalar renorm hurts non-SIMD fallback paths.
- **Key paper:** Giesen et al., "Interleaved Entropy Coders," arXiv:1402.3392 (2014); Recoil, arXiv:2306.12141 (2023).

### Candidate Solution B: tANS / FSE (Duda)
- **Approach:** Table-based ANS (FSE state machine); single-state decode with a 256–4096 entry table.
- **Performance:** Decode ~1–2 GB/s scalar, ~4–6 GB/s with SSE/AVX2. Lower than interleaved rANS because of the single-state dependency chain (Zstd uses this).
- **Time to implement:** ~2 months (FSE is a battle-tested library).
- **Energy cost:** ≈ **3–4 nJ/symbol (est.)** — decode-bound, fewer bits saved than rANS so more downstream I/O energy.
- **Upside:** Simplest to integrate; used by Zstd so tooling exists; state machine is branch-predictable.
- **Downside:** Compression ratio ~1–3% worse than rANS; the single-state chain caps SIMD width.
- **Key paper:** Duda, "Asymmetric Numeral Systems," arXiv:0902.0277 (2009).

### Candidate Solution C: zstd (off-the-shelf)
- **Approach:** Use Facebook's Zstd (FSE + LZ77 + FSE/Huffman) per column block.
- **Performance:** ~1–1.5 GB/s encode, ~4–7 GB/s decode at level 3. Ratio excellent (often better than raw rANS because of the LZ stage).
- **Time to implement:** ~0.5 month (link the library).
- **Energy cost:** ≈ **2–3 nJ/byte (est.)** decode; higher encode energy at high levels.
- **Upside:** Zero R&D, robust, proven on petabyte stores.
- **Downside:** Not tuned for the 64-bit-word column layout; opaque block format fights the "instruction-first" kernel table philosophy; can't fuse decode into AVX-512 filter kernels.
- **Key paper:** Collet, "Zstandard compression," RFC 8478 (2018).

### Recommendation
**Interleaved rANS (A).** It is the only option whose decode throughput (11+ GB/s) keeps up with AVX-512 filter kernels, gives entropy-optimal ratio, and stays inside the engine's "kernel table" model. Cost is ~3 engineering months — acceptable. Use tANS/FSE only as the scalar fallback path. zstd only as an import/export interop codec.

---

## P-S-02: Lossy Compression via Rate-Distortion (Bounded-Error)

### Candidate Solution A: Product Quantization (PQ)
- **Approach:** Split each 64-bit word into d sub-vectors (or treat a column of N words as N×1 vectors), train k-means codebooks per sub-space, store 8-bit codes (Jégou 2011). Distance computed via SIMD lookup tables (ADC).
- **Performance:** Compression 8–100×; distance eval ~1–5 billion ops/s with SIMD ADC (Quicker ADC, IEEE TPAMI 2021). Decode (reconstruct) ~2–4 GB/s.
- **Time to implement:** ~4 months. Codebook training is offline; the per-batch retrain and the bounded-error contract (max distortion guarantee) are the hard parts.
- **Energy cost:** ≈ **0.5–1 nJ/distance (est.)** — table lookups are L1-resident, very cheap. Reconstruction ~1 nJ/value.
- **Upside:** Best ratio/throughput tradeoff for vectors; SIMD ADC is cache-friendly; mature.
- **Downside:** Distortion is *expected*, not *bounded per element* — quantization error is statistical. Hard to give a hard ε guarantee the WAL/durability layer needs.
- **Key paper:** Jégou, Douze, Schmid, "Product Quantization for Nearest Neighbor Search," IEEE TPAMI 33(1), 2011.

### Candidate Solution B: Lloyd-Max Scalar Quantizer
- **Approach:** Per-column scalar non-uniform quantizer (Lloyd 1957 / Max 1960) with a fixed ε bound; store code + reconstruction table.
- **Performance:** Encode/decode ~5–10 GB/s scalar (one divide + table lookup). Trivially SIMD-able.
- **Time to implement:** ~1.5 months.
- **Energy cost:** ≈ **0.3 nJ/value (est.)** — divide is the cost; can be replaced by multiply-shift.
- **Upside:** Gives a *guaranteed* per-element error bound (max |x−x̂| ≤ ε), which fits a durability contract. Simple to reason about.
- **Downside:** Only 1-D; loses multi-dimensional correlation; ratio worse than PQ (typically 4–8× vs 32×+).
- **Key paper:** Lloyd, "Least Squares Quantization in PCM," IEEE TIT 28(2), 1982 (orig. 1957); Max, IRE TIT 1960.

### Candidate Solution C: Rate-Distortion LP / bounded-error coding
- **Approach:** Solve the rate-distortion optimization (Cover & Thomas ch.10) per batch as a small LP over candidate quantizers to hit a target rate R at distortion ≤ D.
- **Performance:** Encode is **slow** (LP per batch, ms-scale); decode fast (~5 GB/s). Throughput-limited by the optimizer, not the codec.
- **Time to implement:** ~6 months (LP solver integration, batch boundary handling).
- **Energy cost:** ≈ **10–50 nJ/value (est.)** at encode (LP is compute-heavy); ~0.5 nJ/value decode.
- **Upside:** Provably optimal R(D) for the batch; strongest theoretical guarantee.
- **Downside:** Encode latency/energy prohibitive for an online WAL; LP solver is a heavy dependency.
- **Key paper:** Cover & Thomas, *Elements of Information Theory*, ch.10 (2nd ed., 2006).

### Recommendation
**Lloyd-Max (B) as the default bounded-error codec**, with PQ (A) as an opt-in for vector columns where statistical error is acceptable. Lloyd-Max gives the hard per-element ε bound the engine needs for "lossy-but-correctable" durability; it is 4× faster to ship and 2–3× lower decode energy than PQ's reconstruction. Reserve rate-distortion LP for offline compaction re-encoding, not the hot path.

---

## P-S-03: ZNS-Aware WAL

### Candidate Solution A: io_uring + libzns zone manager
- **Approach:** Linux io_uring submission queue drives zone append commands; a userspace zone manager maps logical WAL slots to active zones, finishing zones at 2 MB granularity.
- **Performance:** ZNS eliminates GC, giving **4–5× lower write amplification and ~57% better avg latency** vs conventional NVMe, with stable tail latency (Bjørling ATC 2021; CLUSTER 2023). io_uring adds ~0.5–1 µs syscall overhead amortized over batched appends; sustained ~2–3 GB/s per device.
- **Time to implement:** ~3 months. Zone state machine + WAL segment mapping + finish/reset lifecycle.
- **Energy cost:** ≈ **3 nJ/B written (est., device)** at 2.5 GB/s × 6 W; ~20% lower than conventional SSD due to no GC thrash (zonedstorage.io; Micron perf-per-watt data).
- **Upside:** Stays on the kernel path — portable, no DPDK-style pinning; gets most of the ZNS win.
- **Downside:** io_uring syscall + page-cache bypass still costs cycles; zone-finish must be coordinated with CXL commit records.
- **Key paper:** Bjørling et al., "ZNS: Avoiding the Block Interface Tax," USENIX ATC 2021.

### Candidate Solution B: SPDK + ZNS (kernel-bypass)
- **Approach:** SPDK NVMe driver in userspace, polling-mode, direct zone-append to NVMe submission queue.
- **Performance:** ~3–4 GB/s per device, latency **<20 µs** p99 for 4 KB appends; CPU cost is the polling core (1 core ≈ 100%).
- **Time to implement:** ~5 months (SPDK env integration, huge-page pool, reactor threading model).
- **Energy cost:** ≈ **2 nJ/B (est.)** at 3.5 GB/s but a **dedicated ~15 W polling core** burns constantly → bad idle energy (~15 W idle tax).
- **Upside:** Lowest latency; bypasses kernel entirely; aligns with AVX-512 reactor model.
- **Downside:** Burns a core 24/7 even when idle; SPDK is a large, opinionated dependency; harder to debug.
- **Key paper:** Walker et al. (SPDK); atlarge CLUSTER 2023 characterization.

### Candidate Solution C: Custom zone manager on raw NVMe passthrough
- **Approach:** Hand-rolled NVMe driver over /dev/nvme0 char device with custom zone state machine, tailored to the WAL's segment=zone invariant.
- **Performance:** Comparable to SPDK (~3 GB/s, <25 µs) but tunable to the exact commit-record layout.
- **Time to implement:** ~8 months (driver, IRQ handling, multi-queue, fault paths). High risk.
- **Energy cost:** ≈ **2–3 nJ/B (est.)**, interrupt-driven so no polling-core tax when idle.
- **Upside:** Full control; can co-design zone size with the 2 MB region/2 GB tablet layout.
- **Downside:** Reimplementing a driver is a maintenance sink; certification/firmware-compat risk.
- **Key paper:** Bjørling, "Zoned Storage," IEEE Computer 2023.

### Recommendation
**io_uring + libzns (A) for v1.** It captures 80% of the ZNS latency/energy win at 3 months and stays on the portable kernel path. Plan B is SPDK (B) only if profiling shows the io_uring syscall path is the bottleneck at high fan-in. Avoid C unless the zone-size/segment co-design is shown to be load-bearing.

---

## P-S-04: LSM-Tree Compaction (Tier-Aware, AVX-512 Merge)

### Candidate Solution A: Leveled compaction (RocksDB-style)
- **Approach:** L0→L6, each level 10× the prior, one sorted run per level; AVX-512 merge uses `vpcmpeqq` on 64-bit keys.
- **Performance:** Read amp O(1) per level (good for reads); **write amp 10–30×** (Dong, SIGMOD 2017; RUM conjecture, ICDE 2017). AVX-512 merge ~2–4 GB/s of keys.
- **Time to implement:** ~4 months (level metadata, compaction picker, merge kernel).
- **Energy cost:** ≈ **20–40 nJ/key written (est.)** due to repeated re-writes across 6 levels.
- **Upside:** Best read latency; predictable; most-operator-friendly.
- **Downside:** Write amplification burns NVMe endurance and joules; backpressure stalls under heavy writes.
- **Key paper:** O'Neil et al., "The LSM-Tree," Acta Informatica 33(1), 1996; Dong et al., SIGMOD 2017.

### Candidate Solution B: Tiered compaction (Cassandra-style)
- **Approach:** Each tier holds R runs; merge only when tier full → one merge per tier.
- **Performance:** **Write amp 2–4×** (huge win); read amp O(R) per tier (worse); space amp high transiently.
- **Time to implement:** ~3 months.
- **Energy cost:** ≈ **3–6 nJ/key written (est.)** — ~5–7× less than leveled.
- **Upside:** Lowest write energy — critical for a ZNS-endurance-conscious WAL/LSM.
- **Downside:** Reads pay for R sorted runs; needs bloom filters + tier-aware caching to stay flat.
- **Key paper:** O'Neil 1996 (tiered variant); Scylla/Cassandra universal compaction.

### Candidate Solution C: Hybrid (leveled-N / tiered+leveled)
- **Approach:** Tiered in hot levels (L0–L2), leveled in cold (L3+) — RocksDB's `tiered+leveled`.
- **Performance:** Write amp ~4–8×, read amp close to leveled. Tunable to the tier boundary.
- **Time to implement:** ~5 months (two pickers, transition logic).
- **Energy cost:** ≈ **6–10 nJ/key written (est.)**.
- **Upside:** Best of both; maps naturally onto the engine's explicit memory tiers (hot tier = tiered, cold = leveled).
- **Downside:** Most code surface; tuning knobs proliferate.
- **Key paper:** Dong et al. (RocksDB hybrid modes), SIGMOD 2017; RUM conjecture, ICDE 2017.

### Recommendation
**Hybrid tiered+leveled (C), aligned to memory tiers.** The engine already has explicit tiers; co-design compaction policy with them (tiered on the hot CXL-attached tier where write energy dominates, leveled on cold ZNS-backed tiers where read latency dominates). Energy saving vs pure leveled is 3–5× on the write path. Ship tiered-only (B) first as the simpler subset, then add leveled cold tiers.

---

## P-S-05: Erasure-Coded WAL Replication

### Candidate Solution A: Reed-Solomon(10,4) with AVX-512 GFNI
- **Approach:** RS over GF(2^8); 10 data + 4 parity shards per WAL segment; encode/decode via AVX-512 GFNI (`vgf2p8affineqb`).
- **Performance:** GFNI-accelerated GF(2^8) multiply reaches **~10–20 GB/s** encode throughput (Plank NCA 2013; Intel GFNI guide); ~2–4 GB/s without GFNI.
- **Time to implement:** ~3 months. GFNI intrinsics + Jerasure-style matrix inversion for decode.
- **Energy cost:** ≈ **0.5–1 nJ/B (est.)** at 15 GB/s × 15 W — cheaper than the NVMe write itself.
- **Upside:** Maximum storage efficiency (n/(n+k) = 10/14 = 71%); MDS-optimal; deterministic.
- **Downside:** Decode needs k surviving shards before any recover (all-or-nothing per stripe); matrix inversion cost on rebuild.
- **Key paper:** Reed & Solomon, J. SIAM 1960; Plank et al., "Screaming Fast Galois Field Arithmetic Using Intel SIMD," NCA 2013.

### Candidate Solution B: RaptorQ (RFC 6330) fountain code
- **Approach:** Rateless: emit as many repair symbols as needed; receiver collects any K+ε to decode.
- **Performance:** Encode ~0.5–1 GB/s (sparse-matrix for >200 symbols, cberner 2020); decode ~0.5 GB/s. **~10–20× slower than RS** for small symbols.
- **Time to implement:** ~4 months (RFC 6330 is intricate; GF(256) dense + LT subcode).
- **Energy cost:** ≈ **8–15 nJ/B (est.)** — sparse matvec is compute-heavy; high memory footprint (93 MB for 50k symbols).
- **Upside:** Any-k-of-N decoding → survives arbitrary loss patterns; great for lossy cross-rack links.
- **Downside:** Storage overhead higher for fixed redundancy; throughput too low for the hot WAL path.
- **Key paper:** Luby, "LT codes," FOCS 2002; Shokrollahi et al., "RaptorQ," RFC 6330 (2011).

### Candidate Solution C: Cascade / layered RS + parity
- **Approach:** Local RS(6,3) within a rack + global RS for cross-rack (Microsoft Azure LRC pattern).
- **Performance:** Encode ~8–12 GB/s (RS path); repair reads reduced ~2–4× vs plain RS(10,4).
- **Time to implement:** ~6 months (two-layer orchestration).
- **Energy cost:** ≈ **1–1.5 nJ/B (est.)** + metadata; repair energy much lower.
- **Upside:** Cheapest *repairs* (local recovery) — important at scale.
- **Downside:** More metadata; storage efficiency slightly below plain RS(10,4).
- **Key paper:** Huang et al., "Erasure Coding in Windows Azure Storage," USENIX ATC 2012.

### Recommendation
**RS(10,4) + GFNI (A) for the hot WAL** — its 10–20 GB/s encode keeps pace with the ZNS write path and energy is negligible vs the flash write itself. Use **RaptorQ (B) for cross-region log shipping** (P-P-07) where lossy links make any-k decoding valuable. Cascade (C) is the right answer only once rack count exceeds ~6 and repair bandwidth becomes the bottleneck.

---

## P-S-06: Page Checksum and Corruption Recovery

### Candidate Solution A: CRC32C (detect) + per-page parity (correct 1 error)
- **Approach:** xxh3/CRC32C for burst detection on the 4 KB page; add a single parity row (XOR) per page to correct one 64-bit word error.
- **Performance:** CRC32C via SSE4.2 `crc32` ~10–15 GB/s; xxh3 AVX-512 ~30+ GB/s. Parity XOR negligible.
- **Time to implement:** ~1 month.
- **Energy cost:** ≈ **0.1 nJ/B (est.)** at 30 GB/s × 15 W — nearly free.
- **Upside:** Detection is essentially free; single-bit correction covers the dominant NVMe failure mode (single-bit flips).
- **Downside:** Can't correct multi-bit bursts; parity only fixes one word.
- **Key paper:** Intel SW Developers Manual (CRC32 instr); Collet, xxHash.

### Candidate Solution B: Hamming SEC code over the 4 KB page
- **Approach:** (72,64) Hamming per 64-bit word → single-error-correct, double-error-detect (SECDED); store 8 parity bits/word.
- **Performance:** Encode/verify ~5–8 GB/s with AVX-512 (bit-matrix via `vgf2p8affineqb`); ~12.5% space overhead.
- **Time to implement:** ~2 months.
- **Energy cost:** ≈ **0.3 nJ/B (est.)**.
- **Upside:** True single-error *correction*, in-place, no rebuild.
- **Downside:** 12.5% space tax on every page; double-bit errors still unrecoverable.
- **Key paper:** Hamming, Bell System Tech. J. 1950; MacKay, *Information Theory…*, Cambridge 2003.

### Candidate Solution C: LDPC (soft-decision) per page
- **Approach:** Encode each 4 KB page with a rate-0.9 LDPC; iterative belief-propagation decode.
- **Performance:** Software LDPC decode ~0.1–1 Gb/s (AVX-512, Sci. Direct 2023); ASIC ~9 Gb/s at 62 pJ/b (arXiv:2512.17834). **Far too slow for the hot page path.**
- **Time to implement:** ~6 months.
- **Energy cost:** ≈ **5–20 nJ/B (est.)** in software — orders of magnitude worse than CRC.
- **Upside:** Near-Shannon correction; handles correlated multi-bit errors.
- **Downside:** Latency/energy catastrophic for a 64-bit-word engine; complexity enormous.
- **Key paper:** Gallager, "Low-Density Parity-Check Codes," 1963; MacKay 2003.

### Recommendation
**CRC32C + per-page XOR parity (A).** Detection is free at 30 GB/s, and single-word parity corrects the dominant failure class with <1% space overhead. Add Hamming (B) only on "cold/archive" tiers where space is plentiful and silent corruption is a real risk (e.g., erasure-coded backups). LDPC (C) is out of scope for the hot path — its energy is ~100× CRC's.

---

## P-S-07: Variable-Length Cell Support

### Candidate Solution A: Bit-packing (StreamVByte / FastPFOR) with a fixed u64 envelope
- **Approach:** Pack small types (i8/i16/i32, tags) into the 64-bit word using vectorized varint (StreamVByte: ~4–6 GB/s decode) or bit-packed frames.
- **Performance:** StreamVByte decode ~4–6 GB/s (Lemire & Boytsov 2015); FastPFOR ~1–3 GB/s.
- **Time to implement:** ~2 months.
- **Energy cost:** ≈ **2–3 nJ/value (est.)** at 5 GB/s × 15 W.
- **Upside:** Keeps the "everything is a u64" invariant while sub-encoding; SIMD-native; reversible.
- **Downside:** Branchy control stream (length byte); not entropy-optimal.
- **Key paper:** Lemire & Boytsov, "Decoding billions of integers per second through vectorization," Softw. Pract. Exp. 2015; Zukowski et al., "Super-scalar RAM-CPU cache compression," ICDE 2006.

### Candidate Solution B: Dictionary encoding (small-cardinality columns)
- **Approach:** Replace value with a code into a per-column dictionary; codes fit in 8/16 bits inside the u64.
- **Performance:** Encode/decode ~10+ GB/s (lookup); ratio excellent for low-cardinality.
- **Time to implement:** ~1.5 months.
- **Energy cost:** ≈ **0.5 nJ/value (est.)** — L1-resident dict.
- **Upside:** Highest ratio for low-cardinality; trivial decode; cache-friendly.
- **Downside:** Useless for high-cardinality/continuous data; dict must be versioned across snapshots.
- **Key paper:** Abadi et al., "Integrating compression and execution in column-oriented database systems," SIGMOD 2006.

### Candidate Solution C: Sidecar format (out-of-line variable area)
- **Approach:** Fixed 64-bit cell holds a (tag, offset, len) tuple; actual bytes live in a sidecar variable region per page.
- **Performance:** Decode = zero (pointer chase); ~memory-latency-bound (~80–100 ns for the sidecar fetch).
- **Time to implement:** ~2.5 months (page layout, GC of sidecar holes).
- **Energy cost:** ≈ **1–2 nJ/value** (one extra DRAM access ≈ 1–2 nJ).
- **Upside:** Handles truly variable types (strings, blobs) the u64 envelope can't; no truncation.
- **Downside:** Two memory accesses per cell; defeats the single-word instruction model; fragmentation.
- **Key paper:** Abadi et al., "C-Store," VLDB 2005.

### Recommendation
**Bit-packing (A) as the default** for small numeric types (preserves the u64 instruction model, SIMD-fast). **Dictionary (B) as a per-column auto-pick** when cardinality < √N. **Sidecar (C) only for string/blob columns** that cannot fit a word. This trio covers the space with a clear policy: try dict → else bitpack → else sidecar.

---

## P-S-08: Schema-on-Read Column Encoding (Streaming MDL per Batch)

### Candidate Solution A: Per-batch MDL model selection
- **Approach:** For each batch, pick the encoding (raw/run-length/dict/bitpack/rANS) minimizing `L(data|model) + L(model)` (Grünwald 2007). The model header is stored with the batch.
- **Performance:** Selection ~µs per batch (a handful of candidate encodings evaluated on a sample); decode uses the chosen kernel from the AVX-512 table.
- **Time to implement:** ~3 months (encoder scoring + dispatch).
- **Energy cost:** ≈ **5–10 nJ/value at encode (est.)**; decode cost = chosen kernel's cost.
- **Upside:** Principled, automatic, adapts to data drift; no global schema lock-in.
- **Downside:** Encode compute non-trivial; model header overhead on small batches.
- **Key paper:** Grünwald, *The Minimum Description Length Principle*, MIT Press 2007; Rissanen, Automatica 1978.

### Candidate Solution B: Multi-resolution (coarse summary + refined per-batch)
- **Approach:** Maintain a coarse region-level schema with per-batch refinements (delta models) — amortizes selection cost.
- **Performance:** Encode ~2× faster than per-batch MDL (reuse coarse model); decode same.
- **Time to implement:** ~4 months.
- **Energy cost:** ≈ **3–5 nJ/value at encode (est.)**.
- **Upside:** Lower per-batch overhead; smoother across batches.
- **Downside:** Coarse model can go stale under sharp distribution shift; harder to reason about correctness.
- **Key paper:** Grünwald 2007 (hierarchical MDL); Spivak, "functorial data migration," CACM 2012.

### Candidate Solution C: Provenance tracking (per-cell model tag)
- **Approach:** Each cell carries a model-ref tag; schema truly deferred to read time, queries reconstruct on demand.
- **Performance:** Decode = chase tag + run kernel; ~2× the chosen kernel's latency due to tag dispatch.
- **Time to implement:** ~5 months.
- **Energy cost:** ≈ **2–4 nJ/value extra (est.)** for tag dispatch; storage overhead for tags.
- **Upside:** Maximum flexibility; provenance enables audit/explainability.
- **Downside:** Per-cell tags bloat storage and hurt cache density; complex query planning.
- **Key paper:** Spivak 2012; Buneman et al., "Provenance in databases," SIGMOD 2001.

### Recommendation
**Per-batch MDL (A).** It is the cleanest fit: each batch picks the best kernel from the existing AVX-512 table, the model header rides along in the 4 KB page, and decode energy is just the chosen kernel's. Layer multi-resolution (B) on top once batch volumes make per-batch selection cost visible. Avoid C unless provenance is a hard product requirement.

---

## P-S-09: 4 KB Page Format (SOLVED)

### Candidate Solution A: 64-byte header + 504 cells
- **Approach:** 64 B header (magic, CRC, cell count, free-space bitmap, model tag); 504 × 8 B cells = 4032 B; 96 B slack for alignment/parity.
- **Performance:** One cache-line header, cells cache-line aligned; AVX-512 `vmovdqa64` streams 8 cells/instruction.
- **Time to implement:** ~1 month (already specified).
- **Energy cost:** Header parse ≈ **2–3 nJ (est.)** (one CL read); cell stream ~1 nJ/8 cells.
- **Upside:** Header fits one cache line; cell grid is SIMD-aligned; trivial.
- **Downside:** Fixed 504-cell capacity wastes slack on sparse pages.
- **Key paper:** Intel 64 and IA-32 Architectures Optimization Reference Manual.

### Recommendation
**A (as specified).** No change. Verify the header stays within one 64 B cache line (it does at 64 B) and that cell grid starts at offset 64 for `vmovdqa64` alignment.

---

## P-S-10: 2 MB Region / 2 GB Tablet (SOLVED)

### Candidate Solution A: Huge-page + NUMA-aligned regions
- **Approach:** Each region = one 2 MB transparent huge page, NUMA-bound to the CXL node owning its tablet; tablet = 1024 regions = 2 GB.
- **Performance:** Huge pages cut TLB misses ~512×; NUMA binding keeps CXL hops local (~200 ns vs cross-node).
- **Time to implement:** ~1.5 months (mmap `MAP_HUGETLB`, `mbind`, region allocator).
- **Energy cost:** TLB-miss avoidance saves ~2–5 nJ/access; NUMA locality saves CXL-traversal energy.
- **Upside:** Eliminates the TLB bottleneck for a memory-centric engine; aligns region/zone/tablet hierarchically.
- **Downside:** 2 MB granularity causes internal fragmentation on under-filled regions.
- **Key paper:** Navarro et al., "Practical, Transparent OS Support for Superpages," ASPLOS 2002; Linux `mm/hugetlbpage.c`.

### Recommendation
**A (as specified).** No change. Enforce region = THP = NUMA-local at allocation time; document the 2 MB ↔ ZNS-zone ↔ region identity, which also simplifies P-S-03/P-S-04.

---

# PART B — PROTOCOL PROBLEMS (8)

## P-P-01: Linear-Typed Memory Handles

### Candidate Solution A: Rust newtypes + Drop (affine types)
- **Approach:** Each handle is a move-only newtype whose `Drop` releases the tier slot; the borrow checker enforces single-ownership at compile time.
- **Performance:** Zero runtime cost; inlined away.
- **Time to implement:** ~1.5 months.
- **Energy cost:** 0 — compile-time only.
- **Upside:** Idiomatic Rust, no extra tooling, covers the common "use exactly once, then release" case.
- **Downside:** Rust's affine (not strictly linear) types allow *dropping* without use — a handle can be silently leaked-forgotten. Can't express multi-party handoff protocols.
- **Key paper:** Wadler, "Linear types can change the world!," IFIP TC2 1990.

### Candidate Solution B: Session-types library (compile-time protocol checks)
- **Approach:** Encode the handle's lifecycle as a session type (`send → recv → close`); the `session-types`/`mpst` crates verify sequencing at compile time (Honda 1993; Lindley & McBride 2013).
- **Performance:** Zero runtime cost (erased).
- **Time to implement:** ~3 months (protocol modeling per handle class).
- **Energy cost:** 0.
- **Upside:** Verifies *ordering* of operations across the protocol boundary, not just ownership — catches double-send, missed-ack.
- **Downside:** Steep learning curve; type errors can be inscrutable; limits to protocols expressible in the π-calculus fragment.
- **Key paper:** Honda, "Types and dyadic interaction," CONCUR 1993; Lindley & McBride, "Lightweight session types," 2013.

### Candidate Solution C: External linear-logic type checker (e.g., Liquid/ refinement)
- **Approach:** Add a refinement/linear-logic layer (Liquid Haskell-style) that proves true linearity (must-use-once).
- **Performance:** Zero runtime cost.
- **Time to implement:** ~6 months (toolchain integration, specs).
- **Energy cost:** 0.
- **Upside:** Strongest guarantee — true use-exactly-once, not just use-at-most-once.
- **Downside:** Heavy toolchain dependency; slows the build; overkill for an in-house engine.
- **Key paper:** Girard, "Linear logic," TCS 50(1), 1987.

### Recommendation
**Rust newtypes + Drop (A) for v1** — it covers ~90% of handle-safety bugs at near-zero cost. Add **session types (B)** for the cross-boundary coordinator paths (CXL/Raft handshake) where *ordering* matters, not just ownership. C is reserved for the durability-critical WAL-handle sub-protocol if audits demand a proof.

---

## P-P-02: CXL 3.0 Fabric Integration (Single-Rack Coherent Commit)

### Candidate Solution A: CXL.mem shared commit record
- **Approach:** A single 64-byte commit record lives in CXL-shared memory; replicas issue coherent writes; the last writer wins via atomic `cmpxchg16b`.
- **Performance:** CXL memory latency **170–250 ns** (Ruijie 2024; patsnap), ~2× local DRAM. Commit = one CXL round-trip ≈ **200–500 ns**.
- **Time to implement:** ~4 months (shared-memory protocol, coherence fencing).
- **Energy cost:** ≈ **3–5 nJ/commit (est.)** — one CXL traversal ≈ 2× a DRAM access.
- **Upside:** Lowest latency commit; no software consensus for single-rack; CXL 3.0 fabric switching handles routing.
- **Downside:** Requires CXL 3.0 hardware (still maturing); coherence pitfalls (need `MFENCE`/`sfence` discipline); single-fabric failure domain.
- **Key paper:** CXL Consortium, "CXL 3.0 Specification," 2022; Das Sharma, "An Introduction to CXL," ACM (doi 10.1145/3669900), 2024.

### Candidate Solution B: CXL.cache + MFENCE (cache-coherent commit)
- **Approach:** Use CXL.cache so each host caches the commit record and flushes with `mfence`+`clwb` for persistence.
- **Performance:** Cached reads ~100 ns; commit flush ~300–500 ns. Cache-line ping-pong under contention can spike to µs.
- **Time to implement:** ~5 months (cache-line ownership protocol).
- **Energy cost:** ≈ **4–6 nJ/commit (est.)** — flush + coherence traffic.
- **Upside:** Reads are cache-speed (great for read-heavy commit validation).
- **Downside:** False sharing and ping-pong under multi-writer contention; flush ordering is subtle.
- **Key paper:** Das Sharma, ACM 2024; CXL 3.0 spec.

### Candidate Solution C: Software emulation over RDMA (no CXL fabric)
- **Approach:** Emulate coherent commit via RDMA RC writes + a leader, sidestepping CXL fabric entirely.
- **Performance:** ~5–6 µs (RoCEv2) — **20–30× slower** than CXL.
- **Time to implement:** ~2 months (reuses Raft path).
- **Energy cost:** ≈ **50–100 nJ/commit (est.)** — NIC + CPU.
- **Upside:** No CXL hardware dependency; portable; reuses P-P-03.
- **Downside:** Loses the whole point of single-rack CXL; latency in µs not ns.
- **Key paper:** Dragojević et al., FaRM, NSDI 2014.

### Recommendation
**CXL.mem shared commit (A).** It is the design that justifies having CXL at all: ~200–500 ns commits, ~3 nJ/commit. Use `cmpxchg16b` on a 16-byte slot for the commit word and a separate per-replica latch. Keep (C) as the fallback for racks without CXL 3.0 silicon. B only if profiling shows commit validation is read-dominated.

---

## P-P-03: Raft over RoCEv2 (Cross-Rack Consensus)

### Candidate Solution A: openraft + libibverbs (Rust RDMA transport)
- **Approach:** Rust `openraft` core with a custom RDMA transport using `libibverbs` RC QPs; log entries shipped via RDMA write-with-imm.
- **Performance:** RDMA write latency **~1–2 µs** (FaRM NSDI 2014; TPDS 2019); end-to-end Raft 1-RTT append ≈ **5–10 µs**, throughput ~1–2 M entries/s.
- **Time to implement:** ~5 months (RDMA transport, QP management, verbs-async integration).
- **Energy cost:** ≈ **50–80 nJ/entry (est.)** — NIC + CPU for the verbs path.
- **Upside:** Reuses a maintained Raft implementation; Rust safety; one transport to maintain.
- **Downside:** openraft's API assumes a TCP-like transport — shoehorning RDMA zero-copy needs care; libibverbs is C, FFI friction.
- **Key paper:** Ongaro & Ousterhout, "In Search of an Understandable Consensus Algorithm (Raft)," USENIX ATC 2014; Kalia et al., "Using RDMA Efficiently," SIGCOMM 2014.

### Candidate Solution B: Custom RDMA Raft (write-to-log, read-from-memory)
- **Approach:** Raft leader writes entries directly into followers' memory via RDMA (FaRM-style); followers poll the log region — no per-entry CPU interrupt.
- **Performance:** Append ≈ **2–4 µs**, throughput ~5–10 M entries/s (FaRM hit ~75 M msgs/s for small writes).
- **Time to implement:** ~8 months (log replication protocol, membership change, leader election over RDMA).
- **Energy cost:** ≈ **30–50 nJ/entry (est.)** — polling core tax (~15 W) amortized over high throughput.
- **Upside:** Highest throughput/latency; truly zero-copy; no syscall per entry.
- **Downside:** Major engineering investment; RDMA reliability edge cases; one polling core per follower.
- **Key paper:** Dragojević et al., FaRM, NSDI 2014; Kalia et al., SIGCOMM 2014.

### Candidate Solution C: Spanner-style TrueTime over RoCEv2
- **Approach:** Skip consensus for reads; use TrueTime + commits within ε uncertainty.
- **Performance:** Read latency ~1 RTT; commit-wait ~7 ms (Spanner OSDI 2012) — **far worse for writes** than Raft.
- **Time to implement:** ~7 months (GPS/atomic clock infra is the cost).
- **Energy cost:** High operational overhead (clock appliance, commit-wait CPU).
- **Upside:** Globally consistent snapshots without consensus on reads.
- **Downside:** Requires physical clock appliances; commit-wait kills write latency; overkill for cross-rack (not cross-region).
- **Key paper:** Corbett et al., "Spanner," OSDI 2012.

### Recommendation
**openraft + libibverbs (A) for v1.** It is the pragmatic 5-month path to working cross-rack consensus at ~5–10 µs/entry. Reserve (B) as the v2 performance rewrite if throughput targets demand it — its 2–4 µs/entry and 5–10 M entries/s is the ceiling but the cost is 8 months + an RDMA specialist. C is the wrong scope (TrueTime is a cross-region play, not cross-rack).

---

## P-P-04: Cross-Rack Serialization Format

### Candidate Solution A: ANS + Slepian-Wolf (correlated-source coding)
- **Approach:** Exploit correlation between replicas' states (Slepian-Wolf 1973): encode each replica's log delta against a predictor of the other; ANS for the residual.
- **Performance:** Encode ~1–2 GB/s (ANS-bound); decode ~5–11 GB/s. Best ratio when replicas are highly correlated.
- **Time to implement:** ~6 months (Slepian-Wolf binning + predictor design is research-grade).
- **Energy cost:** ≈ **2–3 nJ/B (est.)**.
- **Upside:** Entropy-optimal for correlated sources — near-theoretical-min bytes on the wire.
- **Downside:** Fragile to decorrelation; encoder state must track inter-replica drift; complex.
- **Key paper:** Slepian & Wolf, "Noiseless coding of correlated information sources," IEEE TIT 19(4), 1973.

### Candidate Solution B: Cap'n Proto
- **Approach:** Zero-copy, struct-is-the-wire-format; no decode step — pointers valid in-place.
- **Performance:** **Effectively memcpy speed** (10–30 GB/s); no allocation. Decode = pointer validation only.
- **Time to implement:** ~1 month (schema + Rust crate).
- **Energy cost:** ≈ **0.3–0.5 nJ/B (est.)** — near-zero compute, just the copy.
- **Upside:** Fastest, simplest, SIMD-friendly; message is directly usable from AVX-512 kernels.
- **Downside:** Verbose on wire (no entropy compression); pointer validation needed for untrusted input.
- **Key paper:** Varda, "Cap'n Proto," 2013 (capnproto.org spec).

### Candidate Solution C: FlatBuffers
- **Approach:** Google's zero-copy format; vtable + offset-based field access.
- **Performance:** ~zero-copy, comparable to Cap'n Proto; field access slightly slower (vtable indirection).
- **Time to implement:** ~1.5 months.
- **Energy cost:** ≈ **0.5 nJ/B (est.)**.
- **Upside:** Mature, broad language support.
- **Downside:** Vtable indirection hurts SIMD streaming; larger per-object overhead than Cap'n Proto.
- **Key paper:** van Oortmersen, "FlatBuffers," Google 2014.

### Recommendation
**Cap'n Proto (B) for the wire format** — zero-copy and memcpy-speed means it won't bottleneck RoCEv2 (~5 µs) and is directly streamable into AVX-512 kernels. Compress the *payload columns* with rANS (P-S-01) before Cap'n Proto framing to get entropy wins without sacrificing decode speed. Slepian-Wolf (A) is a research bet — revisit only if cross-rack bandwidth becomes the dominant cost and replicas are demonstrably correlated.

---

## P-P-05: Distributed Transaction Isolation

### Candidate Solution A: Two-Phase Commit (2PC)
- **Approach:** Coordinator prepares all participants, then commits/aborts. Classic.
- **Performance:** 2 RTT minimum; blocking on coordinator failure; ~10–50 µs per txn over CXL/RDMA.
- **Time to implement:** ~3 months.
- **Energy cost:** ≈ **100–300 nJ/txn (est.)** — multiple network traversals.
- **Upside:** Simple, universal, well-understood; works with any participant set.
- **Downside:** Blocking; coordinator is a bottleneck/SPOF; latency under failures is high.
- **Key paper:** Gray, "Notes on data base operating systems," 1978.

### Candidate Solution B: Calvin (deterministic transactions)
- **Approach:** Pre-determinize a global txn order via a replicated log; replicas execute deterministically → no distributed locks/2PC (Thomson SIGMOD 2012).
- **Performance:** Throughput **2–10× higher** than 2PC under contention; latency = log-replication + execute (one consensus RTT).
- **Time to implement:** ~6 months (deterministic execution layer, txn reordering, lock-free scheduler).
- **Energy cost:** ≈ **30–80 nJ/txn (est.)** — no per-participant prepare phase.
- **Upside:** Eliminates distributed deadlocks; replicas converge by construction; great for high-contention.
- **Downside:** Requires transactions be deterministic & pre-declared; struggles with dynamic participant sets; pre-sequencing latency.
- **Key paper:** Thomson et al., "Calvin," SIGMOD 2012.

### Candidate Solution C: Sagas (compensating transactions)
- **Approach:** Long-running transactions decomposed into a sequence of sub-txns, each with a compensating action; eventual consistency.
- **Performance:** Non-blocking; per-step latency only; throughput high but isolation weak.
- **Time to implement:** ~4 months (saga orchestrator, compensation logic per op).
- **Energy cost:** ≈ **20–50 nJ/step (est.)**.
- **Upside:** No distributed locks; survives partial failures gracefully; fits long workflows.
- **Downside:** Only eventual/isolated-ish consistency; compensation logic is bespoke per operation.
- **Key paper:** Garcia-Molina & Salem, "Sagas," SIGMOD 1987.

### Recommendation
**Pick per participant-set size**, as the problem hints:
- **Single-rack (CXL):** 2PC (A) — coordinator and participants share the CXL fabric, so 2-RTT is ~1 µs total, cheap and simple.
- **Cross-rack, high contention:** Calvin (B) — its determinism turns Raft (P-P-03) into the txn-sequencer for free, paying one consensus RTT but eliminating per-participant locking.
- **Cross-region / long-running:** Saga (C).
Ship (A) first; add (B) when contention profiling demands it.

---

## P-P-06: Consistency Models per Tier (STRONG / READ_COMMITTED / EVENTUAL)

### Candidate Solution A: Per-tier snapshot isolation
- **Approach:** Each tier advertises its consistency level; transactions pick a snapshot at the strongest tier they touch. Snapshots via HLC timestamps (P-P-08).
- **Performance:** Snapshot acquire ~100 ns (HLC read); no extra latency for EVENTUAL tiers.
- **Time to implement:** ~3 months.
- **Energy cost:** ≈ **1–2 nJ/txn (est.)** — HLC stamp + version lookup.
- **Upside:** Simple, compositional, maps directly to the tier hierarchy; well-understood semantics.
- **Downside:** Cross-tier transactions must downgrade to the weakest tier's level — coherence across tiers is implicit.
- **Key paper:** Viotti & Vukolić, "Consistency in Non-Transactional Distributed Storage Systems," ACM Computing Surveys 2015.

### Candidate Solution B: Sheaf gluing (category-theoretic consistency)
- **Approach:** Model each tier's consistency as a sheaf over a topology of replicas; gluing conditions give formal cross-tier guarantees (Robinson 2014).
- **Performance:** The framework is analytical, not runtime — no perf impact beyond a consistency-check pass.
- **Time to implement:** ~9 months (formalization + a checker; research-grade).
- **Energy cost:** ~0 runtime; checker energy only on audit.
- **Upside:** Provably-correct cross-tier composition; catches subtle anomalies (cyclic causality).
- **Downside:** Heavy theory; few practitioners; over-engineered for v1.
- **Key paper:** Robinson, "Sheaves and consistency," 2014; Schultz & Spivak, "Temporal type theory," 2020.

### Candidate Solution C: Linear types to enforce tier-bound access
- **Approach:** Use linear types (P-P-01) to make EVENTUAL-tier handles non-observable from STRONG-tier contexts at compile time.
- **Performance:** Zero runtime cost.
- **Time to implement:** ~5 months (type-level tier tagging).
- **Energy cost:** 0.
- **Upside:** Compile-time guarantee that strong contexts never depend on un-observable event values.
- **Downside:** Constrains expressiveness; type-system complexity; doesn't fully replace runtime snapshotting.
- **Key paper:** Wadler 1990; Viotti & Vukolić 2015.

### Recommendation
**Per-tier snapshot isolation (A)** as the runtime mechanism, **reinforced by linear-typed handles (C)** to prevent the most common misuse (reading an EVENTUAL value into a STRONG decision) at compile time. Sheaf gluing (B) is a research direction, not a v1 — adopt it only if a formal consistency audit is required for certification.

---

## P-P-07: Replication Log Shipping (Async Cross-Region)

### Candidate Solution A: RaptorQ fountain
- **Approach:** Stream the log as RaptorQ symbols; receivers collect any K+ε to reconstruct — tolerates packet loss without retransmit.
- **Performance:** Encode ~0.5–1 GB/s, decode ~0.5 GB/s (cberner 2020). **Throughput-limited** vs the log rate.
- **Time to implement:** ~4 months.
- **Energy cost:** ≈ **8–15 nJ/B (est.)** — sparse matvec is costly.
- **Upside:** Loss-tolerant; no ACK/retransmit round-trips; scales to many subscribers.
- **Downside:** Throughput too low for a hot multi-GB/s log; high memory footprint.
- **Key paper:** Luby, "LT codes," FOCS 2002; Shokrollahi, RFC 6330, 2011.

### Candidate Solution B: RDMA log shipping
- **Approach:** Leader RDMA-writes log records into followers' circular buffers; followers poll.
- **Performance:** ~2–4 µs/record, ~5–10 GB/s over 100/200 GbE; **matches the log rate**.
- **Time to implement:** ~5 months.
- **Energy cost:** ≈ **2–5 nJ/B (est.)** — NIC DMA, low CPU.
- **Upside:** Highest throughput/lowest latency; zero-copy; CPU-light.
- **Downside:** Requires RDMA end-to-end (lossless fabric, DCQCN/PFC tuning); within-region only — cross-region still needs TCP/wan.
- **Key paper:** Dragojević, FaRM, NSDI 2014; Kalia et al., SIGCOMM 2014.

### Candidate Solution C: Kafka-style partitioned log
- **Approach:** Cross-region shipping via a partitioned, replicated log (Kafka/Redpanda) — durable broker between regions.
- **Performance:** ~1–5 GB/s aggregate; per-record latency ~5–20 ms cross-region (WAN-bound).
- **Time to implement:** ~2 months (operate a broker).
- **Energy cost:** ≈ **10–50 nJ/B (est.)** — JVM/broker overhead, disk persistence.
- **Upside:** Robust, operationally proven, handles WAN natively, consumer groups, exactly-once.
- **Downside:** Highest energy/latency; broker is a heavy dependency; not zero-copy into the engine.
- **Key paper:** Kreps et al., "Kafka," NetDB 2011.

### Recommendation
**RDMA log shipping (B) within a region/datacenter**, falling back to **Kafka (C) for true cross-region WAN** (RDMA doesn't span the WAN). RaptorQ (A) is a niche tool — use it only for *fan-out to many lossy subscribers* (e.g., read-replica catch-up over a flaky link), not the primary log path. Tiered: B intra-DC, C inter-region.

---

## P-P-08: Clock Synchronization (for Snapshot Isolation)

### Candidate Solution A: HLC (Hybrid Logical Clocks)
- **Approach:** Each node keeps (physical_ts, logical_ct); stamps piggyback physical time with a logical counter to preserve causality without commit-wait (Kulkarni 2014).
- **Performance:** Stamp ~**tens of ns** (a few comparisons + counter); no blocking. Snapshot reads use HLC as the version key.
- **Time to implement:** ~2 months.
- **Energy cost:** ≈ **0.1–0.5 nJ/stamp (est.)** — negligible.
- **Upside:** Drop-in for NTP; no commit-wait; monotonic; causally correct; survives NTP kinks.
- **Downside:** Not externally consistent without bounded clock error; relies on NTP/PTP skew bound (ε).
- **Key paper:** Kulkarni, Demirbas, et al., "Logical Physical Clocks," 2014 (arXiv); Lamport, "Time, clocks…," CACM 1978.

### Candidate Solution B: TrueTime (Spanner)
- **Approach:** GPS/atomic clocks give ε-bounded uncertainty [TT.now()−ε, TT.now()+ε]; commits wait out ε for external consistency.
- **Performance:** Commit-wait ~**7 ms** in Spanner (OSDI 2012); ε ≈ 1–7 ms.
- **Time to implement:** ~8 months (clock appliances + time-slave daemon).
- **Energy cost:** High — dedicated GPS/atomic appliances + commit-wait CPU; operationally expensive.
- **Upside:** Provably externally consistent; enables lock-free consistent reads.
- **Downside:** Requires physical hardware; commit-wait kills write latency; overkill for single-rack/cross-rack.
- **Key paper:** Corbett et al., "Spanner," OSDI 2012.

### Candidate Solution C: NTP / PTP only
- **Approach:** Rely purely on synchronized physical clocks (NTP ~ms; PTP ~µs–sub-µs with hardware support).
- **Performance:** Stamp = `rdtsc` (~20 ns); but correctness depends on skew bound.
- **Time to implement:** ~1 month.
- **Energy cost:** ≈ **0.05 nJ/stamp (est.)**.
- **Upside:** Cheapest; standard.
- **Downside:** No causal guarantee; clock jumps/backward breaks snapshot ordering; insufficient for STRONG tier alone.
- **Key paper:** Mills, "NTP," RFC 1305; IEEE 1588 (PTP).

### Recommendation
**HLC (A) as the primary clock**, fed by PTP for a tight ε (hardware PTP gives sub-µs skew in a rack). HLC gives causal correctness + snapshot isolation at nanosecond stamp cost with no commit-wait — the right tradeoff for a memory-centric engine. Reserve **TrueTime (B)** for any future cross-*region* STRONG-reads product where external consistency is a hard requirement. Pure NTP/PTP (C) alone is insufficient but is the *input* to HLC.

---

# SUMMARY TABLE

| Problem | Pick | Perf (key #) | Impl (mo) | Energy (est.) |
|---|---|---|---|---|
| S-01 Column compress | Interleaved rANS | 11+ GB/s decode | 3 | ~1.5–2 nJ/sym |
| S-02 Lossy/bounded-err | Lloyd-Max (+PQ opt-in) | 5–10 GB/s | 1.5 | ~0.3 nJ/val |
| S-03 ZNS WAL | io_uring + libzns | 4–5× lower WAF, 57% better lat | 3 | ~3 nJ/B |
| S-04 LSM compaction | Hybrid tiered+leveled | W-amp 4–8× | 5 | ~6–10 nJ/key |
| S-05 Erasure WAL | RS(10,4)+GFNI | 10–20 GB/s encode | 3 | ~0.5–1 nJ/B |
| S-06 Checksum/correct | CRC32C + parity | 30 GB/s detect | 1 | ~0.1 nJ/B |
| S-07 Var-len cells | Bit-pack (+dict/sidecar) | 4–6 GB/s | 2 | ~2–3 nJ/val |
| S-08 Schema-on-read | Per-batch MDL | µs/batch select | 3 | ~5–10 nJ/val enc |
| S-09 4 KB page | (as specified) | 1 CL header | 1 | ~2 nJ header |
| S-10 2 MB/2 GB region | THP+NUMA | ~512× fewer TLB misses | 1.5 | saves 2–5 nJ/access |
| P-01 Linear handles | Rust newtypes (+session types) | 0 runtime | 1.5 | 0 |
| P-02 CXL commit | CXL.mem shared record | 200–500 ns commit | 4 | ~3–5 nJ/commit |
| P-03 Raft/RoCEv2 | openraft + libibverbs | 5–10 µs/entry | 5 | ~50–80 nJ/entry |
| P-04 Cross-rack ser | Cap'n Proto (+rANS cols) | memcpy speed | 1 | ~0.3–0.5 nJ/B |
| P-05 Dist txn | 2PC intra-rack / Calvin cross | 2 RTT / 1 consensus RTT | 3–6 | 100–300 / 30–80 nJ/txn |
| P-06 Consistency/tier | Per-tier snapshot + linear types | 100 ns snapshot | 3 | ~1–2 nJ/txn |
| P-07 Log shipping | RDMA intra-DC / Kafka WAN | 5–10 GB/s | 2–5 | ~2–5 nJ/B |
| P-08 Clocks | HLC over PTP | ~ns stamp | 2 | ~0.1–0.5 nJ/stamp |

# NEXT ACTIONS

1. **Sequence the build** by dependency: S-10 → S-09 (region/page layout) unlock everything; S-03 (ZNS WAL) + P-02 (CXL commit) form the durability spine; then S-01/S-04 (compression + compaction) and P-03 (Raft). Estimated **~18 eng-months** on the critical path if parallelized across 2 engineers.
2. **Prototype to de-risk first:** (a) interleaved rANS AVX-512 kernel vs the existing kernel table — confirm 11 GB/s decode holds for our column distributions; (b) CXL.mem `cmpxchg16b` commit microbench — confirm 200–500 ns and coherence fencing; (c) openraft-over-RDMA smoke test — confirm 5–10 µs entry append.
3. **Open research questions to revisit:** Slepian-Wolf serialization (P-P-04A) once cross-rack bandwidth cost is measured; sheaf-gluing consistency (P-P-06B) if formal certification is required; LDPC (S-06C) only if multi-bit NVMe corruption is observed in the field.
4. **No code changes were made in this wave** — this is a research/design deliverable. The above recommendations feed Wave 3 (implementation specs for the kernel table, zone manager, and protocol coordinator).
