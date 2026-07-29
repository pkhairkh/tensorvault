# Glossary

> Terms, notation, and conventions used throughout the problem catalog and
> the engine documentation.

---

## A–B

### AGM bound
The Atserias-Grohe-Marx bound: the worst-case size of a join result is
$\prod_i |R_i|^{f_i}$ over a fractional cover. Used in worst-case-optimal
leapfrog joins. See P-05-05.

### AMS sketch
Alon-Matias-Szegedy sketch for frequency moment estimation (especially $F_2$).
See `docs/research/probability_sketching_for_db.md` §2.

### ANS
Asymmetric Numeral Systems (Duda 2009). An entropy-optimal compression
scheme that uses table lookups, making it SIMD-decodable. See P-03-03.

### Affine type
A type that can be used zero or one times (dropped, but not duplicated).
Rust's default type discipline is affine. See P-04-01, P-05-15.

### Batch
A contiguous run of cells processed by one kernel invocation. Typically
4 KB (504 cells). See P-01-12.

### BMI2
Bit Manipulation Instructions 2 (Intel): `PEXT`, `PDEP`. Fast on Intel
and Zen 3+, microcoded (250× slower) on Zen/Zen2. See P-01-02.

### Bloom filter
A probabilistic data structure for set membership testing with one-sided
error. See `docs/research/probability_sketching_for_db.md` §10.

---

## C

### Calvin
A deterministic concurrency control protocol that pre-orders transaction
inputs to avoid distributed locking. See `docs/tpcc_analysis.md` §3.

### Category theory
The mathematical study of structure via objects, morphisms, functors, and
natural transformations. Used for schema evolution (P-05-09) and type
safety (P-05-15). Pillar V.

### Cell
A single 64-bit value in the engine. Every value (int, float, string,
pointer, null) is stored as a u64. See `ARCHITECTURE.md`.

### CXL
Compute Express Link. A cache-coherent interconnect over PCIe PHY.
CXL.mem (Type-3) provides memory expansion at ~140–500 ns latency.
See `docs/cpu_energy_kb.md` §2.5.

### Cheeger's inequality
Relates the graph's expansion $\phi$ to the second eigenvalue $\lambda_2$
of the Laplacian: $\frac{1}{2}\lambda_2 \le \phi \le \sqrt{2\lambda_2}$.
Used for NUMA partitioning. See P-05-02.

### Competitive ratio
In online algorithms, the ratio between the online algorithm's cost and
the offline optimal cost. LRU is k-competitive. See P-05-08.

### Concentration inequality
A bound on how much a random variable deviates from its expected value.
Hoeffding, McDiarmid, Bernstein. Used for sketch error bounds.
See `docs/research/probability_sketching_for_db.md` §1.

### Count-Min sketch
A sublinear-space sketch for frequency estimation with $(\varepsilon, \delta)$
guarantees. See `docs/research/probability_sketching_for_db.md` §2.

---

## D–F

### DDR5
The current generation of server DRAM. ~90 ns latency, ~50 GB/s per channel.
See `docs/cpu_energy_kb.md` §2.4.

### (ε, δ) guarantee
A probabilistic guarantee: the result is within ε of the true value with
probability at least 1-δ. The SQL surface for approximate queries. See
P-05-04, P-05-11, Q-01.

### Functorial data migration
Schema evolution as a functor application. Three adjoint functors:
$\Sigma_F \dashv \Delta_F \dashv \Pi_F$. See P-05-09, P-09-04.

---

## G–I

### HBM
High Bandwidth Memory. Stacked DRAM with ~1.6–5.3 TB/s bandwidth.
On Xeon Max and MI300A. See P-02-07.

### Heterogeneity
The property of a column or batch having mixed cell types. A monomorphic
batch (all same type) gets a tag-free JIT'd inner loop. See P-07-04.

### Hoeffding's inequality
$P(\bar{X} - \mu \ge \varepsilon) \le e^{-2n\varepsilon^2}$ for bounded
independent random variables. Used for approximate aggregate bounds.
See `docs/research/probability_sketching_for_db.md` §1.

### HLL
HyperLogLog. A sketch for cardinality estimation with RSE $1.04/\sqrt{m}$.
See `docs/research/probability_sketching_for_db.md` §2.

### Information bottleneck
A method for finding a compressed representation $T$ of $X$ that preserves
information about $Y$: $\min I(T;X) - \beta I(T;Y)$. Used for index
selection. See `docs/research/info_theory_for_db.md` §7.

### Instruction-first
The design philosophy: start from the cheapest instructions per joule,
place data in the tier that feeds them, treat protocol boundaries as
first-class. See `ARCHITECTURE.md`.

---

## J–L

### JL lemma
Johnson-Lindenstrauss: n points in $\mathbb{R}^d$ can be embedded into
$\mathbb{R}^k$ with $k = O(\varepsilon^{-2} \log n)$ preserving pairwise
distances within $1 \pm \varepsilon$. See P-05-02,
`docs/research/spectral_db_research.md` §2.

