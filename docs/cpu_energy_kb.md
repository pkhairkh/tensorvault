# CPU, Memory & Protocol Energy Knowledgebase

> A working reference for designing database engines that treat **instruction-level
> energy, memory hierarchy, and storage/network protocols as first-class design
> constraints**. Compiled August 2025 from vendor specs, peer-reviewed papers,
> Agner Fog's instruction tables, uops.info, Chips and Cheese, SemiAnalysis, and
> VLDB/ISCA/MICRO papers.
>
> All energy numbers are **per single instruction issue** or **per 64-byte access**
> at ~3 GHz, 1 active core, L1-resident unless noted. Items marked `(est.)` are
> author estimates extrapolated from related measurements. **Trust Intel RAPL for
> ≥10 ms windows; treat AMD RAPL as a model.** True per-instruction energy on an
> OoO core is a modeling abstraction — no current CPU isolates one instruction's
> energy.

---

## Table of Contents

1. [Per-Instruction Energy on Modern x86](#1-per-instruction-energy-on-modern-x86)
2. [Memory Hierarchy: Latency, Bandwidth, Energy](#2-memory-hierarchy-latency-bandwidth-energy)
3. [Storage Devices and Protocols](#3-storage-devices-and-protocols)
4. [Interconnects and Fabrics](#4-interconnects-and-fabrics)
5. [Cache Coherence and NUMA Topology](#5-cache-coherence-and-numa-topology)
6. [SmartNIC / DPU / Computational Storage](#6-smartnic--dpu--computational-storage)
7. [Top-10 Cheapest & Most-Expensive Instructions per Joule](#7-top-10-cheapest--most-expensive-instructions-per-joule)
8. [Practical Rules for DB Engine Design](#8-practical-rules-for-db-engine-design)
9. [Citations](#9-citations)

---

## 1. Per-Instruction Energy on Modern x86

Covers: Intel Ice Lake, Sapphire Rapids, Emerald Rapids; AMD Zen 3, Zen 4, Zen 5.

Latency in cycles (dependent chain); throughput in instructions/cycle (independent ops).

### 1.1 ALU Integer

| Instruction | Ice Lake | Sapphire Rapids | Zen 3 | Zen 4 | Zen 5 | Energy (nJ) |
|---|---|---|---|---|---|---|
| ADD/SUB r64,r64 | 1 / 0.25 | 1 / 0.25 | 1 / 0.25 | 1 / 0.25 | 1 / 0.25 | ~0.1–0.3 |
| IMUL r64,r64 | 3 / 1 | 3 / 1 | 3 / 1 | 3 / 1 | 3 / 1 | ~0.3–0.5 |
| IDIV r64 | 21–35 / n.p. | 18–30 / n.p. | 17–23 / n.p. | 16–22 / n.p. | 16–22 / n.p. | ~1.5–4 (est.) |
| SHL/SHR/SAR r64,imm8 | 1 / 0.5 | 1 / 0.5 | 1 / 0.5 | 1 / 0.5 | 1 / 0.5 | ~0.2–0.4 |
| SHL/SHR/SAR r64,cl | 3 / 1 | 3 / 1 | 2 / 1 | 2 / 1 | 2 / 1 | ~0.3–0.5 |

**Takeaways:**
- ADD/SUB are effectively free — 4/cycle throughput.
- **IDIV is catastrophic** — 16–35 cycles non-pipelined, ~1.5–4 nJ. Replace with reciprocal-multiply or magic-number division when divisor is known.
- Variable shifts (cl) cost 2–3× an immediate shift.

### 1.2 Bit Manipulation (BMI1/BMI2)

| Instruction | Latency / TP (cycles) | Energy (nJ) | Notes |
|---|---|---|---|
| POPCNT r64 | 3 / 1 | ~0.3–0.6 | bloom filters, null-bitmaps, cardinality |
| LZCNT r64 | 3 / 1 | ~0.3–0.6 | — |
| TZCNT r64 | 3 / 1 | ~0.3–0.6 | — |
| BT/BTS/BTR r64,imm | 1 / 1 | ~0.2–0.4 | — |
| PEXT r64 (BMI2) | 3 / 1 | ~0.4–0.7 | **Avoid on Zen/Zen2** (18-cyc µcode, ~250× worse) |
| PDEP r64 (BMI2) | 3 / 1 | ~0.4–0.7 | Same as PEXT |
| PEXT/PDEP on Zen 5 | 3 / 3 | — | **Zen 5: 3 PDEP/PEXT per cycle** |

**Critical:** AMD Zen/Zen+/Zen 2 implement PEXT/PDEP in microcode at ~18-cycle latency. Zen 3+ has hardware units. **Guard BMI2 usage behind a Zen 3+ check.**

### 1.3 SIMD Integer (YMM/ZMM)

| Instruction | Ice Lake | SPR | Zen 4 | Zen 5 | Energy (nJ) |
|---|---|---|---|---|---|
| VPADDB/W/D/Q (YMM) | 1 / 0.5 | 1 / 0.5 | 1 / 0.5 | 2 / 0.5 ⚠ | ~0.3–0.6 |
| VPMULLW (YMM) | 1 / 1 | 1 / 1 | 1 / 1 | 3 / 1 | ~0.5–0.8 |
| VPMULLD (YMM) | 5 / 1 | 5 / 1 | 4 / 1 | 3 / 1 | ~0.6–1.0 |
| VPSLLW/D/Q (YMM, imm) | 1 / 1 | 1 / 1 | 1 / 1 | 2 / 1 | ~0.3–0.6 |

⚠ Zen 5 raised integer-vector ADD latency from 1→2 cycles in exchange for native 512-bit datapath.

**AVX-512 throughput on modern cores:**
- Sapphire Rapids: 1×512-bit VPADDQ/cycle (fuses 2×256-bit units)
- Zen 4: 1×512-bit FMA/cycle (double-pumped 256-bit)
- **Zen 5: native 512-bit, 2×512-bit FADD + 2×512-bit FMA per cycle (4 IPC)**

### 1.4 SIMD Floating-Point

| Instruction | Ice Lake | SPR | Zen 3 | Zen 4 | Zen 5 | Energy (nJ) |
|---|---|---|---|---|---|---|
| VADDPS/PD (YMM) | 4 / 0.5 | 4 / 0.5 | 3 / 0.5 | 3 / 0.5 | 2 / 0.5 | ~0.4–0.8 |
| VMULPS/PD (YMM) | 4 / 0.5 | 4 / 0.5 | 4 / 0.5 | 4 / 0.5 | 3 / 0.5 | ~0.5–0.9 |
| VFMADD231PS/PD (YMM) | 4 / 0.5 | 4 / 0.5 | 4 / 0.5 | 4 / 0.5 | 3 / 0.5 | ~0.6–1.0 |
| VDIVPS (YMM) | 11 / 2 | 10 / 2 | 9–11 / 2.5 | 9–11 / 2.5 | 9–11 / 2.5 | ~2–4 |
| VDIVPD (YMM) | 14 / 4 | 13–14 / 4 | 12–14 / 4.5 | 12–14 / 4.5 | 12–14 / 4.5 | ~3–6 |
| VSQRTPS (YMM) | 12–13 / 3–6 | 12 / 3–6 | 12–15 / 3.5 | 12–15 / 3.5 | 12–15 / 3.5 | ~3–6 |

**Takeaways:**
- VFMADD231 is the workhorse — 2×512-bit FMA/cycle on SPR = 32 FP32 FLOP/cycle/core.
- **VDIV/VSQRT are 10–20× slower than FMA** and not fully pipelined. Use `vrcpps` + Newton-Raphson.
- Zen 5 cut FADD latency 3→2 cycles — only core here to do so.

### 1.5 SIMD Comparison & Mask

| Instruction | Lat / TP | Energy (nJ) | Notes |
|---|---|---|---|
| VPCMPEQQ (YMM) | 1 / 0.5 | ~0.3–0.6 | 8 int64 compares per instr |
| VPCMPGTQ (YMM) | 1 / 0.5 | ~0.3–0.6 | — |
| VPMOVMSKB (YMM→r32) | 3 / 1 | ~0.4–0.7 | vector→int domain cross — common DB bottleneck |
| VPCMPM (AVX-512 masked) | 3 / 0.5 | ~0.5–0.9 | prefer over VPMOVMSKB on AVX-512 |

### 1.6 AVX-512 Specific

| Instruction | SPR | Zen 4 | Zen 5 | Energy (nJ) | Notes |
|---|---|---|---|---|---|
| VPOPCNTDQ (ZMM) | 3 / 1 | n/a | 3 / 1 | ~0.6–1.0 | Vectorized popcount — DB secret weapon |
| VPTERNLOGQ (ZMM) | 1 / 0.5 | 1 / 0.5 | 2 / 0.5 | ~0.4–0.8 | **Fuses any 3-input bitwise truth table** |
| VPMOVM2Q (k→ZMM) | 1 / 1 | 1 / 1 | 2 / 1 | ~0.3–0.6 | — |
| VPMOVQ2M (ZMM→k) | 3 / 1 | 3 / 1 | 3 / 1 | ~0.4–0.7 | — |

**VPTERNLOGQ is the DB secret weapon** — fold multi-predicate logic in one instruction. **VPOPCNTDQ turns vectorized popcount into 1 instruction/lane.**

### 1.7 Memory Operations

| Instruction | Latency | Energy (nJ, L1 hit) | Notes |
|---|---|---|---|
| MOV r64,[mem] (L1 hit) | 4–5 cyc | ~0.5 | per 8-byte load |
| MOV [mem],r64 (store) | — / 1 | ~0.4–0.7 | — |
| PREFETCHh (T0) | ~10+ cyc async | ~0.1–0.2 (hint only) | marginal on modern cores |
| CLFLUSH | ~100+ cyc serial | ~1–3 (est.) | — |
| CLFLUSHOPT | ~10–20 cyc non-serial | ~0.5–1.5 (est.) | **~9× faster than CLFLUSH** |
| CLWB | ~10–20 cyc | ~0.5–1.5 (est.) | write-back without invalidate (pmem) |
| REP MOVSB (ERMS, hot) | ~1 B/cyc | ~0.05 nJ/byte | modern memcpy idiom |

### 1.8 Atomics

| Instruction | Latency (uncontended) | Energy (nJ) | Notes |
|---|---|---|---|
| LOCK ADD [mem],imm | 8–15 | ~1–3 | — |
| LOCK XADD [mem],r | 9–18 | ~1.5–3.5 | — |
| LOCK CMPXCHG [mem],r | 17–25 | ~2–5 | — |
| MFENCE | ~10–20 | ~1–3 | drains store buffer |
| SFENCE | ~5–10 | ~0.5–1.5 | — |
| LFENCE | ~5 | ~0.3–0.8 | — |
| **Split LOCK** | **3,000–10,000** | **~50–200 (est.)** | **Bus-lock serializes whole fabric** |

**Split locks (atomic access crossing a cache line) cost ~3,000–10,000 cycles.** Align all atomics to their natural width.

### 1.9 Branches

| Operation | Latency | Energy (nJ) |
|---|---|---|
| JMP (direct, predicted) | 0 | ~0.05–0.1 |
| Cond. JMP (predicted taken) | ~0–1 | ~0.05–0.15 |
| Cond. JMP (predicted not-taken) | ~0 | ~0.05–0.1 |
| **Mispredicted branch** | **15–21 (flush)** | **~1.5–4 + reissue (est.)** |

**Mispredicts are the #1 silent energy tax in DB hot loops.** A 19-cycle flush at 3 GHz wastes ~5–10 nJ plus the re-execution of speculatively-issued µops. **Prefer branchless code (CMOV, mask blends)** even when latency is similar.

### 1.10 Conversions

| Instruction | Lat / TP | Energy (nJ) |
|---|---|---|
| VCVTDQ2PS (YMM) | 4 / 1 | ~0.5–0.9 |
| VCVTPS2DQ (YMM) | 4 / 1 | ~0.5–0.9 |
| VCVTSS2SD | 4 / 1 | ~0.4–0.7 |

### 1.11 Crypto

| Instruction | SPR | Zen 4 | Zen 5 | Energy (nJ) |
|---|---|---|---|---|
| AESENC (xmm) | 4 / 0.5 | 4 / 1 | 4 / 1 | ~0.6–1.2 |
| AESDEC (xmm) | 4 / 0.5 | 4 / 1 | 4 / 1 | ~0.6–1.2 |
| PCLMULQDQ (xmm) | 5 / 0.5 | 6 / 1 | 6 / 1 | ~0.7–1.3 |
| SHA256RNDS2 (xmm) | 3 / 2 | 3 / 2 | 3 / 2 | ~0.5–1.0 |

---

## 2. Memory Hierarchy: Latency, Bandwidth, Energy

### 2.1 Latency Hierarchy (L1 → Network)

| Tier | Latency | Notes |
|---|---|---|
| Register | <1 ns | — |
| L1 cache | ~1 ns (4 cycles) | per-core private |
| L2 cache | ~3–4 ns (12 cycles) | per-core private |
| L3 cache / Smart Cache | ~10–20 ns (40 cycles) | shared per socket; SPR ~30 GB/s/core |
| HBM (Xeon Max / MI300) | ~100–150 ns | on-package, multi-TB/s |
| Local DDR5 | ~80–100 ns | — |
| Cross-CCD (same socket) | ~80–110 ns | AMD chiplet hop |
| Cross-NUMA same socket | ~120–180 ns | after SNC/NPS partition |
| Cross-socket (UPI / IF) | ~150–250 ns | 1.5–2× local |
| **CXL.mem (best)** | **~140 ns** | Type-3 expansion |
| **CXL.mem (typical)** | **~250 ns** | — |
| **CXL.mem (contended)** | **~350–520 ns** | under load |
| XL-FLASH SCM (read) | ~29 µs | Kioxia FL6 |
| XL-FLASH SCM (write) | ~8 µs | — |
| NVMe PCIe 5.0 (read) | ~10–30 µs | — |
| NVMe-oF / RDMA | ~30–60 µs added | — |
| NVMe-oF / TCP | ~100–200 µs added | — |
| Ethernet RoCEv2 RTT | ~5–10 µs | host-to-host |
| IB NDR RTT | ~1–3 µs | host-to-host |

### 2.2 Bandwidth Hierarchy

| Tier | Bandwidth |
|---|---|
| L1 cache | ~1–2 TB/s per core |
| L2 cache | ~1 TB/s per core |
| L3 cache (aggregate) | ~100–500 GB/s |
| HBM3E (single stack) | ~1.2 TB/s |
| HBM3E (8 stacks, MI300X) | 5.3 TB/s |
| **HBM4 (base, 2025-2026)** | **2.0 TB/s/stack** |
| Xeon Max (4× HBM2E) | ~1.6 TB/s |
| DDR5-6400 (12ch Turin) | ~460 GB/s/socket |
| Apple M2 Ultra (LPDDR5) | 800 GB/s |
| Apple UltraFusion (die-to-die) | >2.5 TB/s |
| UPI (SPR / EMR) | 16 / 20 GT/s/link (~50 GB/s) |
| Infinity Fabric (Genoa) | 51.2 GB/s/link × 4 links |
| **CXL.mem x16 PCIe 5.0** | **~64 GB/s** |
| **CXL.mem x16 PCIe 6.0** | **~128 GB/s** |
| NVLink 4 (H100) | 900 GB/s/GPU |
| PCIe 5.0 x16 | 64 GB/s |
| PCIe 6.0 x16 | 128 GB/s |
| PCIe 7.0 x16 | 256 GB/s |
| NVMe PCIe 5.0 SSD (x4) | ~14 GB/s seq read |
| NVMe PCIe 6.0 SSD (x4) | ~26 GB/s (Micron) |
| Ethernet | 100 / 200 / 400 / 800 Gb/s |
| InfiniBand NDR / XDR | 400 / 800 Gb/s |

### 2.3 Energy Hierarchy (per 64B access)

| Tier | Energy | Notes |
|---|---|---|
| Register access | ~10 pJ (est.) | — |
| L1 hit | ~50–100 pJ (est.) | — |
| L2 hit | ~200–500 pJ (est.) | — |
| L3 hit | ~1–2 nJ (est.) | — |
| **HBM2 access (64B)** | **~2.0 nJ** | 3.97 pJ/bit × 512b |
| HBM3E (64B) | ~1.2–1.5 nJ (est.) | improved pJ/bit |
| HMC (64B) | ~5.4 nJ | 10.48 pJ/bit |
| **DDR5 (64B transfer)** | **~1.8–2.4 nJ (est.)** | lower VDD than DDR4 |
| DDR4 (64B transfer) | ~2.3–3.0 nJ | — |
| DRAM refresh (idle) | ~40% of DRAM energy at large cap | grows with capacity |
| LPDDR5X idle | 2–4 mW/GB | Apple M-series |
| Cross-socket DRAM access | ~2–4× local (est.) | UPI/IF link power |
| CXL.mem access | local DRAM + ~5–10 nJ link (est.) | PCIe PHY + retimer |
| NVMe read (64B) | ~1–5 µJ (est.) | dominated by controller + NAND |
| 400G optical link (per bit) | ~5–15 pJ/bit (est.) | 15–20 W / 50 GB/s |
| 100G optical link (per bit) | ~10–25 pJ/bit (est.) | 10–15 W / 12.5 GB/s |

### 2.4 DRAM Technology Comparison

| Parameter | DDR4 | DDR5 | LPDDR5X | HBM3E | HBM4 (2025+) |
|---|---|---|---|---|---|
| Data rate | 3200 MT/s | 4800–6400 | 8533–10.7 Gb/s | 9.2–9.6 Gb/s | 8–16 Gb/s |
| VDD | 1.2 V | 1.1 V | 1.05/0.4 V | ~1.1 V | ~1.0 V |
| Per-channel BW | 25.6 GB/s | 38.4–51.2 | — | 1.2 TB/s/stack | 2.0–3.3 TB/s/stack |
| Max DIMM capacity | 64 GB | 512 GB | — | 36 GB/stack | 24–128 GB/stack |
| Interface width | 64b (1 subch) | 64b (2 subch) | — | 1024b | **2048b** |
| Idle power | moderate | lower | 2–4 mW/GB | — | — |
| Energy/bit | ~3-4 pJ | ~2-3 pJ | ~1-2 pJ | ~2.5-3 pJ | ~2 pJ |

### 2.5 CXL Memory — Reality Check

CXL is the cache-coherent interconnect built on PCIe PHY. CXL.mem (Type-3) lets a host CPU load/store from a memory expansion device as if it were local DRAM, with hardware-managed coherence.

| Mode | Latency | Bandwidth | Use case |
|---|---|---|---|
| Local DDR5 (baseline) | ~80–100 ns | ~25 GB/s/DIMM | hot working set |
| NUMA-remote DDR5 | ~130–180 ns | shared | cross-socket access |
| CXL.mem (best case) | ~140 ns | 64 GB/s per x16 PCIe 5.0 | buffer-pool extension |
| CXL.mem (contended) | ~350–520 ns | shared | overloaded pool |

**CXL 2.0 → 3.0 → 3.1:**
- 2.0: PCIe 5.0, single-level switch, single-level pooling
- 3.0: PCIe 6.0 (128 GB/s x16), multi-level switching, fabric, P2P DMA
- 3.1: same PHY, enhanced pooling, shared memory with peer copy

**Caveat:** SemiAnalysis argues "CXL is dead in the AI era" — AI prefers HBM near compute. **CXL's growth is in memory expansion for general-purpose DB workloads**, which is what we care about.

---

## 3. Storage Devices and Protocols

### 3.1 NVMe over PCIe Generations

| PCIe gen | Signaling | x4 BW (duplex) | x16 BW (duplex) | Best SSD seq read | Typical NVMe latency |
|---|---|---|---|---|---|
| 3.0 | NRZ | ~4 GB/s | ~16 GB/s | ~3.5 GB/s | 50–100 µs |
| 4.0 | NRZ | ~8 GB/s | ~32 GB/s | ~7.5 GB/s | 30–80 µs |
| 5.0 | NRZ | ~16 GB/s | ~64 GB/s | ~14 GB/s | 10–30 µs |
| 6.0 | PAM4 | ~32 GB/s | ~128 GB/s | ~26 GB/s (Micron) | ~10–25 µs (est.) |
| 7.0 | PAM4 | ~64 GB/s | ~256 GB/s | not yet shipping | +~100 ns FEC |

**PCIe 6.0 PAM4 introduces ~100 ns FEC latency** — relevant for fast-storage access paths.

### 3.2 NVMe over Fabrics

| Transport | Added latency | CPU overhead | Use case |
|---|---|---|---|
| NVMe/RDMA (RoCEv2 or IB) | ~10–30 µs | lowest (kernel bypass) | highest-perf shared flash |
| NVMe/TCP | ~100–200 µs | moderate | commodity Ethernet |
| NVMe/FC | ~tens of µs | moderate | legacy FC SAN |

### 3.3 NAND Types and Endurance

| NAND | Bits/cell | P/E cycles | Relative cost | Write perf | Use case |
|---|---|---|---|---|---|
| SLC | 1 | 50,000–100,000 | 4× | fastest | enterprise write cache, ZNS |
| MLC | 2 | 3,000–10,000 | 2× | fast | enterprise (legacy) |
| TLC | 3 | 1,500–3,000 | 1× | good | mainstream NVMe |
| QLC | 4 | 100–1,000 | 0.6× | slow | read-heavy, bulk, archive |

**Write amplification factor (WAF):** QLC under random 4K writes can exceed 4–8×; sequential writes approach ~1.0× (esp. with ZNS).

### 3.4 Zoned Namespaces (ZNS)

ZNS exposes SSDs as **zones that must be written sequentially**, shifting FTL responsibility partially to the host. Benefits:
- **Reduces write amplification** by aligning writes to NAND erase blocks
- **Predictable performance** — no GC-induced tail latency
- **Higher usable capacity** (less over-provisioning), longer endurance

**For DB engines:** ZNS is a natural fit for **WAL, LSM-tree compaction streams, append-mostly structures**. RocksDB, Cassandra, ScyllaDB have ZNS integrations.

### 3.5 Computational Storage

| Product | Compute | Offload capability |
|---|---|---|
| Samsung SmartSSD (gen 2) | Xilinx Versal Adaptive SoC | scan-heavy DB queries 50%+ faster; up to 97% offload |
| ScaleFlux CSD | integrated ARM cores | transparent inline compression, in-drive compute |
| NGD Systems | multiple 64-bit ARM cores | in-situ processing, analytics near data |
| Kioxia XL-FLASH (FL6) | — | 29 µs read, 8 µs write latency, 60 DWPD — post-Optane SCM |

**Offload-able ops:** predicate pushdown (scan/filter), projection, aggregation, simple joins, compression/decompression, encryption, erasure coding, RAID.

**Market reality:** Small but growing — $4.2B (2025) → $22.6B (2034), CAGR 20.5%.

### 3.6 Persistent Memory — Post-Optane

Intel cancelled Optane products in 2022. Optane 200-series ships through 2025. No commercial byte-addressable persistent memory replacement exists today.

**For crash-consistency without Optane:** use NVMe with persistent write barriers + FUA + AIO, or CXL.mem with explicit flush + persistent journaling on NVMe. Kioxia XL-FLASH bridges latency (29 µs read) but is block-addressed.

---

## 4. Interconnects and Fabrics

### 4.1 Vendor Proprietary Fabrics

| Vendor | Interconnect | Bandwidth | Use |
|---|---|---|---|
| Intel | UPI | 16 GT/s (SPR), 20 GT/s (EMR) | inter-socket cache-coherent |
| AMD | Infinity Fabric (GMI3) | 51.2 GB/s/link × 4 links | chiplet + inter-socket |
| Apple | UltraFusion | >2.5 TB/s inter-die | silicon interposer, >10,000 signals |
| NVIDIA | NVLink 4.0 / 5.0 | 900 / 1,800 GB/s/GPU | GPU-only |
| CXL Consortium | CXL 3.1 | 128 GB/s x16 (PCIe 6.0) | open cache-coherent |

**CCIX and Gen-Z are dead** — both merged into CXL. **CXL is the single industry-standard cache-coherent interconnect.**

### 4.2 Ethernet and RDMA

| Speed | Per-port power (with optics) | Status |
|---|---|---|
| 100G | 10–15 W | mature |
| 200G | 12–18 W | mature |
| 400G | 15–20 W | mainstream 2025 |
| 800G | ~24 W (OSFP800) | ramping |
| 1.6T | ~30–40 W (est.) | emerging |

**RoCEv2** provides RDMA over Ethernet with kernel bypass; **DCQCN** congestion control is required to approach IB-class behavior. **Spectrum-X** (NVIDIA) is Ethernet tuned for lossless RoCEv2.

### 4.3 InfiniBand

| Gen | Speed | Typical latency | Use |
|---|---|---|---|
| HDR | 200 Gb/s | ~1 µs | legacy HPC |
| NDR | 400 Gb/s | ~1–2 µs end-to-end | modern HPC/AI |
| XDR | 800 Gb/s | ~1 µs | NVIDIA AI factories |

**IB vs RoCEv2:** IB has lower latency (NDR p99.9 within 2–3× of base; RoCEv2 Ethernet spikes more), NDR delivers ~350 GB/s effective vs 270–290 GB/s for 400GbE RoCEv2 (~20–25% gap). RoCEv2 typically adds ~5–6 µs over IB.

**When IB beats Ethernet:** ultra-low-jitter workloads (collective MPI/NCCL ops, distributed training, very-low-latency OLTP replication). **When Ethernet wins:** cost, broader ecosystem, RDMA convergence, general-purpose DB clusters.

### 4.4 CXL Fabric vs Ethernet Fabric

- **CXL fabric:** cache-coherent, memory semantics, lowest latency for memory pooling, but limited to short-reach (rack-scale), ~hundreds of ns.
- **Ethernet/RDMA fabric:** longer reach, much higher port density, but ~µs-class latency and no hardware coherence.
- **Practical pattern:** **CXL within rack, RDMA/Ethernet across racks.**

---

## 5. Cache Coherence and NUMA Topology

### 5.1 Coherence Protocols

| Protocol | States | Owner | Use |
|---|---|---|---|
| **MESI** | M, E, S, I | — | textbook baseline |
| **MOESI** | + O (Owned) | AMD | Owned allows dirty data to be read directly from another cache |
| **MESIF** | + F (Forward) | Intel | Forward state designates which S-state cache answers reads |

- **Snooping (bus broadcast):** simple, low-latency, doesn't scale past ~8 cores.
- **Directory-based:** tracks sharer set per block; scales to hundreds of cores; adds indirection latency and storage overhead (~6–16 bits/block).

**Modern servers (SPR, Genoa, Turin) use directory-based coherence** with snoop filters to suppress unnecessary broadcasts.

### 5.2 Intel Xeon Topology (Sapphire Rapids / Emerald Rapids)

- Up to 60 cores (SPR) / 64 cores (EMR) in tiled EMIB package.
- **UPI:** up to 3 links/socket at 16 GT/s (SPR), 20 GT/s (EMR).
- **SNC (Sub-NUMA Clustering):** SNC-2 / SNC-4 / SNC-6 partition the chip into NUMA nodes.
- **Xeon 6 (Granite Rapids):** SNC-3, 160 MB LLC; single-core L3 BW ~30 GB/s.
- Per-socket DDR5 BW: ~384–460 GB/s.
- **HBM variant (Xeon Max):** 64 GB HBM2E on-package, ~1.6 TB/s sustained.

### 5.3 AMD EPYC Topology (Zen 4 Genoa / Zen 5 Turin)

**Genoa (Zen 4):**
- Up to 96 cores, 12 chiplets (CCDs) on central I/O die (IOD).
- Infinity Fabric (GMI3): 3.2 GHz × 16 B = 51.2 GB/s per link; 4 links CCD↔IOD, 4 links inter-socket.
- 12-channel DDR5-4800; per-socket BW ~460 GB/s.
- **NPS modes:** NPS1 (whole socket), NPS2, NPS4 — partition memory controllers per CCD group.
- Cross-CCD: ~80–110 ns. Cross-NUMA: ~130 ns. Cross-socket: ~150–220 ns.

**Turin (Zen 5):**
- Up to **192 Zen 5 (4 nm) or Zen 5c (3 nm) cores**.
- 12-channel DDR5-6000 (up to DDR5-6400).
- Single 9575F core pulls ~52 GB/s memory read, 48 GB/s write, 95 GB/s RMW.
- NPS4 divides into four NUMA domains.
- CXL 2.0 support.

### 5.4 Apple Silicon

| Chip | Memory tech | Bus width | Bandwidth | Max capacity |
|---|---|---|---|---|
| M1 | LPDDR4X | 128b | 68 GB/s | 16 GB |
| M1 Pro | LPDDR5 | 256b | 200 GB/s | 32 GB |
| M1 Max | LPDDR5 | 512b | 400 GB/s | 64 GB |
| M1 Ultra (UltraFusion) | LPDDR5 | 1024b | 800 GB/s | 128 GB |
| M2 Ultra | LPDDR5 | 1024b | 800 GB/s | 192 GB |
| M3 Max | LPDDR5X | 512b | 400 GB/s | 128 GB |
| M4 Max | LPDDR5X | 512b | 546 GB/s | 128 GB |

UltraFusion die-to-die interposer provides **>2.5 TB/s low-latency inter-die bandwidth** across >10,000 signals. No M-series "Extreme" (4-die) product has shipped.

### 5.5 Cross-tier Latency Summary

| Path | Latency | Energy multiplier vs local |
|---|---|---|
| L1 hit | ~1 ns | 1× |
| L2 hit | ~3–4 ns | ~3× |
| L3 hit | ~10–20 ns | ~10–20× |
| Local DDR5 | ~80–100 ns | ~100–200× |
| Cross-CCD same socket | ~80–110 ns | ~120–250× |
| Cross-NUMA same socket | ~120–180 ns | ~200–400× |
| **Cross-socket (UPI/IF)** | **~150–250 ns** | **~2–4× local DRAM** |
| CXL.mem (typical) | ~250 ns | ~3–5× local DRAM |
| CXL.mem (contended) | ~500 ns | ~5–10× local DRAM |
| NVMe PCIe 5.0 | ~10–30 µs | ~1000×+ local DRAM |

---

## 6. SmartNIC / DPU / Computational Storage

### 6.1 DPU/SmartNIC Landscape

| Product | Cores | Network | Memory | Offload |
|---|---|---|---|---|
| NVIDIA BlueField-2 | 8× Arm A72 | 200 Gb/s | 16 GB DDR4 | basic |
| **NVIDIA BlueField-3** | 16× Arm A78 | 400 Gb/s | 16 GB DDR5 | "equivalent of 300 CPU cores" of offload |
| BlueField-4 (SuperNIC) | Arm + custom ASIC | 800 Gb/s | — | AI/cloud-scale |
| Intel IPU E2100 | — | — | — | virt storage, security isolation |
| **AMD Pensando Salina** | P4-programmable | — | — | up to 1.45× perf over BF-3 |
| AWS Nitro | custom ASICs per function | — | — | hardware encryption, lightweight hypervisor |

### 6.2 DB Operations Offload-able to DPU/SmartNIC

| Operation | Offload target | Energy benefit |
|---|---|---|
| Predicate pushdown (scan filter) | DPU / CSD | high (data stays near) |
| TLS / IPsec termination | DPU hardware | **5–10× more efficient than Xeon core** |
| NVMe-oF target / initiator | DPU | **~10× reduction in host CPU cycles** |
| Compression/decompression | DPU ASIC | 3–5× |
| RAID / erasure coding | DPU | off-CPU |
| Network protocol (TCP/RoCE) | DPU / kernel bypass | high |
| Distributed transaction coordination | experimental (DPDK-based Paxos/Raft) | medium |
| Replication / log shipping | DPU | can avoid host CPU entirely |
| Wire-encryption for cross-AZ replication | DPU TLS | line-rate |

**Energy math:** A BlueField-3 consumes ~120 W but can replace the work of 100–300 Xeon cores worth of pure infra offload (NVIDIA's claim). For DB-relevant workloads: TLS termination 5–10× more energy-efficient on DPU; compression 3–5×; NVMe-oF 10× reduction in host CPU cycles.

---

## 7. Top-10 Cheapest & Most-Expensive Instructions per Joule

### 7.1 The 10 Cheapest Instructions per Joule on Modern x86

| Rank | Instruction | Lat / TP (cyc) | Energy/issue (nJ) | Why it wins |
|---|---|---|---|---|
| 1 | **VPTERNLOGQ** (ZMM) | 1 / 0.5 | ~0.4 | Fuses 3-input logic; 1 instr ≈ 3 bitwise ops |
| 2 | **VFMADD231PS** (ZMM) | 3–4 / 0.5 | ~0.6 | 2 FP ops/instr, 2/cycle |
| 3 | **VPADDQ** (YMM) | 1 / 0.5 | ~0.4 | 8 int adds/instr |
| 4 | **ADD r64,r64** | 1 / 0.25 | ~0.15 | 4/cycle, trivial logic |
| 5 | **VPCMPEQQ** (YMM) | 1 / 0.5 | ~0.4 | 8 comparisons/instr |
| 6 | **IMUL r64,r64** | 3 / 1 | ~0.4 | Dedicated multiplier |
| 7 | **POPCNT r64** | 3 / 1 | ~0.4 | Single-cycle-issued bit count |
| 8 | **REP MOVSB** (ERMS) | ~1 B/cyc | ~0.05/B | Microcoded bulk copy |
| 9 | **VPSLLD** (YMM, imm) | 1 / 1 | ~0.4 | 8 shifts/instr |
| 10 | **AESENC** (xmm) | 4 / 0.5 | ~0.8 | Dedicated AES unit |

### 7.2 The 10 Most Expensive Instructions per Joule on Modern x86

| Rank | Instruction | Lat / TP (cyc) | Energy/issue (nJ) | Why it hurts |
|---|---|---|---|---|
| 1 | **Split LOCK CMPXCHG** | 3,000–10,000 / n.p. | ~50–200 (est.) | Bus-lock serializes whole fabric |
| 2 | **Mispredicted branch** | 15–21 (flush) | ~2–4 + reissue | Wastes speculated µops + pipeline refill |
| 3 | **CLFLUSH** (serializing) | 100+ / serial | ~1–3 (est.) | Forces writeback+invalidate, ordered |
| 4 | **IDIV r64** | 16–35 / n.p. | ~1.5–4 (est.) | Non-pipelined iterative divider |
| 5 | **VSQRTPD** (YMM) | 14–18 / 4–8 | ~4–8 | Iterative sqrt, poor throughput |
| 6 | **VDIVPD** (YMM) | 12–14 / 4 | ~3–6 | Iterative divide |
| 7 | **LOCK CMPXCHG** (contended) | 100s–1000s | ~5–50 (est.) | Coherence traffic + backoff |
| 8 | **MFENCE** | 10–20 / serial | ~1–3 | Drains store buffer, full barrier |
| 9 | **PEXT/PDEP on Zen/Zen2** | ~18 / ~0.004 | ~3–6 | Microcoded; ~250× worse than hw impl |
| 10 | **VPCMPESTRM** | ~11 / ~3 | ~1.5–3 (est.) | Legacy string op, slow & complex |

### 7.3 Energy-Efficiency Ranking (cheapest → priciest per useful op)

1. VPTERNLOGQ (1 instr fuses a truth table — ~3 ops in 1)
2. VFMADD231PS (2 FP ops in 1 instr, 0.5 TP)
3. ADD/VPADDQ (4/cycle)
4. IMUL (3 cyc, 1/cycle)
5. POPCNT/LZCNT/TZCNT (1 useful bit-reduction op)
6. VPCMPEQ + mask (16 comparisons/instr)
7. REP MOVSB (1 B/cyc bulk copy)
8. PEXT/PDEP (on Zen 3+/Intel — complex bit extract in 3 cyc)
9. AESENC (dedicated crypto unit)
10. VCVTDQ2PS (conversion)
11. LOCK ADD (uncontended)
12. VDIVPS (10–20× FMA)
13. VSQRTPS (worse than DIV)
14. IDIV (non-pipelined, 16–35 cyc)
15. Mispredicted branch (15–21 cyc flush)
16. CLFLUSH (serializing, ~100+ cyc)
17. Split LOCK (~3,000–10,000 cyc)

---

## 8. Practical Rules for DB Engine Design

### 8.1 The 12 Hard Rules

1. **Treat memory as a hierarchy, not a tier.** Latency gap between L3 (~15 ns) and cross-socket DRAM (~200 ns) is >10×; to CXL-contended (~500 ns) is >30×; to NVMe (~20 µs) is >1000×. **Data placement must be explicit and workload-aware.**

2. **CXL.mem is real, but not "DRAM-but-cheaper."** Real latency is ~140 ns best to ~520 ns contended, 1.2–2.1× local DRAM. Use as buffer-pool extension or second-tier DRAM, **not for hot indexes.**

3. **HBM-class memory is now available to CPUs** (Xeon Max, MI300A APU). For DB engines that can pin hot working sets, this is **5–10× local-DRAM bandwidth at ~1.6–5.3 TB/s**. Ideal for hash joins, sorts, large in-memory scans.

4. **NUMA awareness is non-optional.** Cross-socket is 1.5–2× local latency and 2–4× energy. **NPS / SNC partitioning** must be reflected in the buffer pool's page placement and worker-thread affinization.

5. **NVMe is the storage baseline; SATA/SAS are legacy.** PCIe 5.0 NVMe gives ~14 GB/s and ~10–30 µs latency. **ZNS should be the WAL/LSM device** — eliminates GC tail latency and cuts write amplification dramatically.

6. **Computational storage is a real lever for scan-heavy OLAP.** Samsung SmartSSD reports 50%+ scan-query speedup. Predicate + projection pushdown to CSDs reduces host bandwidth pressure and energy-per-query.

7. **DPUs/SmartNICs belong in the data path for networked DB.** Offload TLS, NVMe-oF target, compression, replication to BlueField-3 / Pensando / Nitro. Frees host cores for query execution and reduces tail latency on replication.

8. **Memory disaggregation via CXL is the 2025-2026 architectural frontier for scale-up DB.** Rack-as-shared-memory with fine-grained resource scaling. Engine needs: CXL-aware buffer pool, page placement/migration policies, coordination protocol tolerating CXL's variable tail latency, telemetry for CXL link contention.

9. **The Optane gap is not filled.** No commercial byte-addressable persistent memory ships today. Use **NVMe with persistent write barriers + FUA + AIO**, or **CXL.mem with explicit flush + persistent journaling on NVMe**.

10. **Energy is a first-class design constraint.** DRAM refresh at 256 GB+ can be ~40% of idle DRAM energy. Cross-socket access is 2–4× local. DPU offload can be 5–10× more efficient than host CPU for crypto/compression. **Minimize data movement — push compute to data.**

11. **Network fabric choice depends on reach and coherence needs.** Within rack: **CXL 3.0 fabric** for coherent memory sharing, **NVMe-oF/RDMA** for shared block. Across racks: **RoCEv2 400G or IB NDR** — IB wins on latency/jitter for sync-replication; RoCEv2 wins on cost/ecosystem.

12. **For a next-generation DB engine specifically:** design a **3-tier volatile memory pool** (L3/LLC → local DDR5 → CXL.mem), a **2-tier persistent pool** (ZNS NVMe for WAL/LSM, QLC NVMe for cold data), a **DPU-offloaded network/replication plane**, and **computational-storage pushdown** for OLAP scans. Make every page-aware placement decision driven by telemetry on access frequency, latency sensitivity, and energy budget.

### 8.2 SIMD Amortization Break-even

| SIMD width | Min batch size (elements) | Rule |
|---|---|---|
| SSE2 (128-bit, 4×int32) | ~8–16 | Marginal; often not worth branchless setup |
| AVX2 (256-bit, 8×int32) | ~32–64 | Clear win for column scans |
| AVX-512 (512-bit, 16×int32) | ~64–128 | Best; watch Zen 5 2-cyc latency needs ILP |

**Industry standard:** ClickHouse, StarRocks, DuckDB process **1024–4096 rows per batch** — well past break-even, so SIMD is essentially free per element.

### 8.3 Energy Efficiency Sweet Spot

- CPU energy-to-solution is weakly sensitive to frequency scaling for compute-bound phases (switching power dominates).
- **Memory-bound phases benefit from lower frequency** (less wasted core power while waiting on DRAM).
- **Sweet spot: ~60–75% of peak boost** — e.g., ~2.5–3.0 GHz on Zen 2 EPYC.
- For DB scans: cap frequency at ~70% of boost on memory-bound scans; let turbo run on compute-bound SIMD kernels.

### 8.4 SMT Behavior

- SMT2/SMT4 generally **improves energy-per-operation** (fills pipeline stalls with useful work).
- Rule of thumb: **SMT ~+30–40% perf for +5–10% power** → ~20–30% better perf/W.
- **Caveat for DB:** SMT siblings share L1D/L2 and execution ports; memory-heavy sibling can starve its partner. For latency-critical OLTP, pin one DB worker per physical core (SMT off or 1-thread-per-core) for predictable tail latency.

### 8.5 DB Engine Hot-Loop Guidance

| Category | Worth using? | Notes |
|---|---|---|
| ADD/SUB/IMUL | Yes — IMUL is cheap (3 cyc). | Avoid IDIV; use magic-number division. |
| Shifts (imm8) | Yes. | Avoid `cl` variable shifts where compile-time amount works. |
| POPCNT/LZCNT/TZCNT | **Absolutely** — bloom filters, null-bitmaps, cardinality. | |
| PEXT/PDEP | **Yes on Intel & Zen 3+.** ❌ Avoid on Zen/Zen2. | Guard with CPUID. |
| VPADD/VPMULL (SIMD int) | Yes for batch filters/projections. | |
| VFMADD231 | Yes — cheapest multiply-add. Saturate 2/cycle on SPR/Zen5. | |
| VDIV/VSQRT | ❌ Avoid. Use `vrcpps` + Newton-Raphson or reciprocal tables. | |
| VPCMPEQQ/VPCMPGTQ | Yes for vectorized comparisons. | |
| **VPTERNLOGQ** | **Yes — secret weapon** for folding multi-predicate logic. | |
| **VPOPCNTDQ** | Yes (AVX-512_VPOPCNTDQ) for vectorized popcount over column chunks. | Zen 5 + SPR/EMR only. |
| MOV load/store | Unavoidable — **batch** to amortize L1 access energy. | |
| PREFETCHh | Marginal on modern cores. Use sparingly; mis-prefetch wastes ~0.1–0.2 nJ + bandwidth. | |
| CLFLUSHOPT/CLWB | Only for pmem/NUMA-aware flush loops. | CLFLUSHOPT ~9× faster than CLFLUSH. |
| LOCK atomics | Minimize. Uncontended ~10–25 cyc; contended much worse. | Use lock-free structures sparingly. |
| Branches | **Minimize in hot loops.** Mispredict = ~15–21 cyc + ~2–4 nJ. | Prefer branchless (CMOV, mask blends). |
| REP MOVSB (ERMS) | **Yes** for memcpy/memmove of column pages — ~1 B/cycle, HW-prefetched. | |
| AESENC/AESDEC | Yes for TLS/at-rest crypto; 4 cyc, 0.5 TP (Intel). | Offload bulk to QAT if available. |
| SHA256RNDS2 | Yes (SPR/Zen4+) for SHA-256 hashing of keys. | |

---

## 9. Citations

### Per-Instruction Energy (x86)

1. Agner Fog — Instruction Tables — https://www.agner.org/optimize/instruction_tables.pdf
2. uops.info — Table — https://uops.info/table.html
3. HN — PDEP/PEXT 300→3 cycle latency — https://news.ycombinator.com/item?id=25000414
4. Encyclopedia MDPI — BMI2 on AMD pre-Zen3 — https://encyclopedia.pub/entry/29595
5. Dougall (Mastodon) — Zen 5 SIMD latency — https://mastodon.social/@dougall/113032935576694427
6. numberworld — Zen5 AVX512 teardown — https://www.numberworld.org/blogs/2024_8_7_zen5_avx512_teardown
7. Agner forum — Zen 5 vector latency — https://www.agner.org/forum/viewtopic.php?t=287
8. Chips and Cheese — Sapphire Rapids — https://chipsandcheese.com/p/a-peek-at-sapphire-rapids
9. SO — CLFLUSHOPT vs CLFLUSH — https://stackoverflow.com/questions/35900401
10. HN — LOCK ADD latency by uarch — https://news.ycombinator.com/item?id=45082719
11. Chips and Cheese — Split locks — https://chipsandcheese.com/p/investigating-split-locks-on-x86
12. Lemire — Zen 2 branch mispredicts — https://lemire.me/blog/2019/12/06/amd-zen-2-and-branch-mispredictions
13. Chips and Cheese — Zen 2 Cinebench analysis — https://chipsandcheese.com/p/analyzing-zen-2s-cinebench-r15-lead
14. ResearchGate — AESENC latency table — https://www.researchgate.net/figure/...tbl1_352348040
15. Intel — AES-NI whitepaper — https://www.intel.eu/content/dam/www/public/us/en/documents/white-papers/aes-breakthrough-performance-paper.pdf
16. NIST (Gueron 2023) — AES round & PCLMULQDQ — https://csrc.nist.gov/csrc/media/Presentations/2023/...

### Power Measurement

17. Thamm & Leser — RAPL measurement strategies (arXiv 2505.09375) — https://arxiv.org/html/2505.09375v2
18. Khan et al. — RAPL in Action — https://researchportal.helsinki.fi/files/151464102/RAPL_in_Action...pdf
19. Hahnel et al. — Short code paths — http://www.sigmetrics.org/greenmetrics/2012/papers/Hahnel.pdf
20. Scaphandre — RAPL domains — https://hubblo-org.github.io/scaphandre-documentation/explanations/rapl-domains.html
21. PRACE — RAPL slides — https://events.it4i.cz/event/39/attachments/150/348/08-2020-01-30-prace-ee-RAPL.pdf
22. NextPlatform — AMD RAPL grapple — https://www.nextplatform.com/hpc/2021/08/09/hpc-efficiency-gurus-grapple-with-amds-rapl/1638958
23. Schöne et al. — Zen 2 energy efficiency — https://greencompute.uk/References/Cluster_2021/Schone...pdf
24. ScienceDirect — single-threaded SMT power — https://www.sciencedirect.com/science/article/pii/S0743731525000851
25. Chips and Cheese — Alder Lake caching & power — https://chipsandcheese.com/p/alder-lakes-caching-and-power-efficiency

### CPU Power & Frequency

26. WCCFtech — SPR 900W OC — https://wccftech.com/intel-sapphire-rapids-xeon-workstation-cpus-rumored-to-hit-over-900w...
27. Level1Techs — EPYC Genoa idle power — https://forum.level1techs.com/t/ryzen-vs-epyc-idle-power-consumption/198577
28. LLVM issue — AVX-512 no downclock SPR/Zen4 — https://github.com/llvm/llvm-project/issues/102047
29. LinkedIn — Skylake AVX-512 tiers & SPR — https://www.linkedin.com/posts/taras-tsugrii-8117a313...
30. Chips and Cheese — Zen 5 AVX-512 frequency — https://chipsandcheese.com/p/zen-5s-avx-512-frequency-behavior
31. Reddit — Zen efficiency at ~3 GHz — https://www.reddit.com/r/Amd/comments/agnzzi
32. arXiv — Energy-efficiency sweet spots — https://arxiv.org/html/2607.00819v1
33. AMD — SMT perf & efficiency — https://www.amd.com/en/blogs/2025/simultaneous-multithreading-driving-performance-a.html
34. Green-coding.io — Hyper-threading & energy — https://www.green-coding.io/case-studies/hyper-threading-and-energy
35. Tom's Hardware — SMT perf/W — https://forums.tomshardware.com/threads/3305796
36. Kim et al. (Georgia Tech) — PIM energy params — https://hparch.gatech.edu/papers/kim_memsys15.pdf

### Academic Foundations

37. Tiwari et al. 1994 — Power analysis of embedded SW — https://ziyang.eecs.umich.edu/talp/papers/tiwari-instruction-power.pdf
38. Lee et al. — Accurate ILP energy model — https://www.es.mdu.se/pdf_publications/832.pdf
39. Bircher & John — Complete system power — https://lca.ece.utexas.edu/pubs/bircher-TC2012.pdf
40. Hirki et al. — x86-64 decoder power — https://www.usenix.org/system/files/conference/cooldc16/cooldc16-paper-hirki.pdf
41. Desrochers et al. — DRAM RAPL validation — https://dl.acm.org/doi/10.1145/2989081.2989088
42. Shao — ILP energy modeling — https://people.eecs.berkeley.edu/~ysshao/assets/papers/shao2013-islped.pdf
43. Kumar & Gerstlauer — Learning-based CPU power — https://slam.ece.utexas.edu/pubs/tc22.LACPo.pdf
44. PANDA — Architecture-level CPU modeling — https://zhiyaoxie.com/files/TCAD25_PANDA.pdf
45. Dissecting RAPL measurement — https://hal.science/hal-04420527v1/file/all_together.pdf
46. ClickHouse — Vectorized query execution — https://clickhouse.com/resources/engineering/vectorized-query-execution

### Memory & DRAM

47. ATP Electronics — DDR5 datasheet specs — https://www.atpinc.com/blog/ddr5-datasheet-key-specs
48. ITU Online — DDR4 vs DDR5 — https://www.ituonline.com/blogs/ddr4-vs-ddr5
49. Fly-Wing — DDR4 vs DDR5 — https://www.flywing-tech.com/blog/ddr4-vs-ddr5-embedded-ai
50. ADATA — DDR4 vs DDR5 — https://www.adata.com/us/quikTips/differences-between-ddr4-and-ddr5
51. Wikipedia — DDR5 SDRAM — https://en.wikipedia.org/wiki/DDR5_SDRAM
52. UMD DRAMSim — https://terpconnect.umd.edu/~blj/papers/memsys2018-dramsim.pdf
53. LexarEnterprise — LPDDR5X power — https://lexarenterprise.com/lpddr5x-power-consumption-guide
54. SabrePC — LPDDR & unified memory — https://www.sabrepc.com/blog/computer-hardware/what-is-lpddr-memory-and-unified-memory
55. Rambus — HBM3 guide — https://www.rambus.com/blogs/hbm3-everything-you-need-to-know
56. Wikipedia — High Bandwidth Memory — https://en.wikipedia.org/wiki/High_Bandwidth_Memory
57. Micron — HBM3E — https://www.micron.com/products/memory/hbm/hbm3e
58. Siemens EDA — HBM3E/HBM4 IC design guide — https://blogs.sw.siemens.com/semiconductor-packaging/2026/04/24/hbm3e-hbm4-ic-design-guide
59. JEDEC — HBM4 (JESD270-4) — https://www.businesswire.com/news/home/20250416843598/en/
60. SemiAnalysis — Scaling the Memory Wall — https://newsletter.semianalysis.com/p/scaling-the-memory-wall-the-rise-and-roadmap-of-hbm
61. Fine-Grained DRAM, MICRO 2017 — https://www.computer.org/csdl/proceedings-article/micro/2017/08686544/19RRX1PxMcg
62. Intel — Xeon Max — https://www.intel.com/content/www/us/en/products/details/processors/xeon/max-series.html
63. ServeTheHome — SPR-HBM — https://www.servethehome.com/intel-xeon-max-9480-deep-dive-intel-has-64gb-hbm2e-onboard-like-a-gpu-or-ai-accelerator
64. AMD — MI300 — https://www.amd.com/en/products/accelerators/instinct/mi300.html
65. ChipsAndCheese — MI300A — https://chipsandcheese.com/p/inside-the-amd-radeon-instinct-mi300as
66. Introl — HBM evolution — https://introl.com/blog/hbm-evolution-hbm3-hbm3e-hbm4-memory-ai-gpu-2025
67. ResearchGate — Reducing DRAM Refresh Power — https://www.researchgate.net/publication/337282606_Reducing_DRAM_Refresh_Power_Consumption_by_Runtime_Profiling_of_Retention_Time_and_Dual-row_Activation
68. Stanford VLSI DRAM energy — https://www-vlsi.stanford.edu/people/alum/pdf/1810_Ha_DRAMEnergy.pdf
69. Frame.community — RAM density vs suspend power — https://community.frame.work/t/impact-of-ram-density-on-suspend-power-consumption/57664

### CXL

70. CXL Consortium — Q1 2025 webinar — https://computeexpresslink.org/wp-content/uploads/2025/02/CXL_Q1-2025-Webinar-Presentation_FINAL.pdf
71. CXL Consortium — Q3 2025 webinar — https://computeexpresslink.org/wp-content/uploads/2025/10/CXL_Q3-2025-Webinar_FINAL.pdf
72. Rambus — CXL blog — https://www.rambus.com/blogs/compute-express-link
73. Synopsys — CXL 3.0 — https://www.synopsys.com/blogs/chip-design/what-is-compute-express-link-3.html
74. Wikipedia — CXL — https://en.wikipedia.org/wiki/Compute_Express_Link
75. BusinessWire — CXL 3.1 — https://www.businesswire.com/news/home/20231114332690/en/
76. CXL 3.1 webinar — https://computeexpresslink.org/wp-content/uploads/2024/03/CXL_3.1-Webinar-Presentation_Feb_2024.pdf
77. Lenovo — CXL 2.0 intro — https://lenovopress.lenovo.com/lp2146-introduction-to-cxl-20-memory
78. SemiAnalysis — CXL is dead in the AI era — https://newsletter.semianalysis.com/p/cxl-is-dead-in-the-ai-era
79. Lerner et al., VLDB PVLDB 17 — CXL and the Return of Scale-Up DB Engines — https://www.vldb.org/pvldb/vol17/p2568-lerner.pdf
80. arXiv 2401.01150 — CXL for DB — https://arxiv.org/html/2401.01150v1
81. DRack, USENIX ATC 2025 — https://www.usenix.org/system/files/atc25-zhang-xu.pdf
82. Computational CXL-Memory, IEEE CA 2023 — https://www.computer.org/csdl/journal/ca/2023/01/09969883/1IRiqguuyFq
83. Weisgut et al., VLDB PVLDB 18 — CXL Memory Performance — https://www.vldb.org/pvldb/vol18/p3119-weisgut.pdf
84. Liu et al., ASPLOS 2025 — Melody — https://people.cs.vt.edu/~jinshu/docs/papers/Melody_ASPLOS.pdf
85. Wang et al., IPDPS 2025 — CXL Performance — https://pasalabs.org/papers/2025/IPDPS25_CXL.pdf
86. ACM Computing Surveys — Disaggregated Memory — https://dl.acm.org/doi/10.1145/3807443
87. NSF PAR 2023 — Memory Disaggregation — https://par.nsf.gov/servlet/purl/10516025
88. Lim et al., ISCA 2009 — Disaggregated Memory — https://safari.ethz.ch/architecture/fall2021/lib/exe/fetch.php?media=isca09-disaggregate.pdf
89. KAIST — Memory Disaggregation survey — https://oslab.kaist.ac.kr/wp-content/uploads/esos_files/courseware/graduate/EE817/Memory%20disaggregation%20Research%20problems%20and%20opportunities.pdf

### Storage

90. Kingston — NVMe technology — https://www.kingston.com/en/ssd/what-is-nvme-ssd-technology
91. Micron PCIe Gen6 SSD — https://hardforum.com/threads/micron-announces-industry-first-pcie-gen6-ssd-claims-26gb-s-transfer-speed.2036325
92. SimplyBlock — NVMe latency — https://simplyblock.io/glossary/nvme-latency
93. PCI-SIG — PCIe 7.0 webinar — https://pcisig.com/sites/default/files/2026-01/PCI-SIG%20PCIe%207.0%20Webinar_Rev5_FINAL.pdf
94. IntelligentVisibility — NVMe-oF comparison — https://intelligentvisibility.com/nvme-over-fabrics-ethernet-comparison
95. DataCore — NVMe-oF — https://www.datacore.com/blog/breaking-storage-bottlenecks-with-nvme-of
96. Western Digital — RoCEv2 whitepaper — https://documents.westerndigital.com/content/dam/doc-library/en_us/assets/public/western-digital/collateral/white-paper/white-paper-open-flex-data24-roce-vs-tcp.pdf
97. Newegg — SSD lifespan 2026 — https://www.newegg.com/insider/ssd-lifespan-decoded-understanding-nand-types-and-write-endurance-in-2026/
98. Kingston — SLC/MLC/TLC/QLC — https://www.kingston.com/en/blog/pc-performance/difference-between-slc-mlc-tlc-3d-nand
99. Kioxia — NAND endurance brief — https://americas.kioxia.com/content/dam/kioxia/en-us/business/memory/asset/KIOXIA-SSD-NAND-Endurance-Tech-Brief.pdf
100. Solidigm — QLC NAND — https://www.solidigm.com/products/technology/qlc-nand-ready-for-mainstream-use-in-data-center.html
101. ATP Inc — SSD TBW/DWPD — https://www.atpinc.com/de/blog/ssd-tbw-dwpd-endurance
102. zonedstorage.io — ZNS intro — https://zonedstorage.io/docs/introduction/zns
103. arxiv 2511.04687 — SilentZNS — https://arxiv.org/pdf/2511.04687
104. Blocks&Files — Samsung SmartSSD gen 2 — https://www.blocksandfiles.com/ai-ml/2022/07/21/2nd-generation-samsung-smartssd-gets-smarter/1605257
105. AMD/Xilinx — SmartSSD brief — https://www.xilinx.com/publications/product-briefs/xilinx-smartssd-computational-storage-drive-product-brief.pdf
106. ScaleFlux — CSD — https://scaleflux.com/ufaq/what-is-computational-storage
107. CIDR 2021 — Computational Storage — https://www.cidrdb.org/cidr2021/papers/cidr2021_paper29.pdf
108. ACM TACO 2022 — Computational Storage for Energy Efficiency — https://dl.acm.org/doi/full/10.1145/3528577
109. Kioxia — FL6 brief — https://americas.kioxia.com/content/dam/kioxia/shared/business/ssd/enterprise-ssd/asset/productbrief/eSSD-FL6-product-brief.pdf
110. SIGARCH — Persistent Memory: A New Hope — https://www.sigarch.org/persistent-memory-a-new-hope
111. Tom's Hardware — Optane EOL — https://www.tomshardware.com/pc-components/ssds/intel-schedules-the-end-of-its-200-series-optane-memory-dimms-shipments-to-draw-to-an-end-in-late-2025
112. ACM — Post-Optane PM research — https://dl.acm.org/doi/10.1145/3609308.3625268
113. NVM Express — SAS-to-PCIe transition — https://nvmexpress.org/sas-to-pcie-transition-unlocking-the-power-of-nvme-technology
114. Solidigm — NVMe-to-SATA — https://www.solidigm.com/products/technology/nvme-to-sata-transition-for-performance-improvements.html
115. StorageReview — Solidigm + liquid cooling — https://www.storagereview.com/review/boosting-data-center-efficiency-with-solidigm-ssds-and-liquid-cooled-servers
116. NVM Express — Power features — https://nvmexpress.org/resource/technology-power-features

### Interconnects

117. Rambus — PCIe guide — https://www.rambus.com/blogs/the-ultimate-guide-to-pci-express
118. Diodes — PCIe 6.0 — https://www.diodes.com/design/support/perspective/navigating-the-pcie-6-0-era-boosting-performance-with-redrivers
119. Keysight — PCIe 6 PAM4 — https://www.keysight.com/blogs/en/inds/2023/05/11/why-did-pcie-6-0-adopt-pam4-there-are-many-reasons
120. Samtec — PAM4 — https://blog.samtec.com/post/why-did-pcie-6-0-adopt-pam4-there-are-many-reasons
121. CXL blog — CCIX transfer — https://computeexpresslink.org/blog/compute-express-link-consortium-inc-and-ccix-consortium-inc-announce-agreement-for-consortium-to-receive-ccix-consortium-specifications-and-other-ccix-consortium-assets-1052
122. Synopsys — CXL vs CCIX — https://www.synopsys.com/articles/emerging-applications-cxl.html
123. SemiEngineering — CXL vs CCIX — https://semiengineering.com/cxl-vs-ccix
124. NextPlatform — CXL and Gen-Z — https://www.nextplatform.com/connect/2020/04/03/cxl-and-gen-z-iron-out-a-coherent-interconnect-strategy/1644243
125. Intel — 4th-gen Xeon overview — https://www.intel.com/content/www/us/en/developer/articles/technical/fourth-generation-xeon-scalable-family-overview.html
126. SemiAnalysis — Emerald Rapids — https://newsletter.semianalysis.com/p/intel-emerald-rapids-backtracks-on
127. Medium — NUMA refresher — https://medium.com/@hxu296/numa-architecture-quick-refresher-08599f428298
128. AMD — 4th-gen EPYC WP — https://www.amd.com/content/dam/amd/en/documents/products/epyc/4th-gen-epyc-processor-architecture-white-paper.pdf
129. AMD — 5th-gen EPYC WP — https://www.megware.com/fileadmin/user_upload/LandingPage%20AMD/5th-gen-amd-epyc-white-paper.pdf
130. Apple — M1 Ultra — https://www.apple.com/newsroom/2022/03/apple-unveils-m1-ultra-the-worlds-most-powerful-chip-for-a-personal-computer
131. Apple — M2 Ultra — https://www.apple.com/newsroom/2023/06/apple-introduces-m2-ultra
132. NVIDIA — NVLink — https://www.nvidia.com/en-us/data-center/nvlink
133. Wikipedia — NVLink — https://en.wikipedia.org/wiki/NVLink
134. FiberMart — 400G to 1.6T — https://www.fibermall.com/blog/400g-to-800g-to-1600g-optical-transceivers.htm
135. CloudSwitch — 800G Ethernet — https://cloudswit.ch/blogs/what-is-800g-ethernet
136. FiberMart — 100G vs 400G — https://www.fiber-mart.com/news/100g-vs-400g-networks-which-one-to-choose-for-your-data-center-a-6687.html
137. Spheron — InfiniBand vs RoCE vs Spectrum-X — https://www.spheron.network/blog/gpu-networking-infiniband-roce-spectrum-x-guide
138. FS.com — IB vs RoCEv2 — https://www.fs.com/blog/infiniband-vs-roce-v2-whats-the-best-fit-for-your-ai-data-center-25811.html
139. Naddod — IB vs RoCEv2 — https://www.naddod.com/blog/infiniband-vs-roce-v2-which-is-best-network-architecture-for-ai-computing-center

### Topology

140. AMD — NUMA optimization WP — https://www.amd.com/content/dam/amd/en/documents/epyc-business-docs/white-papers/AMD-Optimizes-EPYC-Memory-With-NUMA.pdf
141. Dell — NPS settings — https://infohub.delltechnologies.com/en-us/p/numa-configuration-settings-on-amd-epyc-2nd-generation
142. ChipsAndCheese — Turin — https://chipsandcheese.com/p/amds-turin-5th-gen-epyc-launched
143. StorageReview — Turin — https://www.storagereview.com/review/amd-epyc-turin-review-192-cores-of-zen-5
144. Megware — Turin WP — https://www.megware.com/fileadmin/user_upload/LandingPage%20AMD/5th-gen-amd-epyc-white-paper.pdf
145. ChipsAndCheese — Xeon 6 memory — https://chipsandcheese.com/p/a-look-into-intel-xeon-6s-memory
146. ChipsAndCheese — Genoa-X — https://chipsandcheese.com/p/genoa-x-server-v-cache-round-2
147. ChipsAndCheese — Turin UMA modes — https://chipsandcheese.com/p/evaluating-uniform-memory-access
148. McCalpin — SPR HBM bandwidth — https://www.ixpug.org/images/docs/ISC23/McCalpin_SPR_BW_limits_2023-05-24_final.pdf
149. Jason Rahman — SPR core-to-core latency — https://jprahman.substack.com/p/sapphire-rapids-core-to-core-latency
150. Edera — NUMA — https://edera.dev/stories/numa-part-1-cores-memory-and-the-distance-between-them
151. Medium — Memory proximity — https://sourav-k-paul.medium.com/memory-proximity-for-performance-f1be9f8c0a8a
152. NexThink — cache latency — https://nexthink.com/blog/smarter-cpu-testing-kaby-lake-haswell-memory
153. Nutanix — SNC support — https://portal.nutanix.com/page/documents/solutions/details?targetId=BP-2097-SAP-HANA-on-AHV:sub-numa-clustering-support.html
154. AWS — Nitro System — https://aws.amazon.com/ec2/nitro
155. AWS — Nitro security WP — https://docs.aws.amazon.com/whitepapers/latest/security-design-of-aws-nitro-system/the-components-of-the-nitro-system.html
156. AllThingsDistributed — Nitro — https://www.allthingsdistributed.com/2020/09/reinventing-virtualization-with-nitro.html

### SmartNIC / DPU

157. Introl — DPUs/SmartNICs 2025 — https://introl.com/blog/dpus-smartnics-data-center-infrastructure-bluefield-pensando-2025
158. NVIDIA — BlueField dev blog — https://developer.nvidia.com/blog/offloading-and-isolating-data-center-workloads-with-bluefield-dpu
159. NVIDIA — BlueField storage — https://simplyblock.io/glossary/nvidia-bluefield-dpu
160. Intel — IPU — https://www.intel.com/content/www/us/en/products/details/network-io/ipu.html
161. AMD — Pensando — https://www.amd.com/en/products/data-processing-units/pensando.html
162. ScienceDirect — Security offload — https://www.sciencedirect.com/science/article/abs/pii/S1389128625007960
163. DataIntelo — Computational storage market — https://dataintelo.com/report/global-computational-storage-market

### Cache Coherence

164. Wikipedia — Cache coherency protocols — https://en.wikipedia.org/wiki/List_of_cache_coherency_protocols
165. Cadence — MOESI vs MESI — https://community.cadence.com/cadence_blogs_8/b/breakfast-bytes/posts/what-39-s-the-difference-between-moesi-and-mesi-cache-coherence-for-poets
166. Redis — Cache coherence glossary — https://redis.io/glossary/cache-coherence
167. StackOverflow — MOESI vs MESI — https://stackoverflow.com/questions/49983405/what-is-the-benefit-of-the-moesi-cache-coherency-protocol-over-mesi
168. EAJournals — Demystifying coherence — https://eajournals.org/wp-content/uploads/sites/21/2025/06/Demystifying.pdf
169. Berkeley — Directory coherence lecture — https://people.eecs.berkeley.edu/~pattrsn/252F96/Lecture18.pdf
170. PatSnap — Coherence — https://eureka.patsnap.com/article/cache-coherence-protocols-mesi-moesi-and-directory-based-systems
171. ARM — AMBA CHI fundamentals — https://developer.arm.com/documentation/102407/0102/CHI-protocol-fundamentals
172. ARM — CHI cache stashing — https://developer.arm.com/additional-resources/video-tutorials/amba-chi-cache-stashing-flow-overview

---

## How to Use This Knowledgebase

1. **For hot-loop design:** Start with §7 (top-10 cheapest instructions). Choose the cheapest instruction that does the job. Avoid the top-10 expensive list.
2. **For storage format design:** Start with §2 (memory hierarchy). Pick the tier your hot working set fits in; design the format so the hot loop's instruction can stream from that tier.
3. **For cluster topology:** Start with §4 (interconnects) and §5 (NUMA). Decide CXL vs RDMA reach, NPS/SNC partitioning, page placement.
4. **For energy budgeting:** §2.3 gives energy per access; §1 gives energy per instruction. Multiply through to estimate query energy.
5. **For protocol selection:** §3 (storage) and §4 (network) tell you which protocol fits which reach/coherence need.
6. **For DPU/offload decisions:** §6 lists what can be offloaded and the energy benefit.

**The single biggest takeaway:** data locality beats microoptimization. A single L3 miss costs as much as 20–50 free ALU ops; a DRAM miss costs as much as 200–500 ALU ops. Design the engine around the hierarchy, not the inner loop.

---

*End of knowledgebase. All numeric claims carry inline citations. Items marked `(est.)` should be validated against current hardware before use in design decisions. AMD RAPL is a model; Intel RAPL (post-Haswell) is reasonably accurate for ≥10 ms windows. Per-instruction energy on an OoO core is a modeling abstraction.*
