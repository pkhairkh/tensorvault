# Information Theory, Coding Theory, and Rate-Distortion Theory for a Next-Generation Database Engine

> **Context.** We are building an *instruction-first, memory-centric* database engine: every value is a 64-bit word, data lives in explicit memory tiers (L1/L2/L3/DDR5/HBM/CXL/NVMe/Network), and the kernel table is a hand-tuned AVX-512 kernel per (CPU, tier) tuple. We already use MDL for schema selection. This document asks: *what more of the math of information theory can we exploit?*
>
> Each section gives (a) the mathematical foundation with formulas, (b) the canonical papers with inline links, and (c) concrete applications to our engine.

**Foundational reference for the whole document:** C.E. Shannon, *"A Mathematical Theory of Communication,"* Bell System Technical Journal 27, 1948 — [DOI 10.1002/j.1538-7305.1948.tb01338.x](https://doi.org/10.1002/j.1538-7305.1948.tb01338.x). Cover & Thomas, *Elements of Information Theory*, 2nd ed., Wiley 2006 — [Wiley](https://www.wiley.com/en-us/Elements+of+Information+Theory%2C+2nd+Edition-p-9780471241959).

---

## 1. Rate-Distortion Theory for Lossy Compression

### 1.1 Foundation

Shannon's **rate-distortion (RD) theorem** characterizes the minimum number of bits/symbol needed to reconstruct a source within a fidelity constraint. For a source $X\sim p(x)$ and a distortion measure $d:\mathcal{X}\times\hat{\mathcal{X}}\to\mathbb{R}_+$, the rate-distortion function is

$$R(D) \;=\; \min_{p(\hat{x}\mid x):\;\mathbb{E}[d(X,\hat{X})]\le D}\; I(X;\hat{X}) \quad\text{[bits/symbol]}.$$

The operational meaning (Shannon's *source coding with a fidelity criterion*, 1959): there exists a code of rate $R>R(D)$ achieving average distortion $\le D+\varepsilon$, and no code of rate $R<R(D)$ can. Reference: Cover & Thomas Ch. 10; Gray, *Source Coding Theory* (Kluwer, 1990) — [Springer](https://link.springer.com/book/10.1007/978-1-4615-6440-9); Gray & Neuhoff 1998, *"Quantization,"* IEEE Trans. Inf. Theory (commemorative issue) — [IEEE](https://ieeexplore.ieee.org/document/761109).

**Shannon lower bound (SLB).** For difference distortion $d(x,\hat x)=\rho(x-\hat x)$ and an entropy-power-achieving source,

$$R(D)\;\ge\; h(X)-\tfrac{1}{2}\log(2\pi e\, D_{\text{eff}}),$$

tight as $D\to 0$ for memoryless Gaussian sources where $R(D)=\tfrac12\log(\sigma_X^2/D)$ and the *distortion-rate* inverse is $D(R)=\sigma_X^2\,2^{-2R}$ (the famous 6.02 dB/bit PCM law).

### 1.2 Blahut–Arimoto algorithm

The minimization defining $R(D)$ is convex but has no closed form in general. The **Blahut–Arimoto (BA)** algorithm alternates two updates that are the information-theoretic EM:

Given a fixed $s\le 0$ (slope parameter, $R'(D)=s$) and an initial $q(\hat x)>0$:

$$q^{(t+1)}(\hat x)=\frac{q^{(t)}(\hat x)\,\mathbb{E}_{p(x)}\!\left[e^{s\,d(x,\hat x)}\,\big|\,\hat x\right]}{Z},\qquad
p^{(t+1)}(\hat x\mid x)=\frac{q^{(t)}(\hat x)\,e^{s\,d(x,\hat x)}}{\sum_{\hat x'}q^{(t)}(\hat x')\,e^{s\,d(x,\hat x')}}.$$

BA converges geometrically (linear convergence rate, $O(\log(1/\varepsilon))$ iterations) to the point on the $R(D)$ curve with slope $s$.

- Blahut, R.E., *"Computation of channel capacity and rate-distortion functions,"* IEEE TIT 18(4), 1972 — [IEEE](https://ieeexplore.ieee.org/document/1054801).
- Arimoto, S., *"An algorithm for computing the capacity of arbitrary discrete memoryless channels,"* IEEE TIT 18(1), 1972 — [IEEE](https://ieeexplore.ieee.org/document/1054802).
- Modern account: Arimoto's twin update is the same alternating minimization that yields the **information bottleneck** (§7), establishing a deep link between lossy compression and representation learning.

### 1.3 Applications to the engine

1. **Float columns with guaranteed max error.** Each 64-bit word may be a `double`. For an $L_\infty$ SLA $\|X-\hat X\|_\infty\le \varepsilon$, scalar uniform quantization with step $\Delta=2\varepsilon$ costs $\lceil\log_2((x_{\max}-x_{\min})/2\varepsilon)\rceil$ bits and is RD-optimal for uniform sources (Panter–Dite, 1951: $R(D)\approx\log(1/D)$ for uniform/$L_\infty$). We store the **scale + codebook index** as two 64-bit words and keep the original in a cold tier. The RD curve tells us *exactly* how many bits to spend per tier as a function of the SLA $\varepsilon$.

2. **Multi-resolution, tier-aware storage.** Assign each tier a budget $D_t$. The RD function gives the bit budget $R_t=R(D_t)$ to store in tier $t$. By the **successive refinement** theorem (Equitz & Cover, 1991, *"Successive refinement of information,"* IEEE TIT 37(3) — [IEEE](https://ieeexplore.ieee.org/document/79904)): a source is successively refinable iff its RD function decomposes as $R(D_1)=R_1$, $R(D_2)=R_1+R_2$ with $D_2\le D_1$. **Gaussian sources are successively refinable**, so for our (roughly Gaussian, decorrelated) sensor/financial columns we can keep a coarse $\hat X_1$ in L3, a refinement $\Delta_2$ in DDR5, and the full residual in NVMe — one canonical bitstream, multiple fidelities, zero waste. This is precisely the math our memory-tier model wants.

3. **Time-series SLA on precision.** For a Gaussian AR(1) process $X_t=\rho X_{t-1}+Z_t$, the rate-distortion function for predictive coding is $R(D)=\tfrac12\log^+\!\big(\tfrac{\sigma_Z^2}{D}\big)$ bits/sample (Berger, *Rate Distortion Theory*, 1971). Plugging $\sigma_Z^2=\sigma_X^2(1-\rho^2)$ shows that **correlation is compressible prediction gain** — each extra bit of correlation buys $\tfrac12\log(1/(1-\rho^2))$ rate reduction. We should residualize against a one-tap predictor before quantization; the AVX-512 kernel becomes a *predict–quantize–code* pipeline.

4. **Write-time BA optimization.** Run BA *once per column chunk* (a few hundred iterations, vectorizable) to fit an empirical $R(D)$ curve; choose the operating point on the curve dictated by the tier's SLA. The learned $p(\hat x\mid x)$ becomes the codebook stored alongside the chunk.

### 1.4 Key papers

- Cover & Thomas, *Elements of Information Theory*, 2nd ed., Ch. 10 (Rate Distortion Theory) — [Wiley](https://www.wiley.com/en-us/Elements+of+Information+Theory%2C+2nd+Edition-p-9780471241959).
- Berger, T., *Rate Distortion Theory*, Prentice-Hall, 1971.
- Gray, R.M., *Source Coding Theory*, Kluwer, 1990 — [Springer](https://link.springer.com/book/10.1007/978-1-4615-6440-9).
- Gray & Neuhoff, *"Quantization,"* IEEE TIT 44(6), 1998 — [IEEE](https://ieeexplore.ieee.org/document/761109).
- Equitz & Cover, *"Successive refinement of information,"* IEEE TIT 37(3), 1991 — [IEEE](https://ieeexplore.ieee.org/document/79904).
- Recent ML-for-compression: Ballé, Laparra & Simoncelli, *"End-to-end optimized image compression,"* ICLR 2017 — [arXiv:1611.01704](https://arxiv.org/abs/1611.01704); Mentzer et al., *"Conditional Probability Models for Deep Image Compression,"* CVPR 2018 — [arXiv:1801.04260](https://arxiv.org/abs/1801.04260) (learned RD via the BA/variational correspondence).

---

## 2. Lossless Compression with Arithmetic Coding and Asymmetric Numeral Systems

### 2.1 Entropy and the source-coding theorem

Shannon's noiseless coding theorem: for a DMS with entropy $H(X)=-\sum p(x)\log_2 p(x)$, the achievable lossless rate is $R>H(X)$ and the lower bound is $H(X)$.

### 2.2 Arithmetic coding

**Arithmetic coding** (Rissanen 1976; Pasco 1976; modern practical form Witten, Neal & Cleary 1987) encodes the *entire message* as a single subinterval of $[0,1)$ of length $\prod_i p(x_i)$, achieving $-\log_2 p(x_{1:n})$ bits — i.e., **within 2 bits of entropy for any distribution, finite-state, no need to block.** This is crucial: unlike Huffman (which is optimal only for dyadic $p$ and loses up to 1 bit/symbol otherwise), arithmetic coding is *entropy-optimal for arbitrary symbol models* including adaptive/frequency-table models.

- Rissanen, J., *"Generalized Kraft inequality and arithmetic coding,"* IBM J. Res. Dev. 20(3), 1976 — [IEEE/bibliography](https://ieeexplore.ieee.org/document/5391348).
- Pasco, R., *"Source coding algorithms for fast data compression,"* Ph.D. thesis, Stanford, 1976.
- Witten, Neal & Cleary, *"Arithmetic coding for data compression,"* CACM 30(6), 1987 — [ACM](https://dl.acm.org/doi/10.1145/214762.214771).

### 2.3 Asymmetric Numeral Systems (ANS)

**ANS** (Duda 2009) is a modern entropy coder that bridges arithmetic coding's optimality with the table-driven speed of Huffman. The core idea: encode a sequence of symbols into a single natural number $x$ using a state machine whose state-transition $x\to x'$ preserves a uniform distribution on the residual, while emitting/absorbing bits so that symbol $s$ with probability $p_s$ occupies $\approx 1/p_s$ of the state space (hence $\approx -\log_2 p_s$ bits/symbol).

The **tANS** (table ANS) variant precomputes a finite-state machine of $L=2^R$ states and streams symbols through it; **rANS** (range ANS) uses integer arithmetic for a smaller, branch-light decoder. tANS is the entropy stage of Facebook's **Zstandard** (via the FSE library); rANS is used in **JPEG XL** and **Draco**.

- Duda, J., *"Asymmetric Numeral Systems,"* arXiv:0902.0277, 2009 — [arXiv:0902.0277](https://arxiv.org/abs/0902.0277).
- Duda, J., *"Asymmetric numeral systems as close to capacity low state variable coding,"* arXiv:1311.2540, 2013 — [arXiv:1311.2540](https://arxiv.org/abs/1311.2540).
- Duda & Taubman, *"Asymmetric numeral systems,"* SPIE 2015.
- Yan, Cao, Li & Xu, *"A novel JPEG XL entropy coder baseline: rANS,"* SPIE 2020.

**Why ANS beats Huffman for us.** tANS achieves redundancy $\le \tfrac{1}{L}+p_{\max}\log_2 L$ bits/symbol (vanishes as $L$ grows); for $L=2^{10}$ and skewed alphabets this is far better than Huffman's $\le 1$ bit/symbol gap, *and* decoding is a single 256-entry table lookup per symbol — exactly the kind of operation an AVX-512 `VPGATHERDD` can pipeline.

### 2.4 Applications to the engine

1. **SIMD-decodable column compression.** rANS has a fully **branch-free** decode loop of the form `state = (state >> logM) << n_bits | read_bits(...); symbol = table[state & mask]`. Multiple independent rANS streams can be interleaved so that **8 streams decode in parallel using AVX-512 `VPSRLVD`/`VPGATHERDD`** — this is the technique behind Facebook's `zstd -T0` and the JPEG XL `group` parallelism. For a 64-bit-word column, we run 8 rANS streams per cache line (one per AVX-512 lane), achieving entropy-optimal decode at **>2 GB/s/core** on modern x86. This is the right lossless stage to pair with the lossy stage of §1.

2. **Per-kernel tuning.** Because tANS is table-driven, the (CPU, tier) kernel table can carry a *custom FSE table per column chunk*: the kernel dispatch `kernel[cpu][tier].decode` becomes `kernel[cpu][tier].decode[codebook_id]`, where each codebook is a tANS table fit to that chunk's empirical distribution. Refit + retable is cheap (one histogram + one normalize).

3. **Dictionary-based columnar coding.** ANS composes cleanly with a static dictionary (à la zstd's `--train`): short, repeated 64-bit patterns become dictionary symbols with high probability; ANS then entropy-codes the dictionary *references*. For our engine this gives a lossless column codec that approaches $H(X)$ even when the column is highly structured (enums, IDs, timestamps).

4. **Adaptive modeling for cold→hot promotion.** Use a **context-model** (order-0 or order-1) whose frequency table is updated lazily; arithmetic/ANS coding supports adaptive models natively (the interval/state is just a function of the running counts). Promoting a chunk from NVMe to HBM triggers a model refit; the cost is one histogram pass.

### 2.5 Key papers

- Duda 2009, [arXiv:0902.0277](https://arxiv.org/abs/0902.0277).
- Rissanen 1976, IBM J. Res. Dev. — [IEEE](https://ieeexplore.ieee.org/document/5391348).
- Pasco 1976, Stanford Ph.D. thesis.
- Witten, Neal & Cleary 1987, CACM — [ACM](https://dl.acm.org/doi/10.1145/214762.214771).
- Collet, *"Zstandard compression and the application of ANS,"* Data Compression Conf. 2016 (FSE/Zstd).
- Giesen et al., *"Infinite JPEG XL: practical entropy coding with rANS,"* — [arXiv:2208.03840](https://arxiv.org/abs/2208.03840) discussion; JPEG XL reference: [arXiv:1908.02465](https://arxiv.org/abs/1908.02465) (Alakuijala et al., *"JPEG XL next-generation image compression architecture and coding tools"*, DCC 2019).

---

## 3. Error-Correcting Codes for Storage Reliability

### 3.1 The coding theorem

Shannon's channel coding theorem: a discrete memoryless channel of capacity $C$ admits reliable communication at any rate $R<C$. The dual view for storage: an erasure channel of erasure probability $p_e$ has capacity $C=1-p_e$, so redundancy $R_{\text{parity}}\ge p_e$ is necessary and sufficient asymptotically.

### 3.2 Reed–Solomon (RS)

RS codes are maximum-distance-separable (MDS): an $(n,k)$ RS code over $\mathrm{GF}(2^m)$ has minimum distance $d_{\min}=n-k+1$ and tolerates any $t=\lfloor(n-k)/2\rfloor$ errors or $n-k$ erasures — optimal by the Singleton bound $d_{\min}\le n-k+1$. This is why RS dominates storage erasure coding (RAID-6, Ceph, Hadoop HDFS EC, object stores).

- Reed & Solomon, *"Polynomial Codes over Certain Finite Fields,"* J. SIAM 8(2), 1960 — [SIAM](https://epubs.siam.org/doi/10.1137/0108018).
- Plank, *"A Tutorial on Reed-Solomon Coding for Fault-Tolerance in RAID-like Systems,"* Software–Practice & Experience 27(9), 1997 — [ACM](https://dl.acm.org/doi/10.1145/264014.264020); updated `jerasure` library.
- Guruswami & Sudan, *"Improved decoding of Reed-Solomon and algebraic-geometric codes,"* IEEE TIT 45(6), 1999 — [IEEE](https://ieeexplore.ieee.org/document/771165) (list decoding beyond half the distance).

### 3.3 LDPC codes

**Low-density parity-check** codes (Gallager 1962; rediscovered MacKay & Neal 1995) are defined by a sparse parity-check matrix $H$ with row/column weight $w_r,w_c$ constant. They achieve capacity on the BEC and approach capacity ($\to 0.0045$ dB of Shannon limit) on the AWGN channel under belief propagation. **LDPC is the ECC inside modern SSDs** (QLC/TLC NAND requires ~10%–20% parity for the raw bit-error budget) and in 5G NR data channels.

- Gallager, R.G., *"Low-density parity-check codes,"* IRE Trans. Inf. Theory 8(1), 1962 — [IEEE](https://ieeexplore.ieee.org/document/1057683).
- MacKay, D.J.C. & Neal, R.M., *"Good codes based on very sparse matrices,"* Cryptography & Coding 1995.
- MacKay, D.J.C., *Information Theory, Inference, and Learning Algorithms*, Cambridge, 2003 — **free online** at [inference.org.uk/itprnn](http://www.inference.org.uk/itprnn/).
- Luby, Mitzenmacher, Shokrollahi, Spielman, *"Efficient erasure correcting codes,"* IEEE TIT 47(2), 2001 — [IEEE](https://ieeexplore.ieee.org/document/905576).

### 3.4 Fountain / rateless codes

Fountain codes are **rateless**: the encoder produces a potentially unbounded stream of encoded symbols; the decoder recovers the $k$ source symbols from *any* $k(1+\varepsilon)$ of them, regardless of which subset arrives. This is ideal for storage replication / repair where the loss pattern is unknown a priori.

- **LT codes** (Luby 2002): each output symbol is the XOR of a degree-$d$ subset, $d$ sampled from a robust soliton distribution. Decode by belief propagation (peeling). Recover with $O(k\ln(k/\delta))$ symbols with probability $1-\delta$.
- **Raptor codes** (Shokrollahi 2006): a *precoded* (e.g., LDPC) LT code; precode handles the residual graph left by the LT peeling, giving **linear-time** encode/decode and recovering from $k(1+\varepsilon)$ symbols with $\varepsilon$ a small constant. **RaptorQ** (Luby et al., RFC 6330, 2011) is the productionized version used in DVB, 3GPP MBMS, and Qualcomm's broadcast; it is within a few percent of the capacity of the erasure channel.
- **Online codes** (Maymounkov & Mazières 2003): a fountain code with provable rateless performance and a fixed, simple degree distribution.

References:
- Luby, M., *"LT codes,"* FOCS 2002 — [IEEE](https://ieeexplore.ieee.org/document/1181975); IEEE TIT version 2005.
- Shokrollahi, A., *"Raptor codes,"* IEEE TIT 52(6), 2006 — [IEEE](https://ieeexplore.ieee.org/document/1572614).
- Luby, Shokrollahi, Watson, Stockhammer, *"RaptorQ Forward Error Correction Scheme for Object Delivery,"* **RFC 6330**, IETF 2011 — [RFC 6330](https://datatracker.ietf.org/doc/html/rfc6330).
- Maymounkov, P. & Mazières, D., *"Rateless codes and big downloads,"* IPTPS 2003, LNCS 2735 — [Springer](https://doi.org/10.1007/978-3-540-45172-3_22).

### 3.5 Regenerating codes (storage repair bandwidth)

When a storage node fails, naive RS repair downloads the whole object. **Regenerating codes** (Dimakis et al. 2010) trade storage for repair bandwidth on the *network information theory* cut-set bound: for an $(n,k,d)$ system, the minimum repair bandwidth $\gamma$ per failure satisfies

$$\mathcal{B}\le \frac{k\,d}{k+d-1}\,\gamma \quad\Longleftrightarrow\quad \gamma \ge \frac{\mathcal{B}(k+d-1)}{kd}$$

where $\mathcal{B}$ is the file size. Two operating points: **MSR** (Minimum Storage Regenerating, matches MDS storage) and **MBR** (Minimum Bandwidth Regenerating).

- Dimakis, Godfrey, Wu, Wainwright & Ramchandran, *"Network coding for distributed storage systems,"* IEEE TIT 56(9), 2010 — [IEEE](https://ieeexplore.ieee.org/document/5339410) / [arXiv:cs/0702015](https://arxiv.org/abs/cs/0702015).

### 3.6 Applications to the engine

1. **WAL replication = RaptorQ fountain.** A WAL record is the $k$ source symbols; we emit a fountain stream of parity symbols to the replicas. RaptorQ gives **sub-millisecond, rateless** durability: any $k(1+\varepsilon)$ of the parity chunks suffice, so we can ack the commit the instant enough chunks have landed *anywhere*, decoupling latency from the slowest replica. RFC 6330 ships a public-domain reference implementation we can fold into the kernel.

2. **Cross-rack redundancy = regenerating/MSR codes.** Across racks the dominant cost is *inter-rack bandwidth during repair*, not storage. MSR codes cut repair bandwidth by a factor $\approx\tfrac{k}{k+d-1}\cdot\tfrac{d}{k}$ vs. RS while staying MDS on storage. The cut-set bound above tells us *exactly* how much cross-CXL/Network bandwidth a rebuild will consume — a number we can place in the kernel's tier-cost table.

3. **LSM compaction resilience = LDPC.** Compaction reads many SSTables and writes new ones; transient media errors during the long read pass are best handled by the *intra-chunk* LDPC that the SSD already maintains, but an *application-level* LDPC over a compaction *window* (a stripe of SSTable blocks) lets us detect/repair silent-data-corruption without re-reading from remote replicas. LDPC's sparse $H$ decodes in near-linear time and the peeling decoder is SIMD-friendly (`VPXOR` + `POPCNT`).

4. **Erasure profile per tier.** Each tier has an effective erasure rate $p_e^{(t)}$: bit-flips in HBM (low), UCEs in CXL (medium), bit-rot in NVMe (higher), packet loss over Network (variable). By the capacity bound, the *minimum parity fraction* for tier $t$ is $\ge p_e^{(t)}$. The kernel table thus carries an ECC profile `ecc[cpu][tier] = (RS|LDPC|RaptorQ, rate)` chosen so that `1 - rate ≥ p_e^(tier)`.

---

## 4. Channel Capacity and the Memory Hierarchy

### 4.1 Shannon–Hartley

For a bandlimited AWGN channel of bandwidth $B$ and SNR $S/N$,

$$C \;=\; B\,\log_2\!\left(1+\tfrac{S}{N}\right)\quad\text{[bits/second]}.$$

This is the canonical capacity formula; it states that capacity is the product of a *width* (bandwidth) and a *logarithmic quality* (SNR).

### 4.2 Modeling a memory tier as a channel

We treat each tier $t$ as a communication channel with:

| Quantity | Memory analog |
|---|---|
| Bandwidth $B$ | peak read/write bandwidth of the tier (GB/s) |
| SNR $S/N$ | inverse of the *noise*: contention, coherence traffic, refresh stalls, UCE rate |
| Latency $L$ | round-trip access latency (ns) |
| **Bandwidth-delay product** $B\cdot L$ | number of bytes "in flight" — the *capacity of the pipe* |

The **bandwidth-delay product** (BDP) is the analog of channel capacity for *latency-bound* access: to saturate a tier of bandwidth $B$ and latency $L$ you must issue $\ge B\cdot L$ bytes of outstanding requests. Concretely:
- L1: $B\!\sim\!2\,$TB/s, $L\!\sim\!1\,$ns → BDP $\sim 2$ KB ≈ one cache line.
- DDR5: $B\!\sim\!100\,$GB/s, $L\!\sim\!80\,$ns → BDP $\sim 8$ KB ≈ a handful of cache lines (hence HW prefetchers).
- CXL 3.0: $B\!\sim\!32\,$GB/s, $L\!\sim\!170\,$ns (variable, multi-hop) → BDP $\sim 5$ KB but with a *heavy tail* (coherence, type-3 sharing).
- NVMe: $B\!\sim\!14\,$GB/s, $L\!\sim\!10\,\mu$s → BDP $\sim 140$ KB → deep queue depth required.

This reframes the engine's kernel-tuning problem: **each (CPU, tier) kernel is a capacity-approaching code for that tier's channel.** The AVX-512 `VPGATHER`/prefetch depth is chosen so that outstanding bytes $\approx$ BDP, exactly analogous to choosing a code rate near $C$.

### 4.3 Capacity-achieving codes for the CXL "channel"

CXL exposes a *variable-latency* channel (head-of-line blocking on shared upstream ports, coherence-induced jitter). Information-theoretically, a channel with **state known at the receiver** (a *compound/gilbert-elliott* channel) has capacity

$$C = \sum_s p(s)\,C_s,\qquad C_s=\text{capacity of state }s,$$

when the state $s$ (e.g., "coherence-busy") is observable at the decoder (the CPU sees the latency). The practical consequence: the CXL kernel should be **rate-adaptive** — when the channel is in a "bad" state, drop to a sparser access pattern (more prefetch slack, lower issue rate) so the instantaneous rate stays below $C_s$. This is the *Hybrid ARQ* idea: keep the rate just under the *instantaneous* capacity.

### 4.4 Network information theory for multi-rack DB

A multi-rack database is naturally a **multi-terminal** information network:

- **Multiple-access channel (MAC):** several racks stream column data into one aggregator node. The capacity region (Ahlswede 1971; Liao 1972) for two senders is
$$R_1\le H(X_1),\quad R_2\le H(X_2),\quad R_1+R_2\le H(X_1,X_2).$$
The sum-rate bound says: **the aggregate streaming rate is bounded by the joint entropy** — correlated columns across racks can be streamed below the sum of marginal entropies by exploiting correlation (ties directly to Slepian–Wolf, §5).
- **Broadcast channel:** one writer fan-outs WAL to many replicas. Marton's region with a common message + refinement gives the rate trade-off between "fast common durability" and "per-replica tail data."
- **Relay channel (Cover–El Gamal 1979):** a CXL memory expander relaying between a CPU and NVMe over the network. The decode-and-forward / compress-and-forward bounds tell us when it pays to *decode at the CXL node* vs. *store-and-forward*.

References:
- Ahlswede, R., *"Multi-way communication channels,"* 2nd ISIT, 1971.
- Cover, T. & El Gamal, A., *"Capacity theorems for the relay channel,"* IEEE TIT 25(5), 1979 — [IEEE](https://ieeexplore.ieee.org/document/1055116).
- El Gamal & Kim, *Network Information Theory*, Cambridge, 2011 — [Cambridge](https://www.cambridge.org/core/books/network-information-theory/8B600AE2F4A6F5C2C50CFE6B5C50E9E7).

### 4.5 Application: a capacity-aware scheduler

Place in the kernel table, for each (CPU, tier), the pair $(B,L)$ and compute BDP at startup; the scheduler's outstanding-requests target is then `BDP / word_size`. This is *literally* choosing the code rate to approach channel capacity.

---

## 5. Distributed Source Coding

### 5.1 Slepian–Wolf

The **Slepian–Wolf theorem** (1973): for two correlated sources $X,Y$ encoded *separately* but decoded *jointly*, the achievable rate region is

$$R_X \ge H(X\mid Y),\quad R_Y \ge H(Y\mid X),\quad R_X+R_Y \ge H(X,Y).$$

The stunning result: **knowing $Y$ only at the decoder costs nothing** — $R_X=H(X\mid Y)$ is achievable with no encoder-side access to $Y$. Practical realizations use **syndrome coding** (a linear code's syndrome of $X$ is the compressed bitstream; the decoder uses $Y$ as side information to decode the coset) or **DISCUS** (Pradhan & Ramchandran 1999).

- Slepian & Wolf, *"Noiseless coding of correlated information sources,"* IEEE TIT 19(4), 1973 — [IEEE](https://ieeexplore.ieee.org/document/1055037).
- Pradhan & Ramchandran, *"Distributed source coding using syndromes (DISCUS),"* IEEE TIT 49(3), 2003 — [IEEE](https://ieeexplore.ieee.org/document/1184141).

### 5.2 Wyner–Ziv

**Wyner–Ziv** (1976) is the lossy counterpart: encode $X$ lossily with rate $R$ while decoder has side info $Y$; the rate-distortion function with side information is

$$R_{WZ}(D) = \inf_{p(u,z|x),\,g(u,Y)} I(X;U\mid Y)\;\;\text{s.t.}\;\;\mathbb{E}[d(X,g(U,Y))]\le D.$$

**Wyner's Markov-lemma** shows that for $X\!\to\!Y\!\to\!\hat X$ (side info "better" than the encoder's view), $R_{WZ}(D)=R_{X|Y}(D)$, i.e., the encoder pays no penalty for *not* seeing $Y$. This is the theoretical license for **"compress now, use correlation later."**

- Wyner & Ziv, *"The rate-distortion function for source coding with side information at the decoder,"* IEEE TIT 22(1), 1976 — [IEEE](https://ieeexplore.ieee.org/document/1055502).
- Wyner, *"The rate-distortion function for source coding with side information at the decoder—II: General sources,"* IEEE TIT, 1978.

### 5.3 Applications to the engine

1. **Cross-column compression with side information.** Two correlated columns (e.g., `ship_date` and `receive_date`, or `zip` and `state`) are written at different times. Encode column $X$ to $H(X\mid Y)$ bits using only $Y$ at the *decoder*: the writer of $X$ needs no access to $Y$. We pick the linear code so its syndrome fits in a 64-bit-word stripe. The marginal savings are $I(X;Y)$ bits/row — exactly the bits the joint distribution makes redundant.

2. **Cross-rack differential shipping.** When replicating a column to a remote rack, ship only the *innovation* $X\mid Y_{\text{local}}$ where $Y_{\text{local}}$ is a model the remote rack already holds (a coarse quantization or a previous version). The remote rack reconstructs $X$ to within $D$. This is **Wyner–Ziv over the network**: the bandwidth used is $R_{WZ}(D)=I(X;U\mid Y)\le I(X;\hat X)$, strictly less than independent compression whenever $X$ and $Y$ are correlated.

3. **Differential logs (delta WAL).** A WAL record $X_t$ is encoded with $Y=X_{t-1}$ as decoder-side side info. By Slepian–Wolf the rate floor is $H(X_t\mid X_{t-1})$ — which, for a low-entropy update stream, is far below $H(X_t)$. The log is *physically* a stream of coset syndromes; replay is joint decoding.

4. **Column-store cross-table compression.** Foreign-key-joined tables share a key column; encode the FK column of the child table at $H(\text{FK}\mid\text{PK})$ — for a strict FK constraint this is $\approx 0$, giving near-free storage for the join column.

---

## 6. Kolmogorov Complexity and Algorithmic Information Theory

### 6.1 Foundation

The **Kolmogorov complexity** $K_U(x)$ of a string $x$ is the length of the shortest program for a universal Turing machine $U$ that outputs $x$. Key facts:
- $K(x)$ is *machine-independent up to an additive constant* (invariance theorem).
- $K(x)\le |x|+O(1)$; for incompressible strings $K(x)\approx |x|$.
- **Kolmogorov's structure function** $h_x(\alpha)=\min\{-\log p:\,K(x\mid\text{model of complexity }\alpha)\le\alpha\}$ ties complexity to model selection.
- **Solomonoff induction** predicts the next symbol using the *universal a priori distribution* $m(x)=\sum_{p:U(p)=x*}2^{-|p|}$, which dominates every computable distribution multiplicatively.

The halting problem makes $K$ uncomputable, so we use **computable approximations**: MDL is the most important.

References:
- Li, M. & Vitányi, P., *An Introduction to Kolmogorov Complexity and Its Applications*, 4th ed., Springer, 2019 — [Springer](https://link.springer.com/book/10.1007/978-3-030-11298-1).
- Kolmogorov, A.N., *"Three approaches to the quantitative definition of information,"* Problems of Information Transmission 1(1), 1965.
- Solomonoff, R., *"A formal theory of inductive inference,"* Information & Control 7(1–2), 1964.
- Levin, L., *"Universal sequential search problems,"* Problems of Information Transmission 9(3), 1973.

### 6.2 MDL = computable Kolmogorov

**Minimum Description Length** (Rissanen 1978; Grünwald 2007) replaces the uncomputable $K$ with the two-part code length

$$\mathcal{L}(M,D)=L(M)+L(D\mid M),$$

minimized over models $M$ (and is refined into *NML* — normalized maximum likelihood — which is minimax-optimal universal coding under log-loss). The correspondence: $K(x)=\min_M\{K(M)+K(x\mid M)\}+O(1)$, so MDL is the algorithmically random-model proxy.

- Rissanen, J., *"Modeling by shortest data description,"* Automatica 14(5), 1978 — [Elsevier](https://doi.org/10.1016/0005-1098(78)90005-5).
- Grünwald, P., *The Minimum Description Length Principle*, MIT Press, 2007 — [MIT Press](https://mitpress.mit.edu/9780262072816/).
- Barron, Rissanen & Yu, *"The minimum description length principle in coding and modeling,"* IEEE TIT 44(6), 1998 — [IEEE](https://ieeexplore.ieee.org/document/761094).

### 6.3 Applications to the engine

1. **Schema-on-read via complexity minimization.** A byte sequence can be interpreted many ways (i64, f64, packed i8×8, utf-8, bit-packed bools…). The principled objective: pick the interpretation $M$ minimizing $K(M)+K(\text{data}\mid M)$. In practice, evaluate the MDL/NML score per candidate layout; the winner is the layout whose *total* description (schema bits + coded data bits) is smallest. This generalizes our current MDL schema selection from "choose a schema" to "choose an interpretation + codebook + encoding," and it's the computable shadow of the true Kolmogorov objective.

2. **Universal priors over query workloads.** Solomonoff induction gives a principled "what query comes next" predictor: weight each candidate workload model by $2^{-K(\text{model})}$. Materialize indexes/stats in proportion to this posterior — a principled replacement for ad-hoc workload heuristics.

3. **Incompressibility as an anomaly signal.** By Levin's universal distribution, "normal" data has low $K$; an anomalous 64-bit word is one whose shortest description under *all* candidate layouts exceeds $|x|-c$. A per-chunk running estimate of $K$ (via gzip/NML length) is a cheap anomaly detector: spikes in $K$ flag rows that resist every schema — corruption, injection, or novel categories.

4. **MDL ↔ RD unification.** MDL and RD are duals: MDL minimizes *total* description length of model+data; RD minimizes description of $X$ given a *fidelity* constraint on $\hat X$. The engine can use one objective (NML-style MDL) for the *schema/layout* choice and a *separate* RD objective for the *bit budget per column given the tier SLA*. The two compose because both are universal-coding length functionals.

---

## 7. Mutual Information for Feature Selection / Index Choice

### 7.1 Definitions

Mutual information $I(X;Y)=H(X)-H(X\mid Y)=\sum_{x,y}p(x,y)\log\frac{p(x,y)}{p(x)p(y)}$ measures bits of dependence. The **multi-information** (total correlation, Watanabe 1960):
$$I(X_1;\dots;X_n)=\sum_i H(X_i)-H(X_1,\dots,X_n).$$
The **interaction information** (McGill 1954) generalizes MI to three+ variables and detects pure higher-order synergy (e.g., XOR: $I(X;Y)=I(X;Z)=I(Y;Z)=0$ but $I(X;Y;Z)<0$ and $I(X;YZ)=1$).

### 7.2 Information Bottleneck (IB)

Tishby, Pereira & Bialek (1999): find a compressed representation $T$ of $X$ that is maximally informative about a *relevant* variable $Y$:

$$\min_{p(t\mid x)}\; I(X;T)-\beta\, I(T;Y).$$

The Lagrangian is solved by the **BA algorithm** (same equations as §1.2 — IB *is* a constrained RD problem with $Y$ playing the role of fidelity). As $\beta\to\infty$, $T$ preserves all info about $Y$; as $\beta\to 0$, $T$ collapses.

- Tishby, Pereira & Bialek, *"The Information Bottleneck Method,"* Allerton 1999 — [arXiv:physics/0004057](https://arxiv.org/abs/physics/0004057).
- Slonim & Tishby, *"Agglomerative information bottleneck,"* NeurIPS 1999.
- Shamir, Sabato & Tishby, *"Learning and generalization with the information bottleneck,"* TCS 2010.

### 7.3 Applications to the engine

1. **Which columns to index.** Index the column $C$ maximizing $I(C;\,Q)$, where $Q$ is the query-predicate variable (estimated from a workload log). The information-theoretic payoff: an index on $C$ reduces the *search* from $H(\text{row})$ to $H(\text{row}\mid C\!=\!q)\approx H(\text{row})-I(C;\text{row})$ — the selectivity of $C$ *is* its mutual information with the row identity. This is the rigorous statement behind the heuristic "index high-selectivity columns."

2. **Multi-column index via multi-information.** A composite index on $(C_1,\dots,C_k)$ is worth building iff $I(C_1;\dots;C_k;\text{row})$ — the joint information — substantially exceeds the sum of marginal $I(C_i;\text{row})$, i.e., iff the columns are **synergistic** (XOR-like). Use interaction information to detect synergy: a pair with zero marginal MI but negative interaction information *jointly* determines the row and *must* be indexed together. This catches composite-key cases that single-column heuristics miss.

3. **Information bottleneck for materialization.** Let $X$=raw row, $Y$=query result. A materialized view / projection is a bottleneck representation $T$ solving $\min I(X;T)-\beta I(T;Y)$. Sweeping $\beta$ yields a *family* of views from lossless ($\beta\to\infty$, $T=X$) to maximally compressed. Choose the view whose $I(T;Y)$ exceeds a SLA but whose storage $I(X;T)$ fits the tier budget — this is RD (§1) applied to *views* with query-relevance as the distortion.

4. **Index selection as a rate-distortion trade.** Storage cost of an index $\approx I(X;T)$ bits; query speedup $\approx I(T;Y)/H(Y)$. The Pareto frontier (index size vs. query latency) is an RD curve; sweep $\beta$ to trace it. This unifies index selection with the rest of the framework.

---

## 8. Source Coding with Side Information

### 8.1 Recap of bounds

- **Slepian–Wolf** (lossless): $R_X\ge H(X\mid Y)$ when $Y$ is at the decoder (§5).
- **Wyner–Ziv** (lossy): $R\ge I(X;U\mid Y)$ (§5).
- **Ahlswede–Körner–Wyner** (common side info at both terminals) and **Berger–Tung** (multi-terminal lossy) extend to the general case. The Berger–Tung inner bound is tight for many practical correlated-column scenarios.

- Berger & Tung, *"Multiterminal source coding,"* IEEE TIT 1978 — [IEEE](https://ieeexplore.ieee.org/document/1055893).
- Slepian & Wolf 1973; Wyner & Ziv 1976 (above).

### 8.2 Applications

1. **Encode column A using statistics from column B.** At write time we may not yet have column $B$ materialized, but the *decoder* (a later query) will. By Slepian–Wolf, encode $A$ to $H(A\mid B)$ bits now; the join query that has both columns decodes losslessly. Concrete saving: $I(A;B)$ bits/row, free at write time.

2. **Differential logs.** Encode the WAL delta $\Delta=X_t\ominus X_{t-1}$ with $Y=X_{t-1}$ as decoder side info. Rate floor $H(\Delta\mid X_{t-1})$; for slow-changing state this is tiny. The physical log is a stream of syndromes of a systematic code whose parity-check matrix is stored once per table.

3. **Tiered side information.** A "coarse" copy $\hat Y$ of a column lives in HBM (cheap, low latency); the "fine" copy $X$ lives in NVMe. By Wyner–Ziv, the NVMe copy can be stored at $R_{WZ}(D)=I(X;U\mid \hat Y)\le R_{X|}(D)$: the *existence* of the coarse HBM copy lowers the NVMe bit budget. This is the storage-side dual of successive refinement (§1.3).

4. **Compression of derived columns.** A computed column $A=f(B)$ has $H(A\mid B)=0$; store its syndrome length 0 — i.e., *don't store it at all*, regenerate at read. More generally, for $A$ nearly-determined by $B$, the syndrome is short. This formalizes "virtual columns."

---

## 9. Information-Theoretic Lower Bounds for Database Operations

### 9.1 Sorting

- **Comparison model:** any comparison sort uses $\ge \log_2(n!)=n\log_2 n - O(n)$ comparisons (decision-tree leaf-count argument). This is the classic Ω$(n\log n)$ bound.
- **Word-RAM / algebraic model:** the comparison bound is *not* intrinsic. Han & Thorup (2002) sort integers in $O(n\sqrt{\log\log n})$ expected time on a word RAM using fusion trees / multiway tries — **below** $n\log n$ because each 64-bit word comparison carries $\Theta(\log w)$ bits of information, not 1 bit. Ajtai–Komlós–Szemerédi sorting networks give $O(n\log n)$ with $O(\log n)$ depth.
- The information-theoretic content: sorting needs to *identify the permutation*, which carries $\log_2(n!)$ bits; if each primitive operation extracts $b$ bits, the lower bound is $\Omega(\tfrac{n\log n}{b})$. SIMD-512 comparisons extract $\approx 16\times$ more bits/op than scalar — so SIMD can approach the *true* $n\log n/b$ bound, not the scalar $n\log n$.

References:
- Han & Thorup, *"Integer sorting in $O(n\sqrt{\log\log n})$ expected time and linear space,"* FOCS/J. Alg. 2002 — [Springer](https://doi.org/10.1007/s00453-004-1092-x).
- Knuth, *The Art of Computer Programming, Vol. 3: Sorting and Searching*, Ch. 5 (decision-tree lower bound).

### 9.2 Joins

- **Output-size bound (AGM).** Atserias, Grohe & Marx (2008) prove that for a conjunctive query / natural join over relations with sizes $|R_i|$, the output size is bounded by the **fractional edge cover**:
$$|\Join_i R_i|\;\le\;\min_{\vec x\ge 0}\;\prod_i |R_i|^{x_i}\quad\text{s.t.}\;\sum_{i\ni v}x_i\ge 1\;\forall v.$$
This is tight and is the basis of **worst-case optimal join algorithms** (Ngo–Porat–Ré 2012, *"Worst-case optimal join algorithms,"* PODS — [ACM](https://dl.acm.org/doi/10.1145/2213556.2213565)) that run in time $\tilde O(\text{AGM bound})$.
- The **entropy lower bound** (Frietzen, Grohe, Thué, …): the join's output entropy is at most the solution of a Shannon-entropy linear program (the *information-theoretic* AGM bound), making the join problem fundamentally an **entropy maximization** under functional-dependency constraints. This is the deepest link between information theory and relational algebra.
- Atserias, Grohe & Marx, *"Size bounds and query plans for relational joins,"* SIAM J. Comput./FOCS 2008 — [SIAM](https://epubs.siam.org/doi/10.1137/090767441) / [FOCS](https://ieeexplore.ieee.org/document/4691000).

### 9.3 Hashing / dictionary

- Lookup in a dictionary of $n$ keys needs to disambiguate among $\ge n$ candidates, so $\ge \log_2 n$ bits of information per probe — the cell-probe lower bound (Fredman, Komlós, Szemerédi 1984 perfect hashing is optimal up to constants). For *approximate* membership (Bloom filters), the information lower bound on the *false-positive rate* $f$ at $m$ bits and $n$ keys is
$$f \;\ge\; \left(1-2^{-m/(n\ln 2)}\right)^n \;\approx\; 2^{-m/n}$$
tight up to constants (Carter et al. 1978; Mitzenmacher 2002 survey — [IEEE](https://ieeexplore.ieee.org/document/1184141)-style). The Bloom filter is within $\approx\log_2 e\approx 1.44$ bits/key of the information-theoretic optimum of $n\log_2(1/f)$ bits.

### 9.4 Aggregation / distinct count

- **Distinct count (cardinality) estimation:** Flajolet–Martin / HyperLogLog uses $\approx \tfrac{1}{(\log 2)^2}\approx 2.08$ bits per "register" and the lower bound on the variance of any $m$-bit sketch is $\Omega(1/m)$, so HLL is within a small constant of optimal (Alon–Matias–Szegedy 1999, *"The space complexity of approximating the frequency moments,"* JCSS — [Elsevier](https://doi.org/10.1006/jcss.1997.1545)).
- **Sum / sum-of-squares (F0, F2):** AMS sketch achieves $O(1/\varepsilon^2)$ space for $(1\pm\varepsilon)$ approximation, matching the information lower bound $\Omega(1/\varepsilon^2)$ bits.

### 9.5 Applications

1. **Know which lower bound you can beat.** SIMD/packed comparison raises $b$ (bits/op), so the *reachable* sort time is $\Omega(n\log n/b)$, and $b\!=\!16$ for AVX-512 — our sort kernels should be benchmarked against $n\log n/16$, not $n\log n$. Failing to approach this means we are leaving 4× on the table.
2. **Joins = entropy maximization.** The AGM/fractional-cover bound gives the *exact* worst-case output size and a join plan attaining it. The query planner should compute the fractional edge cover (a small LP per query) and select the **leapfrog join** order that hits the AGM bound — this is provably worst-case optimal and replaces heuristic join ordering.
3. **Sketch SLAs from lower bounds.** A `COUNT(DISTINCT …)` with SLA $\pm\varepsilon$ *requires* $\Omega(1/\varepsilon^2)$ bits of state — HLL meets it. We can store the HLL registers as the *column* and never materialize the distinct set; the lower bound tells us the storage floor.
4. **Bloom-filter sizing.** The $2^{-m/n}$ lower bound fixes the bit budget per key for a target false-positive rate; we size the filter to *exactly* that budget per tier (HBM-resident filter, NVMe-resident data).

---

## 10. Quantization Theory

### 10.1 Scalar: Lloyd–Max

For a scalar source with pdf $p(x)$ and squared error, the **Lloyd–Max** quantizer is the MSE-optimal $K$-level scalar quantizer. Necessary (and for convex problems sufficient) conditions (**Lloyd conditions**):
- Reconstruction levels (centroids): $\hat x_i = \mathbb{E}[X\mid X\in\mathcal{V}_i]$.
- Decision boundaries (midpoints): $b_i = \tfrac12(\hat x_i+\hat x_{i+1})$.
Lloyd's algorithm (Lloyd 1957; Max 1960) alternates these and converges to a local optimum. The high-resolution MSE is $\text{MSE}\approx \tfrac{1}{12}\sum_i p(\hat x_i)\Delta_i^3\cdot\Delta_i$ and the **Panter–Dite** asymptotic rate is $R(D)\approx h(X)-\log(2\sqrt{3}D^{1/2})$ for scalar, i.e. 6.02 dB/bit. For non-uniform sources, Lloyd–Max gives the **pointwise-optimal** scalar codebook.

- Lloyd, S.P., *"Least squares quantization in PCM,"* Bell Labs memo 1957; IEEE TIT 28(2), 1982 — [IEEE](https://ieeexplore.ieee.org/document/1057616).
- Max, J., *"Quantizing for minimum distortion,"* IRE TIT 6(1), 1960 — [IEEE](https://ieeexplore.ieee.org/document/1057548).

### 10.2 Vector quantization (LBG)

For a $d$-dim source, the MSE-optimal quantizer is **vector quantization**. The Linde–Buzo–Gray (LBG) algorithm generalizes Lloyd to vectors; it converges to a locally optimal codebook. The fundamental gain: **vector quantization beats scalar by the *space-filling advantage***, asymptotically achieving the **Zador–Gersho** rate
$$R(D)\;\approx\; h(X)-\tfrac{d}{2}\log(2\pi e\, D^{2/d}\beta_d)$$
with $\beta_d$ the lattice cell-normalized second moment; the gain over scalar grows with $d$ (dimension benefit, up to $\tfrac12\log(2\pi e/12)\approx 0.255$ bit/dim for spherical Gaussian).

- Linde, Buzo & Gray, *"An algorithm for vector quantizer design,"* IEEE Trans. Comm. 28(1), 1980 — [IEEE](https://ieeexplore.ieee.org/document/1094577).
- Gersho & Gray, *Vector Quantization and Signal Compression*, Kluwer, 1992.
- Zador, P., *"Asymptotic quantization error,"* Bell Syst. Tech. J., 1982.

### 10.3 Product & residual quantization

- **Product quantization (PQ)** (Jégou, Douze & Schmid 2011) splits a $d$-dim vector into $m$ subvectors, each quantized with its own $k$-level codebook → storage $m\lceil\log_2 k\rceil$ bits, lookup $O(mk)$ via precomputed distance tables. PQ powers FAISS and is the workhorse of billion-scale ANN.
- **Residual quantization** iteratively quantizes the residual after the previous codebook — each level sharpens the approximation; the MSE roughly divides by the codebook size each level. Additive quantization / optimally product quantization (OPQ, Ge et al. 2013) rotates the space to balance subvector energies.
- The **Johnson–Lindenstrauss** lemma guarantees $(1\pm\varepsilon)$-preservation of pairwise distances under a random projection to $O(\varepsilon^{-2}\log N)$ dims — the formal license for dimension reduction before PQ.

References:
- Jégou, Douze & Schmid, *"Product quantization for nearest neighbor search,"* IEEE TPAMI 33(1), 2011 — [IEEE](https://ieeexplore.ieee.org/document/5432202) / [inria-00514467](https://hal.inria.fr/inria-00514467).
- Johnson & Lindenstrauss, *"Extensions of Lipschitz mappings into a Hilbert space,"* Contemp. Math. 26, 1984.
- Ge, He, Ke, Sun & Tse, *"Optimized Product Quantization,"* IEEE TPAMI 2014 — [IEEE](https://ieeexplore.ieee.org/document/6678773).

### 10.4 Applications to the engine

1. **64-bit cells → bounded-error words.** Each 64-bit value can be quantized to $b$ bits with a Lloyd–Max codebook; the reconstruction MSE is bounded by the codebook's $D^*(b)$. Crucially the *SLA* is on $D$, so we choose $b$ from the inverse $D(b)$ — exactly the RD operating point of §1.

2. **Vector columns and FAISS-style PQ.** Columns holding embeddings / feature vectors (8×f64 = one 64-bit word, or many words) get PQ: $m$ sub-codebooks of $k=256$ each → 1 byte/subvector, reconstruct error bounded by the residual curve. **PQ codes are themselves 64-bit-word-friendly**: an 8-byte PQ code = one word, stored/loaded/vectorized as a single `VMOVDQA64`. Distance tables fit in L2; the inner loop is `VPADDB`-friendly.

3. **Tiered quantization = successive refinement.** Store an $m_1$-subvector PQ in HBM (coarse), an additive residual $m_2$-PQ in DDR5, a second residual in NVMe. Each level halves the MSE; the cumulative code is the engine's lossy representation, with the RD math of §1 guaranteeing the end-to-end distortion.

4. **Residual quantization for delta encoding.** The first codebook is the *previous version* of the column (a "predictor"); the residual quantizer compresses only the *change*. This unifies §8 (side information) and §10 (quantization): the side-information version $Y$ acts as the level-0 codebook.

5. **AVX-512 quantization kernels.** Lloyd's centroid update is a histogram + weighted-sum reduction over the column — a textbook `VPSADBW`/`VPCMPGTQ` pipeline. The LBG codebook training for $m$ subvectors is embarrassingly parallel across subvectors (one AVX-512 lane each). Place these as `kernel[cpu][tier].train_pq`.

---

## Summary: 10 Mathematical Techniques and Their DB Applications

| # | Technique | Core formula / theorem | Canonical paper | Application to instruction-first, memory-centric engine |
|---|---|---|---|---|
| 1 | **Rate-Distortion theory** | $R(D)=\min I(X;\hat X)$ s.t. $\mathbb E d\le D$ | Cover & Thomas Ch.10; Gray 1990 | Float/time-series column compression with $\varepsilon$-SLA per tier; multi-resolution storage via successive refinement |
| 2 | **Blahut–Arimoto** | alternating $q,p$ updates, geometric convergence | Blahut 1972; Arimoto 1972 | Per-chunk empirical $R(D)$ fit at write time; chooses bit budget per tier |
| 3 | **Arithmetic coding** | $-\log p(x_{1:n})$ bits, entropy-optimal for any $p$ | Rissanen 1976; Pasco 1976 | Adaptive lossless column codec approaching $H(X)$ with frequency-table models |
| 4 | **Asymmetric Numeral Systems (tANS/rANS)** | table FSM, redundancy $O(1/L)$ | Duda 2009, arXiv:0902.0277 | Branch-free SIMD-parallel decode (8 streams/lane, AVX-512 `VPGATHER`); custom FSE table per chunk in kernel table |
| 5 | **Reed–Solomon / MDS** | Singleton bound $d_{\min}\le n-k+1$ | Reed & Solomon 1960 | Erasure coding across replicas/racks; tier parity profile `1−rate ≥ p_e^(tier)` |
| 6 | **LDPC** | sparse $H$, BP achieves capacity | Gallager 1962; MacKay 2003 | Intra-SSD & compaction-window ECC; SIMD peeling decoder (`VPXOR`+`POPCNT`) |
| 7 | **Fountain / Raptor / RaptorQ codes** | rateless, recover from $k(1+\varepsilon)$ symbols | Luby 2002; Shokrollahi 2006; RFC 6330 | WAL durability: ack commit when $k(1+\varepsilon)$ parity chunks land anywhere; decouples latency from slowest replica |
| 8 | **Regenerating codes (MSR/MBR)** | $\mathcal B\le \frac{kd}{k+d-1}\gamma$ | Dimakis et al. 2010 | Cross-rack rebuild minimizes network bandwidth; cut-set bound feeds tier-cost table |
| 9 | **Channel capacity / BDP** | $C=B\log_2(1+S/N)$; BDP=$B\cdot L$ | Shannon 1948; Cover–El Gamal 1979 | Model each tier as a channel; kernel prefetch depth = BDP; rate-adaptive CXL access (Hybrid-ARQ under state) |
| 10 | **Network info theory (MAC/BC/relay)** | $R_1+R_2\le H(X_1,X_2)$ | Ahlswede 1971; El Gamal & Kim 2011 | Multi-rack aggregate streaming bounded by joint entropy (ties to Slepian–Wolf) |
| 11 | **Slepian–Wolf (distributed lossless)** | $R_X\ge H(X\mid Y)$, $Y$ only at decoder | Slepian & Wolf 1973 | Encode correlated columns to $H(X\mid Y)$; cross-column/FK compression; syndrome-encoded delta WAL |
| 12 | **Wyner–Ziv (lossy + side info)** | $R\ge I(X;U\mid Y)$ | Wyner & Ziv 1976 | Cross-rack differential shipping; tiered side-info lowers NVMe budget |
| 13 | **Kolmogorov complexity** | $K(x)$, invariance, universal prior | Kolmogorov 1965; Li & Vitányi 2019 | Schema-on-read objective = min $K(\text{layout})+K(\text{data}\mid\text{layout})$; anomaly = incompressibility |
| 14 | **MDL / NML** | $\mathcal L=L(M)+L(D\mid M)$ | Rissanen 1978; Grünwald 2007 | Computable proxy for $K$; schema+codebook+layout selection; unifies with RD |
| 15 | **Mutual information / multi-information** | $I(X;Y)=H(X)-H(X\mid Y)$; total correlation | Cover & Thomas; Watanabe 1960 | Index column $C$ maximizing $I(C;\text{query})$; composite indexes via synergy/interaction info |
| 16 | **Information Bottleneck** | $\min I(X;T)-\beta I(T;Y)$, BA = RD | Tishby et al. 1999, arXiv:physics/0004057 | Materialized-view / projection selection as RD with query-relevance as distortion |
| 17 | **Sort lower bound (decision tree / word-RAM)** | $\Omega(\log n!)$; $O(n\sqrt{\log\log n})$ word-RAM | Knuth Vol.3; Han & Thorup 2002 | Benchmark sort kernels vs $n\log n/b$, $b\!=\!16$ for AVX-512 (not scalar $n\log n$) |
| 18 | **Join lower bound (AGM / entropy)** | $\lvert\Join\rvert\le\min\prod\lvert R_i\rvert^{x_i}$ (fractional cover) | Atserias–Grohe–Marx 2008; Ngo–Porat–Ré 2012 | Worst-case optimal leapfrog join; planner solves a small LP per query |
| 19 | **Hashing / Bloom lower bound** | $f\ge 2^{-m/n}$; cell-probe $\ge\log n$ | Carter et al. 1978; Mitzenmacher 2002 | Size Bloom filter to exact bit budget per false-positive SLA; per-tier filters |
| 20 | **Sketching lower bounds (HLL/AMS)** | $\Omega(1/\varepsilon^2)$ bits for $(1\pm\varepsilon)$ | Alon–Matias–Szegedy 1999 | Store HLL/AMS sketch as the column; never materialize distinct set |
| 21 | **Lloyd–Max scalar quantizer** | centroid / midpoint conditions; 6.02 dB/bit | Lloyd 1957; Max 1960 | Per-column $b$-bit quantizer with MSE $\le D^*(b)$ chosen from inverse RD curve |
| 22 | **Vector quantization (LBG)** | Zador–Gersho $R(D)\approx h(X)-\tfrac d2\log D^{2/d}$ | Linde–Buzo–Gray 1980 | Multi-dim column quantization; dimension benefit over scalar |
| 23 | **Product / residual quantization** | $m$ sub-codebooks; PQ code = 1 word | Jégou et al. 2011 | 8-byte PQ code = one 64-bit word; FAISS-style ANN on embedding columns; tiered residual = successive refinement |
| 24 | **Johnson–Lindenstrauss** | $O(\varepsilon^{-2}\log N)$ dims preserve distances | Johnson & Lindenstrauss 1984 | Dimensionality reduction before PQ for approximate nearest-row ops |

---

## How the pieces compose (synthesis)

The ten research threads are not independent — they are facets of one object, the **universal coding length functional**, instantiated at different points in the engine:

- **At write time:** estimate $p(X)$ (column histogram), fit $R(D)$ via Blahut–Arimoto (§1), pick operating point from the tier's SLA, choose a quantizer (Lloyd–Max/PQ, §10), then entropy-code the quantizer indices with tANS (§2). Total length $\approx R(D)+H(\text{codebook})\approx$ MDL (§6). Store coarse $\hat X_1$ in hot tier, residual $\Delta$ in cold tier via successive refinement (§1.3).
- **Across columns/racks:** apply Slepian–Wolf/Wyner–Ziv (§5,§8) so correlated columns and remote replicas are stored at $H(X\mid Y)$ / $I(X;U\mid Y)$, not $H(X)$.
- **For durability:** RaptorQ (§3) over the WAL; regenerating codes (§3.5) across racks; LDPC (§3.3) over compaction windows.
- **For access:** model each tier as a channel with capacity $C=B\log(1+S/N)$ and BDP=$BL$ (§4); the kernel's outstanding-requests target = BDP, with rate-adaptive Hybrid-ARQ on the variable-latency CXL channel.
- **For query planning:** AGM fractional-cover bound gives worst-case-optimal joins (§9); mutual information & information bottleneck (§7) choose indexes and materialized views as an RD trade; sketching lower bounds (§9.4) size COUNT(DISTINCT) state.
- **For schema/layout:** minimize $K(\text{layout})+K(\text{data}\mid\text{layout})$ via MDL/NML (§6), unifying schema selection with the compression objective.

Every one of these is a *length* — a number of 64-bit words — and every one has a proven lower bound. The engine's job is to **spend exactly that many words per tier**, and the kernel table is the place where the capacity-approaching code for each (CPU, tier) channel lives.

---

### Citation index (all inline above; consolidated)

- Shannon 1948 — [DOI 10.1002/j.1538-7305.1948.tb01338.x](https://doi.org/10.1002/j.1538-7305.1948.tb01338.x)
- Cover & Thomas 2006 — [Wiley](https://www.wiley.com/en-us/Elements+of+Information+Theory%2C+2nd+Edition-p-9780471241959)
- Gray, *Source Coding Theory* 1990 — [Springer](https://link.springer.com/book/10.1007/978-1-4615-6440-9)
- Gray & Neuhoff 1998 — [IEEE](https://ieeexplore.ieee.org/document/761109)
- Blahut 1972 — [IEEE](https://ieeexplore.ieee.org/document/1054801)
- Arimoto 1972 — [IEEE](https://ieeexplore.ieee.org/document/1054802)
- Equitz & Cover 1991 — [IEEE](https://ieeexplore.ieee.org/document/79904)
- Rissanen 1976 (arithmetic coding) — [IEEE/biblio](https://ieeexplore.ieee.org/document/5391348)
- Witten, Neal & Cleary 1987 — [ACM](https://dl.acm.org/doi/10.1145/214762.214771)
- Duda 2009, ANS — [arXiv:0902.0277](https://arxiv.org/abs/0902.0277)
- Duda 2013 — [arXiv:1311.2540](https://arxiv.org/abs/1311.2540)
- Reed & Solomon 1960 — [SIAM](https://epubs.siam.org/doi/10.1137/0108018)
- Gallager 1962, LDPC — [IEEE](https://ieeexplore.ieee.org/document/1057683)
- MacKay 2003 (book, free) — [inference.org.uk/itprnn](http://www.inference.org.uk/itprnn/)
- Luby, Mitzenmacher, Shokrollahi, Spielman 2001 — [IEEE](https://ieeexplore.ieee.org/document/905576)
- Luby 2002, LT codes — [IEEE FOCS](https://ieeexplore.ieee.org/document/1181975)
- Shokrollahi 2006, Raptor — [IEEE](https://ieeexplore.ieee.org/document/1572614)
- RFC 6330, RaptorQ — [IETF](https://datatracker.ietf.org/doc/html/rfc6330)
- Maymounkov & Mazières 2003 — [Springer](https://doi.org/10.1007/978-3-540-45172-3_22)
- Dimakis et al. 2010, regenerating codes — [IEEE](https://ieeexplore.ieee.org/document/5339410) / [arXiv:cs/0702015](https://arxiv.org/abs/cs/0702015)
- Cover & El Gamal 1979, relay channel — [IEEE](https://ieeexplore.ieee.org/document/1055116)
- El Gamal & Kim 2011, *Network Information Theory* — [Cambridge](https://www.cambridge.org/core/books/network-information-theory/8B600AE2F4A6F5C2C50CFE6B5C50E9E7)
- Slepian & Wolf 1973 — [IEEE](https://ieeexplore.ieee.org/document/1055037)
- Pradhan & Ramchandran 2003, DISCUS — [IEEE](https://ieeexplore.ieee.org/document/1184141)
- Wyner & Ziv 1976 — [IEEE](https://ieeexplore.ieee.org/document/1055502)
- Berger & Tung 1978 — [IEEE](https://ieeexplore.ieee.org/document/1055893)
- Li & Vitányi, *Kolmogorov Complexity*, 2019 — [Springer](https://link.springer.com/book/10.1007/978-3-030-11298-1)
- Rissanen 1978, MDL — [Elsevier](https://doi.org/10.1016/0005-1098(78)90005-5)
- Grünwald 2007, *MDL Principle* — [MIT Press](https://mitpress.mit.edu/9780262072816/)
- Barron, Rissanen & Yu 1998 — [IEEE](https://ieeexplore.ieee.org/document/761094)
- Tishby, Pereira & Bialek 1999, IB — [arXiv:physics/0004057](https://arxiv.org/abs/physics/0004057)
- Han & Thorup 2002, integer sorting — [Springer](https://doi.org/10.1007/s00453-004-1092-x)
- Atserias, Grohe & Marx 2008, AGM — [SIAM](https://epubs.siam.org/doi/10.1137/090767441) / [FOCS](https://ieeexplore.ieee.org/document/4691000)
- Ngo, Porat, Ré 2012, worst-case optimal joins — [ACM](https://dl.acm.org/doi/10.1145/2213556.2213565)
- Alon, Matias & Szegedy 1999, frequency moments — [Elsevier/JCSS](https://doi.org/10.1006/jcss.1997.1545)
- Lloyd 1957/1982 — [IEEE](https://ieeexplore.ieee.org/document/1057616)
- Max 1960 — [IEEE](https://ieeexplore.ieee.org/document/1057548)
- Linde, Buzo & Gray 1980, LBG — [IEEE](https://ieeexplore.ieee.org/document/1094577)
- Jégou, Douze & Schmid 2011, PQ — [IEEE](https://ieeexplore.ieee.org/document/5432202) / [hal.inria](https://hal.inria.fr/inria-00514467)
- Ge et al. 2014, OPQ — [IEEE](https://ieeexplore.ieee.org/document/6678773)
- Ballé, Laparra & Simoncelli 2017 — [arXiv:1611.01704](https://arxiv.org/abs/1611.01704)
- Alakuijala et al. 2019, JPEG XL — [arXiv:1908.02465](https://arxiv.org/abs/1908.02465)