### Kan extension
A categorical construction that gives left ($\Sigma$) and right ($\Pi$)
adjoints to pullback ($\Delta$). The basis of functorial data migration.
See P-05-09.

### Kernel
A hand-tuned instruction sequence for a specific (operator, CPU, tier)
tuple. The kernel table is the engine's moat. See `ARCHITECTURE.md`.

### Kernel table
The registry of all kernels, indexed by (operator, CPU, tier). At startup,
the engine probes CPUID and selects the best kernel per (operator, tier).
See `src/kernel/mod.rs`.

### Kingman's formula
$W \approx \frac{\rho}{1-\rho} \cdot \frac{c_a^2 + c_s^2}{2} \cdot \mu^{-1}$.
Predicts G/G/1 queueing delay. Used for CXL latency modeling. See P-02-06.

### Kleisli category
The category of free algebras for a monad. Used to model query composition.
See `docs/research/category_theory_topology_db.md` §5.

### Leapfrog join
A worst-case-optimal join algorithm that achieves the AGM bound.
See P-05-05.

### Linear type
A type that must be used exactly once (not zero, not duplicated). Stronger
than affine. Used for CXL reference safety. See P-04-01, P-05-15.

### LSH
Locality-Sensitive Hashing. Hashes that preserve similarity: similar items
collide with higher probability. Andoni-Indyk achieves $\rho = 1/c$ optimal.
See P-09-07, `docs/research/probability_sketching_for_db.md` §3.

---

## M–O

### MDL
Minimum Description Length. The computable approximation to Kolmogorov
complexity. Used for schema selection. See P-05-01, `src/schema/mdl.rs`.

### MDP
Markov Decision Process. Used for adaptive execution. See P-07-03.

### Memory tier
A level of the memory hierarchy: L1/L2, L3, DDR5, HBM, CXL, NVMe,
NVMe-oF, Network. Each has characteristic latency, bandwidth, energy.
See `src/memory/tier.rs`.

### Migration
Moving a region from one tier to another. The unit of migration is a 2 MB
region. See P-02-03, P-02-04.

### Monoid
A set with an associative binary operation and an identity. Used for
query composition (the list monad, the sum monoid). See
`docs/research/category_theory_topology_db.md` §9.

### NUMA
Non-Uniform Memory Access. Different memory nodes have different latencies.
Cross-socket access is 1.5–2× local. See P-02-05,
`docs/cpu_energy_kb.md` §5.

### NPS
NUMA Per Socket (AMD). Partitioning a socket into multiple NUMA nodes.
See `docs/cpu_energy_kb.md` §5.3.

### Online algorithm
An algorithm that processes input sequentially without knowing the future.
LRU is k-competitive for paging. See P-05-08.

### Operator
A kernel-table identifier for a database operation: ScanEqU64, HashProbe,
AggregateSumF64, SimilarityHamming, etc. See `src/kernel/mod.rs`.

---

## P–R

### PAC
Probably Approximately Correct (Valiant 1984). The learning framework:
with probability $1-\delta$, the result is within $\varepsilon$ of optimal.
See P-05-11.

### Page
A 4 KB I/O unit: 64-byte header + 4032 bytes (504 u64 cells). See
`src/storage/page.rs`.

### Partitioning
Splitting data across NUMA nodes, CXL devices, or racks. Spectral
partitioning uses the Fiedler vector. See P-05-02.

### Pillar
One of the five mathematical domains: I (info theory), II (spectral),
III (probability), IV (optimization), V (category theory). See
`docs/math_foundations.md`.

### POPCNT / VPOPCNTDQ
Population count (number of 1 bits in a word). `VPOPCNTDQ` is the AVX-512
vectorized version (8×64-bit lanes per cycle). See P-01-06.

### Protocol boundary
A transition between coherence domains: CXL (single-rack), Raft (cross-rack),
async (cross-region). See `src/protocol/`.

### Quantization
Approximating a high-precision value with a lower-precision one. Lloyd-Max
(scalar), product quantization (vector). See P-03-04,
`docs/research/info_theory_for_db.md` §10.

### RAPL
Running Average Power Limit. Intel's on-die energy accounting. Accurate
for ≥10 ms windows. See `docs/cpu_energy_kb.md` §2.

### Raft
A consensus algorithm for replicated logs. Used for cross-rack transactions.
See `src/protocol/raft.rs`.

### Rate-distortion
Shannon's theory of lossy compression: $R(D) = \min I(X;\hat{X})$ s.t.
$E[d] \le D$. See P-03-04, `docs/research/info_theory_for_db.md` §1.

### Region
A 2 MB unit of memory: 512 pages. The unit of migration between tiers.
See `src/memory/region.rs`.

### RoCEv2
RDMA over Converged Ethernet v2. Kernel-bypass networking over Ethernet.
~5–10 µs RTT. See `docs/cpu_energy_kb.md` §4.5.

---

## S–T

### Schema-on-read
Deferring the type interpretation of a column until query time. Backed by
MDL selection. See P-03-10, P-05-01.

### Selinger DP
The dynamic programming algorithm for join ordering (System R, 1979).
$O(3^n)$. See P-07-02.

### Sheaf
A mathematical construction that assigns local data to open sets of a
topology, with gluing conditions. Used for distributed consistency.
See P-05-10, P-09-08.

### SIMD
Single Instruction, Multiple Data. Processing multiple data elements per
instruction. AVX-512 processes 8×64-bit lanes per cycle. See
`docs/cpu_energy_kb.md` §1.3–1.6.

### Sketch
A sublinear-space summary of a data stream with probabilistic guarantees.
HLL, Count-Min, AMS, t-Digest. See `docs/research/probability_sketching_for_db.md` §2.

### Spectral graph theory
The study of graphs via the eigenvalues of their adjacency/Laplacian
matrices. Cheeger's inequality, spectral clustering. See P-05-02,
`docs/research/spectral_db_research.md`.

### Submodular function
A function with diminishing returns: $f(A \cup \{x\}) - f(A) \ge f(B \cup \{x\}) - f(B)$
for $A \subseteq B$. Greedy gives $(1-1/e)$ approximation. Used for index
selection. See P-05-14.

### Tablet
A 2 GB unit of memory: 1024 regions. The unit of NUMA placement.
See `src/storage/tablet.rs`.

### Tier
See Memory tier.

### Topos
A category that behaves like the category of sets, supporting an internal
logic. Used for schema-as-theory. See `docs/research/category_theory_topology_db.md` §3.

### Trace JIT
A just-in-time compiler that records a "trace" (hot path) and compiles it
to specialized machine code. TraceMonkey (2009) is the precedent. See P-07-04.

### tANS
Table-based Asymmetric Numeral Systems. Used by Facebook's Zstd. See P-03-03.

---

## U–Z

### Univalence
The HoTT axiom: $(A = B) \simeq (A \simeq B)$. Used for schema equivalence.
See `docs/research/category_theory_topology_db.md` §15.

### VFMADD231PS
Fused Multiply-Add: $a \cdot b + c$ in one instruction. 2/cycle on SPR.
The workhorse of FP kernels. See `docs/cpu_energy_kb.md` §1.4.

### VPCMPEQQ
AVX-512 instruction: compare 8×64-bit lanes for equality, producing a
mask. The workhorse of scan kernels. See `docs/cpu_energy_kb.md` §1.5.

### VPOPCNTDQ
See POPCNT.

### VPTERNLOGQ
AVX-512 instruction: any 3-input bitwise truth table in one instruction.
The cheapest-per-joule instruction in the knowledgebase. See P-01-05.

### WAL
Write-Ahead Log. A durable append-only log for transaction recovery.
Should use ZNS NVMe. See `src/storage/wal.rs`, P-03-05.

### Wyner-Ziv
Lossy source coding with side information at the decoder. Used for
cross-column compression. See `docs/research/info_theory_for_db.md` §5.

### ZNS
Zoned Namespace SSD. An SSD that exposes zones which must be written
sequentially. Eliminates GC tail latency. See P-03-05.

---

## Notation

| Symbol | Meaning |
|--------|---------|
| $n$ | Number of cells / rows |
| $k$ | Number of tiers, or LSH hash functions |
| $\varepsilon$ | Error bound (approximate queries) |
| $\delta$ | Failure probability (approximate queries) |
| $\rho$ | Utilization (queueing theory) |
| $\lambda$ | Arrival rate (queueing theory) |
| $\mu$ | Service rate (queueing theory) |
| $c_a, c_s$ | Coefficients of variation (Kingman) |
| $L$ | Latency, or Laplacian matrix |
| $D$ | Distortion (rate-distortion), or degree matrix |
| $\lambda_2$ | Second eigenvalue (algebraic connectivity) |
| $I(X;Y)$ | Mutual information |
| $H(X)$ | Shannon entropy |
| $R(D)$ | Rate-distortion function |
| $K(x)$ | Kolmogorov complexity |
| $\Sigma_F, \Delta_F, \Pi_F$ | Functorial migration adjoints |

---

## Conventions

- **Status**: 🔴 open, 🟡 partial, 🟢 solved
- **Effort**: S (< 1 month), M (1–3 months), L (3–6 months), XL (6+ months)
- **Impact**: low, medium, high, critical
- **Math pillars**: I (info), II (spectral), III (prob), IV (opt), V (cat)
- **Problem IDs**: `P-<layer>-<number>` (e.g., P-02-04 is the 4th memory problem)
- **Query extension IDs**: `Q-<number>` (e.g., Q-01 is the approximate query extension)

---

*This glossary is updated as new terms are introduced. If you encounter an
undefined term, add it here.*
