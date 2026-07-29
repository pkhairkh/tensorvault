# Probability, Concentration & Sketching Mathematics for a Next-Generation Instruction-First Database Engine

> **Scope.** This document maps twelve families of probabilistic mathematics onto concrete subsystems of an "instruction-first, memory-centric" database engine: 64-bit word values, explicit memory tiers (DRAM / CXL / NVM), and AVX-512 kernels. Each section gives the mathematical foundation (formulas), the foundational papers with citations, and a concrete engineering application.

---

## Table of Contents

1. [Concentration Inequalities for Tight Error Bounds](#1-concentration-inequalities-for-tight-error-bounds)
2. [Streaming Algorithms and Sketches](#2-streaming-algorithms-and-sketches)
3. [Locality-Sensitive Hashing Theory](#3-locality-sensitive-hashing-theory)
4. [Minhash and Bottom-k Sketches for Set Similarity](#4-minhash-and-bottom-k-sketches-for-set-similarity)
5. [Randomized Numerical Linear Algebra (RandNLA)](#5-randomized-numerical-linear-algebra-randnla)
6. [Markov Chain Monte Carlo for Query Sampling](#6-markov-chain-monte-carlo-for-query-sampling)
7. [Sequential Analysis and Adaptive Sampling](#7-sequential-analysis-and-adaptive-sampling)
8. [Bayesian Inference for Cardinality Estimation](#8-bayesian-inference-for-cardinality-estimation)
9. [Hypothesis Testing for A/B Comparisons](#9-hypothesis-testing-for-ab-comparisons)
10. [Probabilistic Data Structures Compendium](#10-probabilistic-data-structures-compendium)
11. [Probability Theory for Variable-Latency Tiers (CXL)](#11-probability-theory-for-variable-latency-tiers-cxl)
12. [Probabilistic Guarantees for Approximate Queries](#12-probabilistic-guarantees-for-approximate-queries)
13. [Summary Table](#summary-table-12-probabilistic-techniques-and-their-db-applications)
14. [Bibliography](#bibliography)

---

## 1. Concentration Inequalities for Tight Error Bounds

Concentration inequalities are the **certificate layer** of every approximate subsystem: they convert "we sampled randomly" into "the answer is within ε of truth with probability 1−δ." For an instruction-first engine they are what lets the planner emit a *guaranteed* approximate plan instead of a heuristic guess.

### 1.1 The ladder of inequalities

Let $X_1,\dots,X_n$ be independent, $X_i\in[a_i,b_i]$, $\mu=\mathbb{E}[\bar X]$, $S_n=\sum_i X_i$.

| Inequality | Statement | Tightness |
|---|---|---|
| **Markov** (non-negative $X$) | $\Pr[X\ge a]\le \mathbb{E}[X]/a$ | weak, 1-sided, only needs mean |
| **Chebyshev** | $\Pr[\lvert X-\mu\rvert\ge a]\le \sigma^2/a^2$ | needs variance; $O(1/n)$ rate |
| **Chernoff–Hoeffding** | $\Pr[\bar X-\mu\ge \varepsilon]\le \exp\!\bigl(-2n\varepsilon^2/(b-a)^2\bigr)$ | exponential, uses range |
| **Bernstein** | $\Pr[S_n-\mu\ge t]\le \exp\!\bigl(-\tfrac{t^2}{2(\sigma^2+Mt/3)}\bigr)$ | uses variance *and* bound; sharper when $\sigma^2\ll (b-a)^2$ |
| **McDiarmid** (bounded differences) | $\Pr[f(X)-\mathbb{E}f\ge t]\le \exp\!\bigl(-2t^2/\sum_i c_i^2\bigr)$ | for *functions* of independent vars, not just sums |

**Hoeffding's lemma** (the engine under Hoeffding's inequality): for $X\in[a,b]$ with $\mathbb{E}X=0$,

$$\mathbb{E}[e^{\lambda X}]\le \exp\!\Bigl(\tfrac{\lambda^2(b-a)^2}{8}\Bigr)\quad\forall\lambda,$$

which via Chernoff bounding $\Pr[S\ge t]=\Pr[e^{\lambda S}\ge e^{\lambda t}]\le e^{-\lambda t}\mathbb{E}[e^{\lambda S}]$ and optimizing over $\lambda$ yields the $\exp(-2n\varepsilon^2)$ tail.

**McDiarmid's bounded-differences condition:** if changing the $i$-th coordinate moves $f$ by at most $c_i$,
$$\bigl|f(x_1,\dots,x_i,\dots,x_n)-f(x_1,\dots,x_i',\dots,x_n)\bigr|\le c_i,$$
then $f$ concentrates around its mean with the tail above. This is the right tool for *sketch estimators that are not simple sums* (e.g., the median-of-averages in AMS, or the harmonic-mean-style collapse in HyperLogLog).

**Bernstein's advantage.** When each $X_i$ is Bernoulli with small $p$ (heavy-hitter counts in a sparse stream), $\sigma^2=np(1-p)$ is tiny while the range is $[0,n]$; Hoeffding ignores this and gives $\exp(-2n\varepsilon^2)$, whereas Bernstein gives $\exp(-t^2/(2np(1-p)+t/3))$ — orders of magnitude tighter. This matters for confidence intervals on rare-item estimates.

### 1.2 Application to the three flagship sketches

* **HyperLogLog.** The estimator is $\hat n = \alpha_m m^2 \bigl(\sum_{j=1}^m 2^{-M_j}\bigr)^{-1}$ where $M_j$ are register maxima. Flajolet et al. prove $\hat n$ is asymptotically normal with relative standard error $\approx 1.04/\sqrt{m}$. McDiarmid gives the non-asymptotic tail: register changes are bounded, so the harmonic-mean functional satisfies the bounded-differences condition with $c_i=O(1/m)$, yielding $\Pr[|\hat n-n|\ge \varepsilon n]\le 2\exp(-\Theta(m\varepsilon^2))$. **Engineering choice:** $m=2^{14}$ registers ⇒ ~0.81% standard error, and a $(\varepsilon=2\%,\delta=1\%)$ guarantee uses $\approx 26$ KB — a single AVX-512 cache-way sized sketch.

* **Count-Min.** With width $w=\lceil e/\varepsilon\rceil$ and depth $d=\lceil\ln(1/\delta)\rceil$, the estimate $\hat f_i=\min_j\sum_t X_{tj}$ overflows only upward. Hoeffding applied *per row* gives $\Pr[\hat f_i>f_i+\varepsilon\|f\|_1]\le 1/e$ per row; taking the min and unioning over $d$ rows yields the $1-\delta$ guarantee. **The min is exactly where McDiarmid/union-bound interacts:** the min-of-means estimator has the bounded-differences property with $c_t$ proportional to the per-row contribution.

* **AMS / $F_2$.** The estimator $X=(\sum_i s_i)^2$ over a 4-wise independent sign vector has $\mathbb{E}X=F_2$ and $\mathrm{Var}(X)\le 4F_2^2$; averaging $k=O(1/\varepsilon^2)$ independent copies and applying Chebyshev gives $\Pr[|\bar X-F_2|>\varepsilon F_2]\le 1/4$; repeating $O(\log(1/\delta))$ times and taking the median yields $(\varepsilon,\delta)$ with total space $O(\varepsilon^{-2}\log(1/\delta)\log n)$.

### 1.3 Foundational references

* **Hoeffding (1963)**, "Probability Inequalities for Sums of Bounded Random Variables," *J. Amer. Statist. Assoc.* 58(301):13–30. [doi:10.2307/2282952](https://doi.org/10.2307/2282952)
* **McDiarmid (1989)**, "On the method of bounded differences," *Surveys in Combinatorics* 141:148–188, Cambridge UP. [link](https://www.cambridge.org/core/books/surveys-in-combinatorics-1989/on-the-method-of-bounded-differences/)
* **Bernstein (1927)**; modern exposition in **Boucheron, Lugosi, Massart (2013)**, *Concentration Inequalities: A Nonasymptotic Theory of Independence*, Oxford Univ. Press. [link](https://global.oup.com/academic/product/concentration-inequalities-9780199535255)
* **Dubhashi & Panconesi (2009)**, *Concentration of Measure for the Analysis of Randomized Algorithms*, Cambridge UP. [link](https://www.cambridge.org/core/books/concentration-of-measure-for-the-analysis-of-randomized-algorithms/)

---

## 2. Streaming Algorithms and Sketches

Streaming algorithms are the **single-pass, sublinear-memory** backbone of an engine that must answer analytics over petabyte-scale tables without materializing them. The unifying object is the *turnstile frequency vector* $f\in\mathbb{Z}^n$ updated by a stream of $(i,\Delta)$ increments.

### 2.1 Frequency moments — AMS

**Alon–Matias–Szegedy (1999)** introduced the sketch paradigm for $F_p=\sum_i |f_i|^p$.

*Core estimator for $F_2$:* pick a 4-wise independent sign vector $s\in\{-1,+1\}^n$, compute $Z=(\sum_i s_i f_i)^2$. Then
$$\mathbb{E}[Z]=F_2,\qquad \mathrm{Var}(Z)\le 4F_2^2.$$
Averaging $k$ and median-of-$O(\log 1/\delta)$ repetitions yields an $(\varepsilon,\delta)$-approximation in space $O(\varepsilon^{-2}\log(1/\delta)\log n)$ bits. AMS proved the celebrated **lower bound** $\Omega(\varepsilon^{-2})$ space for $F_2$, and $\Omega(n^{1-1/p})$ for $p>2$ — showing sketches are *necessary*, not merely convenient.

$F_0$ (distinct count) is the entry point to HyperLogLog (§2.4); $F_2$ powers inner-product / self-join size estimation.

### 2.2 Count-Min sketch — heavy hitters & point queries

**Cormode & Muthukrishnan (2004).** A $d\times w$ array of counters; each item hashed by $d$ independent pairwise-independent functions. Point estimate $\hat f_i=\min_j C[j,h_j(i)]$.

$$\hat f_i \le f_i,\qquad \Pr\!\bigl[\hat f_i \le f_i + \varepsilon\|f\|_1\bigr]\ge 1-\delta,\quad \varepsilon=\tfrac{e}{w},\;\delta=e^{-d}.$$

Space $O(\tfrac{1}{\varepsilon}\log\tfrac{1}{\delta})$ words. **Conservative updates** (Estan–Varghese 2002) — only increment the minimum-affected counters — tighten the $\|f\|_1$ term to $\|f\|_{\text{tail}}$ in practice. Heavy hitters are recovered by a heap keyed on $\hat f_i$ in $O(\varepsilon^{-1}\log 1/\delta)$ space via the *Count-Min with conservative updates + misra-gries* hybrid.

For an instruction-first engine: each row is one AVX-512 `vpaddd` update; $d=8$ rows fit in one ZMM register, so a full Count-Min update is **one vectorized instruction** per hash.

### 2.3 Count sketch — $\ell_2$-heavy hitters

**Charikar, Chen, Farach-Colton (2002).** Uses *signed* updates ($\pm1$ hashes); estimate $\hat f_i=\text{median}_j\, C[j,h_j(i)]\cdot s_j(i)$. Guarantees $\Pr[|\hat f_i-f_i|>\varepsilon\|f\|_2]\le\delta$. Finds items with $|f_i|\ge\varepsilon\|f\|_2$ (the $\ell_2$-heavy hitters), strictly stronger than Count-Min's $\ell_1$ threshold for skewed data.

### 2.4 HyperLogLog — distinct-count

**Flajolet, Fusy, Gandouet, Meunier (2007).** Stochastic averaging over $m=2^b$ registers; the estimator is the *harmonic mean* of $2^{M_j}$:
$$\hat n=\alpha_m m^2\Bigl(\sum_{j=1}^m 2^{-M_j}\Bigr)^{-1},\quad \alpha_m=\Bigl(2\!\int_0^\infty\!(\log_2\tfrac{2+u}{1+u})^m du\Bigr)^{-1}.$$
Relative standard error $\approx 1.04/\sqrt{m}$. The Flajolet et al. martingale analysis proves the asymptotic normality; for finite $m$, McDiarmid (§1) gives the concentration certificate. Storage: $m\cdot 5$ bits (register saturation).

**HyperLogLog++ (Heule, Nunkesser, Hall 2013, Google):** sparse representation for low cardinalities + 8-bit registers + bias correction for the small-range regime — the production-grade variant used in BigQuery, Presto, Druid.

### 2.5 KMV / Bottom-$k$ sketches

**Bar-Yossef et al. (2002)** $k$-minimum-values (KMV): retain the $k$ smallest hash values of a stream; estimate $F_0\approx k/(\text{max retained hash})$. Variance $\approx F_0^2/(k-2)$; unbiased for the threshold $U_{(k)}$: $\hat F_0=(k-1)/U_{(k)}$. Bottom-$k$ generalizes to set operations (§4).

### 2.6 Application to the engine

| Subsystem | Sketch | Guarantee |
|---|---|---|
| `COUNT(DISTINCT)` operator | HLL++ | 1% rel. err., 12 KB |
| `GROUP BY` cardinality | HLL++ on keys | sublinear in group count |
| Heavy-hitter / `TOP k` | Count-Min + heap | $\varepsilon\|f\|_1$ additive, $1-\delta$ |
| Self-join size / `SUM(x*y)` | AMS $F_2$ | $(\varepsilon,\delta)$, $O(\varepsilon^{-2})$ space |
| Quantiles (`PERCENTILE`) | t-digest / GK | $\varepsilon$-approx rank |

### 2.7 References

* **Alon, Matias, Szegedy (1999)**, "The Space Complexity of Approximating the Frequency Moments," *J. Comput. Syst. Sci.* 58(1):137–147. [doi:10.1006/jcss.1997.1545](https://doi.org/10.1006/jcss.1997.1545) — **Gödel Prize 2005**.
* **Cormode & Muthukrishnan (2004)**, "An Improved Data Stream Summary: The Count-Min Sketch and its Applications," *J. Algorithms* 55(1):58–75. [doi:10.1016/j.jalgor.2003.12.001](https://doi.org/10.1016/j.jalgor.2003.12.001)
* **Charikar, Chen, Farach-Colton (2002)**, "Finding Frequent Items in Data Streams," *ICALP*. [doi:10.1007/3-540-45465-9_59](https://doi.org/10.1007/3-540-45465-9_59)
* **Flajolet, Fusy, Gandouet, Meunier (2007)**, "HyperLogLog: the analysis of a near-optimal cardinality estimation algorithm," *DMTCS Proc.* AH:127–146. [link](https://algo.inria.fr/flajolet/Publications/FlFuGaMe07.pdf)
* **Bar-Yossef, Jayram, Kumar, Sivakumar, Trevisan (2002)**, "Counting Distinct Elements in a Data Stream," *RANDOM*. [doi:10.1007/3-540-45726-7_1](https://doi.org/10.1007/3-540-45726-7_1)
* **Heule, Nunkesser, Hall (2013)**, "HyperLogLog in Practice," *EDBT*. [doi:10.1145/2452376.2452456](https://doi.org/10.1145/2452376.2452456)
* **Cormode, Garofalakis, Johnson, Shkapenyuk (2008)**, "Tracking icebergs in data streams," *ACM Trans. DB Syst.*

---

## 3. Locality-Sensitive Hashing Theory

LSH turns *approximate similarity search* into *exact hash collision lookup*: two close objects collide with high probability, distant ones rarely. For an instruction-first engine this is the primitive behind **similarity joins, deduplication, and vector search on any column type**.

### 3.1 The LSH definition

A family $\mathcal{H}$ is $(r,cr,p_1,p_2)$-sensitive for a distance $D$ if
$$\Pr_{h\sim\mathcal{H}}[h(x)=h(y)]\ge p_1 \;\text{ when } D(x,y)\le r,\qquad \Pr[h(x)=h(y)]\le p_2 \;\text{ when } D(x,y)\ge cr.$$
Amplification: concatenate $k$ hashes (AND) to drop $p_2^k$, then take $L$ such bands (OR) to recover $1-(1-p_1^k)^L$ on near pairs. The query inspects $O(L)$ buckets; with $k,L$ tuned, near-neighbor recall $1-\delta$ at $O(n^{1/c})$ (or $O(n^\rho$, $\rho<1$) work — **sublinear**.

### 3.2 p-stable LSH for $\ell_p$ (Indyk 2000; Datar-Immorlica-Indyk-Mirrokni 2004)

Sample $a\in\mathbb{R}^d$ from a $p$-stable distribution ($p=1$ Cauchy, $p=2$ Gaussian), hash $h_{a,b}(x)=\lfloor (a\cdot x+b)/r\rfloor$. Then
$$\Pr[h(x)=h(y)]=1-\tfrac{\|x-y\|_p}{r}\quad\text{(p=1, in 1D projection)},$$
generalizing to a closed collision function $p(\|x-y\|/r)$ depending only on the projected distance. For $\ell_2$, $\rho=\ln(1/p_1)/\ln(1/p_2)<1/c$.

### 3.3 Optimal LSH (Andoni–Indyk)

**Andoni & Indyk (2008)** achieve the **optimal exponent** $\rho=1/c$ for $\ell_1$ (and, via embeddings, near-optimal for $\ell_2$), matching the lower bound of Motwani–Naor–Panigrahy. This is the theoretical ceiling for LSH-based NN.

### 3.4 Simhash (cosine) — Charikar 2002

$h(x)=\text{sign}(a\cdot x)$, $a\sim N(0,I)$. Then
$$\Pr[h(x)=h(y)]=1-\tfrac{\theta(x,y)}{\pi},$$
i.e., collision probability is a linear function of the **angle** $\theta$. Concatenating $k$ bits hashes to a $k$-bit key; the Hamming distance between keys estimates $\theta$. This is the default for **text/feature-vector similarity joins**.

### 3.5 Minhash (Jaccard) — Broder 1997

$h_\pi(x)=\min_{i\in x}\pi(i)$. Collision probability equals the Jaccard similarity (see §4). The combination of simhash (angle) + minhash (Jaccard) + $p$-stable ($\ell_p$) covers **all common column semantics** in a unified LSH-join operator.

### 3.6 LSH Forests (Bawa–Cooper–Sugar 2005)

Variable-depth prefix trees on concatenated LSH bits, avoiding the need to pre-fix $k$. Each leaf is a bucket; the query descends to the deepest prefix that yields enough candidates. This gives **data-adaptive bucketing** — critical when similarities are not known a priori, as in ad-hoc SQL joins.

### 3.7 Application to the engine

A single `LSH_JOIN` operator parameterized by the metric:
- `cosine` ⇒ simhash bits, AVX-512 `vpopcnt` for Hamming.
- `euclidean` ⇒ $p$-stable, 64-bit fixed-point projection (matches the 64-bit-word design).
- `jaccard` ⇒ minhash.
- `hamming` ⇒ random bit-projection.

The instruction-first design maps each band to a ZMM register; $L$ bands ⇒ $L$ lookups, each a SIMD gather.

### 3.8 References

* **Indyk (2000)**, "Stable distributions, pseudorandom generators, embeddings, and data stream computation," *FOCS*. [doi:10.1109/SFCS.2000.892008](https://doi.org/10.1109/SFCS.2000.892008)
* **Charikar (2002)**, "Similarity Estimation Techniques from Rounding Algorithms," *STOC*. [doi:10.1145/509907.509965](https://doi.org/10.1145/509907.509965)
* **Andoni & Indyk (2008)**, "Near-Optimal Hashing Algorithms for Approximate Nearest Neighbor in High Dimensions," *Commun. ACM* 51(1):117–122. [doi:10.1145/1327452.1327494](https://doi.org/10.1145/1327452.1327494)
* **Datar, Immorlica, Indyk, Mirrokni (2004)**, "Locality-sensitive hashing scheme based on p-stable distributions," *SoCG*. [doi:10.1145/997817.997857](https://doi.org/10.1145/997817.997857)
* **Bawa, Cooper, Sugar (2005)**, "LSH Forest: Self-tuning Indexes for Similarity Search," *WWW*. [doi:10.1145/1060745.1060768](https://doi.org/10.1145/1060745.1060768)
* **Broder (1997)**, see §4.

---

## 4. Minhash and Bottom-$k$ Sketches for Set Similarity

### 4.1 Broder's theorem

For a random permutation $\pi$ and sets $A,B$,
$$\Pr\!\bigl[\min_{a\in A}\pi(a)=\min_{b\in B}\pi(b)\bigr]=\frac{|A\cap B|}{|A\cup B|}=J(A,B).$$
*Proof sketch:* the minimum of $\pi$ over $A\cup B$ lands in $A\cap B$ with probability exactly $|A\cap B|/|A\cup B|$, and that is precisely the event the two minima coincide.

This converts **similarity estimation into Bernoulli counting**: generate $k$ minhashes, count matches $\hat J = (\text{\# matches})/k$, and Hoeffding (§1) bounds
$$\Pr[|\hat J-J|\ge\varepsilon]\le 2e^{-2k\varepsilon^2}\;\Rightarrow\;k=\tfrac{1}{2\varepsilon^2}\ln\tfrac{2}{\delta}.$$

### 4.2 Bottom-$k$ (a.k.a. $k$-mins / KMV) unbiased estimator

Retain the $k$ smallest hashes of $A$. Let $x_A$ be the $k$-th smallest. The **Jaccard estimator** (Thorup 2013; Beyer et al.) is
$$\hat J_{\text{bottom-}k}=\frac{|\text{Bottom}_k(A)\cap\text{Bottom}_k(B)|\;/\;k}{\;x_A + x_B - |\text{Bottom}_k(A)\cap\text{Bottom}_k(B)|/k\;}\;\;\text{(ratio form)},$$
and the simpler unbiased estimator of $|A\cap B|$ is $(k-1)/x_{A\cup B}$ where $x_{A\cup B}$ is the $k$-th smallest over the union. Bottom-$k$ is **strictly more sample-efficient** than $k$ independent minhashes for the same $k$ (lower variance, $\approx 1/\sqrt{k-2}$ vs $1/\sqrt{k}$).

### 4.3 Applications

- **Deduplication / fuzzy join** on string columns: shingle $\to$ minhash $\to$ LSH band; pairs with $\hat J>\tau$ are candidate matches.
- **Approximate set intersection size** $|A\cap B|$ for IN-list / semi-join planning.
- **Set containment** $C(A,B)=|A\cap B|/|A|$ via the containment sketch (Broder).

### 4.4 References

* **Broder (1997)**, "On the Resemblance and Containment of Documents," *Compression and Complexity of Sequences (SEQUENCES)*, IEEE. [doi:10.1109/SEQUEN.1997.666900](https://doi.org/10.1109/SEQUEN.1997.666900)
* **Broder, Charikar, Frieze, Mitzenmacher (1998)**, "Min-Wise Independent Permutations," *STOC*. [doi:10.1145/276698.276781](https://doi.org/10.1145/276698.276781)
* **Thorup (2013)**, "Bottom-k and priority sampling, set similarity and subset sums with minimal independence," *STOC*.

---

## 5. Randomized Numerical Linear Algebra (RandNLA)

RandNLA brings **matrix approximation with probabilistic guarantees** — the math for approximate joins, low-rank materialized views, and PCA-style analytics over wide tables.

### 5.1 Randomized matrix multiplication (Drineas–Kannan–Mahoney)

To approximate $AB$ where $A\in\mathbb{R}^{m\times n}, B\in\mathbb{R}^{n\times p}$, sample $c$ columns of $A$ (rows of $B$) with probabilities $\{p_i\}$, scale by $1/\sqrt{cp_i}$, and form $\tilde A\tilde B$. With $p_i\propto\|A_{:i}\|^2\|B_{i:}\|^2$ (length-squared sampling),
$$\mathbb{E}\bigl[\|\tilde A\tilde B - AB\|_F^2\bigr]\le \tfrac{1}{c}\Bigl(\sum_i\|A_{:i}\|\|B_{i:}\|\Bigr)^2,$$
giving $(\varepsilon,\delta)$ bounds with $c=O(\varepsilon^{-2}\log(1/\delta))$.

### 5.2 Randomized SVD (Halko–Martinsson–Tropp 2011)

Two-stage:
1. **Range finding.** Draw $\Omega\in\mathbb{R}^{n\times(k+p)}$ Gaussian, form $Y=A\Omega$, orthonormalize $Q=\text{qr}(Y)$. With $p$ oversampling, $\mathbb{E}\|A-QQ^*A\|\le \bigl(1+\tfrac{4\sqrt{k+p}}{p-1}\sqrt{\min(m,n)}\bigr)\sigma_{k+1}$.
2. **Small SVD.** $B=Q^*A$, SVD of the small $B$, lift back.

Cost: $O(mnk)$ vs $O(\min(mn^2,m^2n))$ exact. **Power iteration** ($q$ steps of $(AA^*)^q A\Omega$) decays the error as $\sigma_{k+1}^{2q+1}$, crucial for slow spectral decay.

### 5.3 Leverage scores

The statistical leverage score $\ell_i=\|U_{:i}\|_2^2$ of the top-$k$ left singular vectors measures the *influence* of row $i$. Leverage-score sampling ($p_i\propto \ell_i$) gives **optimal** low-rank approximation with $c=O(k\log k/\varepsilon^2)$ samples (Drineas–Mahoney–Muthukrishnan 2006–2008). Leverage scores also underpin **ridge-leverage-score coresets** for kernel methods.

### 5.4 Application to the engine

- **Approximate `JOIN`/`GROUP BY` aggregates** over wide fact tables: treat the join as matrix product, sample rows, bound the aggregate error.
- **Low-rank materialized views**: maintain randomized-SVD factors as a refreshable sketch; `SELECT` projected columns answered from the low-rank core.
- **`MATVIEW` compression**: store $U\Sigma$ (rank-$k$) instead of full table for analytics-heavy columns; error certificate from §5.2.
- All matrix multiplications map to **AVX-512 `vfmadd`** FMA kernels on the 64-bit-word layout (use FP32/FP64 words in the analytics tier).

### 5.5 References

* **Halko, Martinsson, Tropp (2011)**, "Finding Structure with Randomness: Probabilistic Algorithms for Constructing Approximate Matrix Decompositions," *SIAM Review* 53(2):217–288. [doi:10.1137/090771806](https://doi.org/10.1137/090771806)
* **Drineas, Kannan, Mahoney (2006)**, "Fast Monte Carlo Algorithms for Matrices I–III," *Theor. Comput. Sci.* [doi:10.1016/j.tcs.2005.10.013](https://doi.org/10.1016/j.tcs.2005.10.013)
* **Mahoney (2011)**, "Randomized Algorithms for Matrices and Data," *Found. Trends ML* 3(2):123–224. [doi:10.1561/2200000035](https://doi.org/10.1561/2200000035)
* **Drineas & Mahoney (2016)**, "RandNLA: randomized numerical linear algebra," *Commun. ACM* 59(6):80–90. [doi:10.1145/2842602](https://doi.org/10.1145/2842602)
* **Woodruff (2014)**, *Sketching as a Tool for Numerical Linear Algebra*, FnT TCS. [doi:10.1561/0400000060](https://doi.org/10.1561/0400000060)

---

## 6. Markov Chain Monte Carlo for Query Sampling

When a table is too large to scan but the query is an aggregate, **MCMC table sampling** produces a Markov chain whose stationary distribution is the desired sampling distribution, enabling unbiased online aggregation with quantifiable mixing.

### 6.1 Metropolis–Hastings

Given target $\pi$ and proposal $q(x\to y)$, accept with probability
$$\alpha(x\to y)=\min\!\Bigl(1,\;\tfrac{\pi(y)q(y\to x)}{\pi(x)q(x\to y)}\Bigr).$$
The chain $\{X_t\}$ has stationary distribution $\pi$. For table sampling, $\pi(i)\propto w_i$ (a row-weight function, e.g., the group key or a predicate). The estimator $\hat\mu=\frac{1}{T}\sum_t g(X_t)$ is asymptotically unbiased.

### 6.2 Gibbs sampling

Update each coordinate $x_j$ from its conditional $\pi(x_j\mid x_{-j})$. Useful for **multi-table sampling** where conditioning on join keys makes the full conditional tractable.

### 6.3 Hit-and-run (Smith 1984)

From point $x$ in a convex body, pick a random direction $u$ uniformly on the sphere, step to a uniformly chosen point on the chord through $x$ along $u$. Converges fast (mixing $O^*(n^2\log(1/\varepsilon))$ for isotropic convex bodies per Lovász–Vempala).

### 6.4 Mixing-time analysis via spectral gap

The **spectral gap** $\gamma=1-\lambda_2$ of the chain's transition matrix controls convergence:
$$\|P^t(x,\cdot)-\pi\|_{TV}\le \tfrac{1}{2}\sqrt{\tfrac{1-\pi(x)}{\pi(x)}}\,e^{-\gamma t}.$$
Mixing time $t_{\text{mix}}(\varepsilon)=O(\gamma^{-1}\log(1/\varepsilon\pi_{\min}))$. This is the direct bridge to spectral graph theory (the Laplacian / Fiedler eigenvalue of the data-graph). Conductance $\Phi$ bounds the gap via Cheeger: $\Phi^2/2\le\gamma\le 2\Phi$.

### 6.5 Application to the engine

- **Online aggregation** with confidence intervals: as the chain runs, emit running mean $\pm$ Hoeffding-based CI (using the *effective sample size* $T_{\text{eff}}=T/(1+2\sum_k\rho_k)$ from autocorrelations — because MCMC samples are dependent, plain Hoeffding is optimistic).
- **Predicate-aware sampling**: $\pi(i)\propto \mathbf{1}[\text{row }i\text{ matches predicate}]\cdot w_i$ via Metropolis on a join graph.
- The **join-graph Laplacian** (Fiedler value) predicts when sampling will mix fast vs. get stuck in a skewed partition — a query-planner signal.

### 6.6 References

* **Metropolis, Rosenbluth, Rosenbluth, Teller, Teller (1953)**, "Equation of State Calculations by Fast Computing Machines," *J. Chem. Phys.* 21:1087. [doi:10.1063/1.1699114](https://doi.org/10.1063/1.1699114)
* **Hastings (1970)**, "Monte Carlo sampling methods using Markov chains and their applications," *Biometrika* 57:97. [doi:10.1093/biomet/57.1.97](https://doi.org/10.1093/biomet/57.1.97)
* **Geman & Geman (1984)**, "Stochastic Relaxation, Gibbs Distributions, and the Bayesian Restoration of Images," *IEEE TPAMI*. [doi:10.1109/TPAMI.1984.4767596](https://doi.org/10.1109/TPAMI.1984.4767596)
* **Andrieu, de Freitas, Doucet, Jordan (2003)**, "An Introduction to MCMC for Machine Learning," *Machine Learning* 50:5–43. [doi:10.1023/A:1020281327116](https://doi.org/10.1023/A:1020281327116)
* **Lovász & Simonovits (1993)**, "Random walks in a convex body and an improved volume algorithm," *Random Structures & Algorithms* 4(4):359–412. [doi:10.1002/rsa.3240040403](https://doi.org/10.1002/rsa.3240040403)
* **Levin, Peres, Wilmer (2017)**, *Markov Chains and Mixing Times*, 2nd ed., AMS. [link](https://www.ams.org/books/mbk/107)

---

## 7. Sequential Analysis and Adaptive Sampling

### 7.1 Wald's SPRT

The **Sequential Probability Ratio Test** stops sampling as soon as the likelihood ratio crosses a boundary. Testing $H_0:\theta=\theta_0$ vs $H_1:\theta=\theta_1$, accumulate $L_n=\prod_{i\le n} f_{\theta_1}(X_i)/f_{\theta_0}(X_i)$; stop when $L_n\ge A$ (accept $H_1$) or $L_n\le B$ (accept $H_0$).

**Wald's approximations** for target error rates $\alpha,\beta$:
$$A\approx\tfrac{1-\beta}{\alpha},\qquad B\approx\tfrac{\beta}{1-\alpha}.$$
**Average sample number** (Wald's equation): $\mathbb{E}_\theta[n]\approx \tfrac{\mathbb{E}_\theta[\log\text{LR}]}{\log A\;\text{or}\;\log B}$. SPRT uses **on average $50$–$80\%$ fewer samples** than a fixed-sample test of equal power — directly applicable to *early-stopping online aggregation*.

### 7.2 Adaptive / $(\varepsilon,\delta)$-sequential stopping

A modern formulation (following the $(\varepsilon,\delta)$-approximation, §12): sample until an Hoeffding/Bernstein confidence interval has half-width $\le\varepsilon\hat\mu$. Stopping rule
$$T=\inf\Bigl\{n:\;\hat\sigma_n^2\,\tfrac{2\log(2/\delta)}{n\varepsilon^2}\le 1\Bigr\}$$
(empirical Bernstein sequential CI, Audibert–Munos–Catoni 2009) gives $\Pr[|\hat\mu_T-\mu|>\varepsilon\mu]\le\delta$ with $T=O(\sigma^2\varepsilon^{-2}\log(1/\delta))$.

### 7.3 Application to the engine

- **Online aggregation with early stop & live CI**: a `SELECT AVG(...)` over a billion rows emits a stream of $(\hat\mu_t, \text{CI}_t)$; the client stops when the CI is tight enough. The planner binds $\varepsilon,\delta$ from a SQL hint `APPROXIMATE WITHIN ε CONFIDENCE 1-δ`.
- **Adaptive predicate selectivity**: stop sampling a predicate once the selectivity estimate CI is narrow enough to pick the right join order.
- **A/B test within the DB**: SPRT on two materialized aggregates decides winner at minimal sample cost (§9).

### 7.4 References

* **Wald (1945)**, "Sequential Tests of Statistical Hypotheses," *Ann. Math. Statist.* 16(2):117–186. [doi:10.1214/aoms/1177731118](https://doi.org/10.1214/aoms/1177731118)
* **Wald & Wolfowitz (1948)**, "Optimum Character of the Sequential Probability Ratio Test," *Ann. Math. Statist.* 19:326. [doi:10.1214/aoms/1177730204](https://doi.org/10.1214/aoms/1177730204)
* **Audibert, Munos, Catoni (2009)**, "Variance-based Value Function Approximation," *ICML*.
* **Neyman (1934)**, "On the two different aspects of the representative method," *JRSS* 97:558–625. [doi:10.2307/2342192](https://doi.org/10.2307/2342192)

---

## 8. Bayesian Inference for Cardinality Estimation

Classical cardinality estimators (histograms, sampling) are unbiased but ignore **query-log history**. Bayesian methods place priors over selectivity distributions and update from observed run-stats, yielding tighter estimates for recurring workload patterns.

### 8.1 Conjugate priors

- **Beta-Binomial** for a Boolean predicate selectivity $p$: prior $p\sim\text{Beta}(\alpha,\beta)$, observe $x$ matches in $n$ Bernoulli trials ⇒ posterior $p\mid x\sim\text{Beta}(\alpha+x,\beta+n-x)$. Posterior mean $(\alpha+x)/(\alpha+\beta+n)$ shrinks the MLE toward the prior; credible interval from the Beta quantiles.
- **Dirichlet–Multinomial** for a multi-valued group key: prior $\boldsymbol\theta\sim\text{Dir}(\alpha_1,\dots,\alpha_K)$, observe counts $\mathbf{n}$ ⇒ posterior $\text{Dir}(\alpha_1+n_1,\dots)$. This is the natural model for `GROUP BY` cardinality per distinct value.

### 8.2 Hierarchical Bayes

Place a hyper-prior on the Beta parameters themselves, e.g. $(\alpha,\beta)\sim\text{hyperprior}$, so that *different predicates share strength* — a rarely-seen predicate borrows from the population of similar predicates. Estimated via Gibbs / variational inference.

### 8.3 Bayesian cardinality estimation in DBMS

**Tzoumas, Deshpande, Hellerstein (2013)** formalize selectivity estimation as inference in a *probabilistic graphical model* over attribute statistics: the posterior over the join-size $|R\bowtie S|$ given observed per-column counts is computed by message passing, giving **principled correlated-attribute cardinality** (the failure mode of independence assumptions in classical optimizers). More recently, **deep / learned cardinality estimators** (MSCN, Kipf–Kipf 2018; NeuroCard 2021) use neural density models as amortized Bayesian inference.

### 8.4 Application to the engine

- A **learned selectivity cache**: per-(table, predicate-shape) Beta/Dirichlet posterior, updated after each query by exact or sampled counts; the optimizer reads the posterior mean ± credible interval.
- **Query-log-driven priors**: histograms from recurring workload shapes (e.g., `WHERE date BETWEEN ?`) inform the $\alpha,\beta$.
- Credible intervals feed a **risk-aware plan selector**: prefer plan B if plan A's expected cost has a heavy upper tail (mean insufficient; use posterior percentiles).

### 8.5 References

* **Bishop (2006)**, *Pattern Recognition and Machine Learning*, Springer. [link](https://www.microsoft.com/en-us/research/people/cmbishop/)
* **Gelman, Carlin, Stern, Dunson, Vehtari, Rubin (2013)**, *Bayesian Data Analysis*, 3rd ed., CRC. [link](https://www.routledge.com/Bayesian-Data-Analysis/Gelman/p/book/9781439840955)
* **Tzoumas, Deshpande, Hellerstein (2013)**, "Statistics-based Cardinality Estimation for Complex Queries," *ACM TODS* 38(3):17. [doi:10.1145/2512453](https://doi.org/10.1145/2512453)
* **Kipf & Kipf (2018)**, "Cardinality Estimation with Deep Neural Networks," *arXiv:1805.02244*. [link](https://arxiv.org/abs/1805.02244)

---

## 9. Hypothesis Testing for A/B Comparisons

### 9.1 Neyman–Pearson lemma

For $H_0:\theta=\theta_0$ vs $H_1:\theta=\theta_1$, the **most powerful test at level $\alpha$** is the likelihood-ratio test: reject $H_0$ when $L(x)=f_{\theta_1}(x)/f_{\theta_0}(x)>c$ with $c$ chosen so $\Pr_{H_0}[\text{reject}]=\alpha$. This is the optimality foundation for every test that follows.

### 9.2 Standard tests

- **Two-sample t-test** (Welch): $t=\frac{\bar X_1-\bar X_2}{\sqrt{s_1^2/n_1+s_2^2/n_2}}$, $\nu\approx\dfrac{(s_1^2/n_1+s_2^2/n_2)^2}{(s_1^2/n_1)^2/(n_1-1)+(s_2^2/n_2)^2/(n_2-1)}$. Robust to unequal variances.
- **Chi-squared** for contingency tables: $\chi^2=\sum_{ij}(O_{ij}-E_{ij})^2/E_{ij}\sim\chi^2_{(r-1)(c-1)}$ under $H_0$ of independence — powers `GROUP BY`-aware A/B comparisons.
- **Mann–Whitney U** (distribution-free) for ordinal metrics.

### 9.3 Multiple-testing correction

Testing $m$ hypotheses at level $\alpha$ each yields $\sim m\alpha$ false positives in expectation.

- **Bonferroni:** reject $H_i$ only if $p_i\le\alpha/m$. Controls FWER $\le\alpha$; conservative.
- **Benjamini–Hochberg (1995):** sort $p_{(1)}\le\dots\le p_{(m)}$; find largest $k$ with $p_{(k)}\le (k/m)\alpha$; reject all $H_{(1)},\dots,H_{(k)}$. Controls the **False Discovery Rate** $\text{FDR}\le\alpha$ — the modern standard when $m$ is large (e.g., feature screening, multi-metric A/B dashboards).

### 9.4 Application to the engine

A built-in `EXPERIMENT` / `AB_TEST` SQL primitive:
```sql
SELECT ab_test(metric, variant, alpha => 0.05, correction => 'bh')
FROM events GROUP BY experiment_id;
```
runs the appropriate test, returns effect size, CI, $p$-value, and FDR-adjusted $q$-value across all metrics. This turns the DB into the **experimentation platform**, avoiding out-of-DB statistical pipelines. Sequential variants (§7) enable **always-valid $p$-values** (Howard et al. 2021, mixture sequential probability ratio test — mSPRT) for continuous peeking.

### 9.5 References

* **Neyman & Pearson (1933)**, "On the Problem of the Most Efficient Tests of Statistical Hypotheses," *Phil. Trans. R. Soc. A* 231:289–337. [doi:10.1098/rsta.1933.0009](https://doi.org/10.1098/rsta.1933.0009)
* **Benjamini & Hochberg (1995)**, "Controlling the False Discovery Rate: a Practical and Powerful Approach to Multiple Testing," *JRSS B* 57(1):289–300. [doi:10.1111/j.2517-6161.1995.tb02031.x](https://doi.org/10.1111/j.2517-6161.1995.tb02031.x)
* **Welch (1947)**, "The generalization of Student's problem when several different population variances are involved," *Biometrika* 34:28. [doi:10.1093/biomet/34.1-2.28](https://doi.org/10.1093/biomet/34.1-2.28)
* **Howard, Ramdas, McAuliffe, Sekhon (2021)**, "Time-uniform, nonparametric, nonasymptotic confidence sequences," *Ann. Statist.* 49(2):1055–1080. [doi:10.1214/20-AOS1998](https://doi.org/10.1214/20-AOS1998)

---

## 10. Probabilistic Data Structures Compendium

| Structure | Op | Space | Error | Paper |
|---|---|---|---|---|
| **Bloom filter** | membership | $m$ bits | FPR $(1-e^{-kn/m})^k$ | Bloom 1970 |
| **Cuckoo filter** | membership + delete | ~$m$ bits | FPR $\le 2b\cdot(2\ln2)^b/(2^f)$ | Fan et al. 2014 |
| **Quotient filter** | membership + counts | $m$ slots | FPR $\approx$ quotient | Bender et al. 2012 |
| **HyperLogLog++** | distinct count | $m\cdot 5$ bits | $\approx 1.04/\sqrt{m}$ | Flajolet 2007 / Heule 2013 |
| **Count-Min (conservative)** | freq + heavy hitters | $dw$ | $\varepsilon\|f\|_1$ w.p. $1-\delta$ | Cormode 2004 |
| **Count sketch** | freq ($\ell_2$-heavy) | $dw$ | $\varepsilon\|f\|_2$ w.p. $1-\delta$ | Charikar 2002 |
| **t-digest** | quantiles | $O(1/\delta)$ centroids | $\varepsilon$-approx rank | Dunning 2019 |
| **GK sketch** | quantiles | $O(\varepsilon^{-1}\log(\varepsilon n))$ | $\varepsilon$-approx rank | Greenwald–Khanna 2001 |
| **Ribbon filter** | membership | $m$ bits | FPR $\approx (1-e^{-kn/m})^k$ | Breslav 2020 / Zukowski 2020 |

### 10.1 Bloom filter math

$k$ hash functions into $m$ bits; after $n$ insertions, a bit is still 0 with probability $(1-1/m)^{kn}\approx e^{-kn/m}$. False-positive rate
$$\text{FPR}=\bigl(1-e^{-kn/m}\bigr)^k,\quad\text{minimized at }k=\tfrac{m}{n}\ln2,\;\;\text{FPR}_{\min}=(0.6185)^{m/n}.$$
Optimal: $\log_2(1/\text{FPR})$ bits per element.

### 10.2 Cuckoo filter

Stores fingerprints in a cuckoo-hash table; lookup checks two candidate buckets. Supports **deletion** (unlike Bloom). FPR $\approx 2b/(2^f)$ for fingerprint size $f$ and bucket capacity $b$; space-optimal configurations are more compact than Bloom at equal FPR.

### 10.3 t-digest (Dunning 2019)

Merges clusters with size bounded by a scale function $\delta$ so that **tail quantiles are extremely accurate**: a $t$-digest of $\sim 1000$ centroids gives $<0.1\%$ error at the 0.999 quantile. Built for streaming; merges are associative, enabling **distributed quantile aggregation** (map-reduce over shards). This makes `PERCENTILE` over a sharded table both exact-enough and parallel.

### 10.4 Application to the engine

Each structure maps to an instruction-first primitive over the 64-bit-word tier:
- Bloom/cuckoo/ribbon: the **set-membership** path for `IN`/semi-join anti-join; one word = one fingerprint bucket; AVX-512 `vpshufb` for parallel lookup.
- HLL++: per-shard distinct counts merged by $\alpha_m m^2/(\sum 2^{-M_j})$ — a single SIMD harmonic-mean reduction.
- Count-Min: vectorized `vpaddd` increments.
- t-digest: per-shard centroids merged; rank queries via binary search over centroid cumulative weights.

### 10.5 References

* **Bloom (1970)**, "Space/time trade-offs in hash coding with allowable errors," *Commun. ACM* 13(7):422–426. [doi:10.1145/362686.362692](https://doi.org/10.1145/362686.362692)
* **Fan, Andersen, Kaminsky, Mitzenmacher (2014)**, "Cuckoo Filter: Practically Better Than Bloom," *CoNEXT*. [doi:10.1145/2674005.2674994](https://doi.org/10.1145/2674005.2674994)
* **Bender, Farach-Colton, Johnson, Kuszmaul, McCauley, Porter, Shieh (2012)**, "Don't Thrash: How to Cache Your Hash on Flash," *WALDIM/VLDB*. [doi:10.1007/978-3-642-33061-3_13](https://doi.org/10.1007/978-3-642-33061-3_13)
* **Dunning (2019)**, "Computing Extremely Accurate Quantiles Using t-Digests," *arXiv:1902.04023*. [link](https://arxiv.org/abs/1902.04023)
* **Greenwald & Khanna (2001)**, "Space-Efficient Online Computation of Quantile Summaries," *SIGMOD*. [doi:10.1145/375663.375670](https://doi.org/10.1145/375663.375670)

---

## 11. Probability Theory for Variable-Latency Tiers (CXL)

A memory-tiered engine with DRAM, CXL-attached memory, and NVM has **non-uniform, stochastic latency**. Queueing theory turns this into a *predictive* model the planner can use to choose scan vs. index, batch size, and replication.

### 11.1 Little's Law

For any stable system in steady state,
$$L=\lambda W,$$
i.e., average number in system $L$ = arrival rate $\lambda$ × average sojourn $W$. Holds for *any* arrival/service distribution — a universal invariant. **Engineering use:** given a target latency $W$ and observed throughput $\lambda$, the planner computes the required concurrency $L$ and sizes thread pools / prefetch queues.

### 11.2 M/M/1 and M/M/c

Exponential arrivals (rate $\lambda$), exponential service (rate $\mu$), $c$ servers. For M/M/1 ($\rho=\lambda/\mu<1$):
$$L=\tfrac{\rho}{1-\rho},\quad W=\tfrac{1}{\mu-\lambda},\quad L_q=\tfrac{\rho^2}{1-\rho}.$$
For M/M/c: $L_q=\frac{\rho^c}{c!}\frac{\rho}{(1-\rho/c)^2}p_0$, with Erlang-C $p_0$. **Latency blows up as $\rho\to1$** — the planner must keep tier utilization $\rho\le 0.7$ on the hot path.

### 11.3 M/G/1 — Pollaczek–Khinchine

General service distribution with variance $\sigma_s^2$ and mean $1/\mu$:
$$W_q=\frac{\lambda\,\mathbb{E}[S^2]}{2(1-\rho)}=\frac{\rho\bigl(1+c_s^2\bigr)}{2\mu(1-\rho)},\qquad c_s^2=\sigma_s^2\mu^2.$$
The $c_s^2$ term is decisive: **high-variance service (CXL tail latency) inflates waiting time linearly in the squared coefficient of variation.**

### 11.4 Kingman's approximation (G/G/1)

For general arrival *and* service distributions, with arrival $c_a^2$ and service $c_s^2$ squared coefficients of variation:
$$\boxed{\;W\approx \frac{\rho}{1-\rho}\cdot\frac{c_a^2+c_s^2}{2}\cdot\frac{1}{\mu}\;}$$
This is the single most useful formula for tier-latency planning: it decomposes latency into (i) utilization penalty $\rho/(1-\rho)$, (ii) variability penalty $(c_a^2+c_s^2)/2$, (iii) raw service $1/\mu$. CXL memory has $c_s^2\gg1$ due to contention on shared links; **batching** reduces $c_a^2$, **replication / caching** reduces effective $\mu^{-1}$.

### 11.5 Tail-latency: heavy tails and the BCMP/PS view

CXL contention creates near-heavy-tailed response times. Under an M/G/1 **processor-sharing** discipline (time-sliced server), the mean response time is $1/(\mu-\lambda)$ *independent of the service-time distribution* — a strong argument for **fine-grained time-slicing of memory access** (many in-flight AVX-512 requests) over head-of-line blocking.

### 11.6 Application to the engine

- A **cost model** in the planner: estimated query latency = Kingman term on each tier in the plan, summed along the critical path.
- **Batch sizing**: choose batch $b$ to minimize $c_a^2(b)$ subject to AVX-512 register pressure — Kingman then predicts the latency at that $b$.
- **Tail-latency SLO**: given a $p99$ budget, invert the latency distribution (lognormal/Weibull fit to observed CXL latencies) to cap concurrency.
- **Memory-tier placement policy**: hot rows (high $\lambda$) ⇒ DRAM (low $1/\mu$); warm ⇒ CXL ($c_s^2$ penalized but tolerable at $\rho<0.6$); cold ⇒ NVM.

### 11.7 References

* **Little (1961)**, "A Proof for the Queuing Formula L = λW," *Operations Research* 9(3):383–387. [doi:10.1287/opre.9.3.383](https://doi.org/10.1287/opre.9.3.383)
* **Kingman (1961)**, "The single server queue in heavy traffic," *Math. Proc. Cambridge Phil. Soc.* 57(4):902–904. [doi:10.1017/S0305004100036094](https://doi.org/10.1017/S0305004100036094)
* **Kleinrock (1975)**, *Queueing Systems, Vol. I: Theory*, Wiley. [link](https://www.wiley.com/en-us/Queueing+Systems%2C+Volume+1%3A+Theory-p-9780471491101)
* **Pollaczek (1930); Khintchine (1932)**, the P–K mean-value formula for M/G/1.
* **Harchol-Balter (2013)**, *Performance Modeling and Design of Computer Systems: Queueing Theory in Action*, Cambridge UP. [link](https://www.cambridge.org/core/books/performance-modeling-and-design-of-computer-systems/)

---

## 12. Probabilistic Guarantees for Approximate Queries

### 12.1 The $(\varepsilon,\delta)$-approximation

A randomized estimator $\hat\theta$ is an $(\varepsilon,\delta)$-approximation of $\theta$ if
$$\Pr\bigl[|\hat\theta-\theta|>\varepsilon|\theta|\bigr]\le\delta\quad\text{(multiplicative)}\qquad\text{or}\quad\Pr[|\hat\theta-\theta|>\varepsilon]\le\delta\;\text{(additive)}.$$
This is the **contract** an approximate-query engine exposes to SQL: the user declares tolerance; the planner proves the chosen sketch/sampling meets it. Every structure in §2 and §10 carries such a theorem; concentration inequalities (§1) are the proof technique.

### 12.2 PAC framework

**Valiant (1984)** formalized *Probably Approximately Correct* learning: a concept class is PAC-learnable if, for any target concept and distribution, an algorithm with samples $m\ge \tfrac{1}{\varepsilon}\bigl(\ln|\mathcal{H}|+\ln(1/\delta)\bigr)$ returns an $\varepsilon$-good hypothesis with probability $1-\delta$. The $(\varepsilon,\delta)$ quantifier is identical to the approximation contract — sketch/sample complexity is PAC sample complexity. The union-bound $m\ge \tfrac{1}{\varepsilon}(\ln|\mathcal{H}|+\ln(1/\delta))$ is exactly the Hoeffding + union bound used in Count-Min's depth $d=\lceil\ln(1/\delta)\rceil$.

### 12.3 VC dimension & $\varepsilon$-samples

For range/count queries, the **VC dimension** $d_{\text{VC}}$ of the query family bounds the sample size for a uniform $(\varepsilon,\delta)$-approximation of *all* counts simultaneously:
$$m\ge \tfrac{c}{\varepsilon^2}\bigl(d_{\text{VC}}+\ln\tfrac{1}{\delta}\bigr)$$
(Vapnik–Chervonenkis 1971; Har-Peled–Sharir; Löffler–Phillips). Halfspace queries have $d_{\text{VC}}=d+1$; intervals $d_{\text{VC}}=2$. This tells the planner **how big a uniform sample must be** to answer any range-count within $\varepsilon$.

### 12.4 Application to the engine

- A SQL surface `SELECT ... APPROXIMATE WITHIN ε CONFIDENCE 1-δ` maps to the $(\varepsilon,\delta)$ contract; the planner picks the minimal-cost sketch/sample whose theorem matches.
- **Compose-ability**: an approximate plan composes sub-estimators; the planner must propagate $(\varepsilon,\delta)$ through the DAG (union bound for OR-of-estimates; Hölder/Minkowski for sums), exposing a final compound $(\varepsilon',\delta')$.
- **Verification hooks**: at runtime, the engine checks the empirical Bernstein CI (§7) and, if violated, falls back to an exact path — closing the loop between the PAC contract and observed behavior.

### 12.5 References

* **Valiant (1984)**, "A Theory of the Learnable," *Commun. ACM* 27(11):1134–1142. [doi:10.1145/1968.1972](https://doi.org/10.1145/1968.1972)
* **Vapnik & Chervonenkis (1971)**, "On the Uniform Convergence of Relative Frequencies of Events to Their Probabilities," *Theory Probab. Appl.* 16(2):264–280. [doi:10.1137/1116025](https://doi.org/10.1137/1116025)
* **Har-Peled & Sharir (2011)**, "Relative $(p,\varepsilon)$-approximations in geometry," *Discrete Comput. Geom.* 45:462–496. [doi:10.1007/s00454-010-9283-3](https://doi.org/10.1007/s00454-010-9283-3)
* **Agarwal, Har-Peled, Varadarajan (2005)**, "Geometric approximation algorithms via core sets," *Combinatorial and Computational Geometry*.

---

## Summary Table — 12 Probabilistic Techniques and their DB Applications

| # | Technique | Key Formula / Result | Space / Cost | DB Application in the Instruction-First Engine |
|---|---|---|---|---|
| 1 | **Concentration inequalities** (Hoeffding, McDiarmid, Bernstein) | $\Pr[\bar X-\mu\ge\varepsilon]\le e^{-2n\varepsilon^2}$; $\Pr[f-\mathbb{E}f\ge t]\le e^{-2t^2/\sum c_i^2}$ | — | Error certificates for all sketches; planner emits guaranteed $(\varepsilon,\delta)$ approximate plans |
| 2 | **Streaming sketches** (AMS, Count-Min, Count, HLL++, KMV) | $\hat f_i\le f_i+\varepsilon\|f\|_1$ w.p. $1-\delta$; HLL RSE $1.04/\sqrt{m}$ | $O(\varepsilon^{-1}\log\delta^{-1})$ words | `COUNT(DISTINCT)`, heavy-hitters, self-join size, quantiles — all sublinear |
| 3 | **Locality-Sensitive Hashing** (p-stable, simhash, optimal LSH) | $\Pr[h(x)=h(y)]=p(D)$; $\rho=1/c$ optimal | $O(nL)$ buckets, $O(n^\rho)$ query | Unified `LSH_JOIN` over cosine/Euclidean/Jaccard/Hamming; AVX-512 gather per band |
| 4 | **Minhash / Bottom-k** | $\Pr[\minh(A)=\minh(B)]=J(A,B)$; $k=\frac{1}{2\varepsilon^2}\ln\frac{2}{\delta}$ | $k$ hashes per set | Fuzzy joins, deduplication, approximate set intersection for semi-join planning |
| 5 | **RandNLA** (randomized SVD, leverage scores) | $\mathbb{E}\|A-QQ^*A\|\le(1+\ldots)\sigma_{k+1}$ | $O(mnk)$ vs $O(mn^2)$ | Low-rank materialized views, approximate joins, PCA over wide tables |
| 6 | **MCMC** (Metropolis–Hastings, Gibbs, hit-and-run) | Stationary $\pi$; mixing $t\le\gamma^{-1}\log(1/\varepsilon\pi_{\min})$ | $O(\gamma^{-1})$ steps | Predicate-weighted table sampling for online aggregation; join-graph spectral gap predicts mix |
| 7 | **Sequential analysis** (Wald SPRT, empirical-Bernstein stop) | Boundaries $A\!\approx\!\frac{1-\beta}{\alpha}$, $B\!\approx\!\frac{\beta}{1-\alpha}$; 50–80% fewer samples | $O(\sigma^2\varepsilon^{-2}\log\delta^{-1})$ | Early-stop online aggregation with live CIs; adaptive selectivity; always-valid A/B |
| 8 | **Bayesian inference** (Beta-Binomial, Dirichlet, hierarchical) | Posterior $\text{Beta}(\alpha+x,\beta+n-x)$; credible intervals | $O(1)$ per predicate | Learned selectivity cache; risk-aware plan selection via posterior tails |
| 9 | **Hypothesis testing** (Neyman–Pearson, t, χ², BH-FDR) | NP lemma; BH rejects $p_{(k)}\le(k/m)\alpha$ | $O(1)$ per test | Built-in `AB_TEST`/`EXPERIMENT` SQL; FDR-controlled multi-metric dashboards |
| 10 | **Probabilistic data structures** (Bloom, Cuckoo, t-digest, Ribbon) | Bloom FPR $(1-e^{-kn/m})^k$, opt $k=\frac{m}{n}\ln2$ | $\log_2(1/\text{FPR})$ bits/elt | Set-membership for `IN`/semi-join; streaming quantiles; per-shard mergeable |
| 11 | **Queueing theory** (Little, M/M/1, P–K, Kingman) | $W\approx\frac{\rho}{1-\rho}\frac{c_a^2+c_s^2}{2}\frac{1}{\mu}$ | — | CXL/NVM tier-latency model; batch sizing; p99 SLO; memory-tier placement |
| 12 | **PAC / $(\varepsilon,\delta)$ guarantees** (Valiant, VC dimension) | $m\ge\frac{c}{\varepsilon^2}(d_{\text{VC}}+\ln\delta^{-1})$ | sample size | Formal `APPROXIMATE WITHIN ε CONFIDENCE 1-δ` contract; compositional error propagation |

---

## Cross-Cutting Synthesis: How the Twelve Families Compose

The power of this stack is **compositionality along the query DAG**, each node carrying an $(\varepsilon_i,\delta_i)$ certificate:

1. **Cardinality estimation** (§8 Bayesian + §2 HLL++) feeds the optimizer with selectivity posteriors and distinct-count estimates — each with a credible/$(\varepsilon,\delta)$ interval.
2. The **planner** (§11 Kingman cost model + §12 PAC contract) selects, per operator, the cheapest plan whose propagated $(\varepsilon',\delta')$ meets the SQL-declared tolerance.
3. **Execution** uses §2/§10 sketches (vectorized on AVX-512) and §6 MCMC / §7 sequential sampling for online aggregation, emitting live CIs.
4. **Verification** (§1 concentration + §7 empirical-Bernstein) checks the runtime CI; if violated, the engine falls back to an exact scan — closing the PAC loop.
5. **Analytics & experimentation** (§5 RandNLA low-rank views + §9 hypothesis testing) turn the engine into a combined OLAP + experimentation platform.

The 64-bit-word + explicit-tier design is not incidental: each probabilistic primitive maps to a fixed-width record (one register per HLL register-row, per Bloom word, per LSH band), making the SIMD kernel **bit-exact and provably bounded** rather than heuristic.

---

## Bibliography

1. Agarwal, S., Har-Peled, S., Varadarajan, K. R. (2005). *Geometric approximation algorithms via core sets.* In *Combinatorial and Computational Geometry*, MSRI Pub. 52.
2. Alon, N., Matias, Y., Szegedy, M. (1999). *The Space Complexity of Approximating the Frequency Moments.* JCSS 58(1):137–147. [doi:10.1006/jcss.1997.1545](https://doi.org/10.1006/jcss.1997.1545)
3. Andoni, A., Indyk, P. (2008). *Near-Optimal Hashing Algorithms for Approximate Nearest Neighbor in High Dimensions.* Commun. ACM 51(1):117–122. [doi:10.1145/1327452.1327494](https://doi.org/10.1145/1327452.1327494)
4. Andrieu, C., de Freitas, N., Doucet, A., Jordan, M. I. (2003). *An Introduction to MCMC for Machine Learning.* Machine Learning 50:5–43. [doi:10.1023/A:1020281327116](https://doi.org/10.1023/A:1020281327116)
5. Audibert, J.-Y., Munos, R., Catoni, C. (2009). *Variance-based Value Function Approximation.* ICML.
6. Bar-Yossef, Z., Jayram, T. S., Kumar, R., Sivakumar, D., Trevisan, L. (2002). *Counting Distinct Elements in a Data Stream.* RANDOM. [doi:10.1007/3-540-45726-7_1](https://doi.org/10.1007/3-540-45726-7_1)
7. Bawa, M., Cooper, B. F., Sugar, A. (2005). *LSH Forest: Self-tuning Indexes for Similarity Search.* WWW. [doi:10.1145/1060745.1060768](https://doi.org/10.1145/1060745.1060768)
8. Benjamini, Y., Hochberg, Y. (1995). *Controlling the False Discovery Rate.* JRSS B 57(1):289–300. [doi:10.1111/j.2517-6161.1995.tb02031.x](https://doi.org/10.1111/j.2517-6161.1995.tb02031.x)
9. Bender, M. A., Farach-Colton, M., Johnson, R., Kuszmaul, B. C., McCauley, D., Porter, S., Shieh, R. (2012). *Don't Thrash: How to Cache Your Hash on Flash.* WALDIM/VLDB. [doi:10.1007/978-3-642-33061-3_13](https://doi.org/10.1007/978-3-642-33061-3_13)
10. Bernstein, S. (1927). *Theory of Probability.* (Modern exposition in Boucheron–Lugosi–Massart 2013.)
11. Bishop, C. M. (2006). *Pattern Recognition and Machine Learning.* Springer.
12. Bloom, B. H. (1970). *Space/time trade-offs in hash coding with allowable errors.* Commun. ACM 13(7):422–426. [doi:10.1145/362686.362692](https://doi.org/10.1145/362686.362692)
13. Boucheron, S., Lugosi, G., Massart, P. (2013). *Concentration Inequalities: A Nonasymptotic Theory of Independence.* Oxford Univ. Press.
14. Broder, A. Z. (1997). *On the Resemblance and Containment of Documents.* SEQUENCES, IEEE. [doi:10.1109/SEQUEN.1997.666900](https://doi.org/10.1109/SEQUEN.1997.666900)
15. Broder, A. Z., Charikar, M., Frieze, A. M., Mitzenmacher, M. (1998). *Min-Wise Independent Permutations.* STOC. [doi:10.1145/276698.276781](https://doi.org/10.1145/276698.276781)
16. Charikar, M. (2002). *Similarity Estimation Techniques from Rounding Algorithms.* STOC. [doi:10.1145/509907.509965](https://doi.org/10.1145/509907.509965)
17. Charikar, M., Chen, K., Farach-Colton, M. (2002). *Finding Frequent Items in Data Streams.* ICALP. [doi:10.1007/3-540-45465-9_59](https://doi.org/10.1007/3-540-45465-9_59)
18. Cormode, G., Muthukrishnan, S. (2004). *An Improved Data Stream Summary: The Count-Min Sketch and its Applications.* J. Algorithms 55(1):58–75. [doi:10.1016/j.jalgor.2003.12.001](https://doi.org/10.1016/j.jalgor.2003.12.001)
19. Cormode, G., Garofalakis, M. N., Johnson, T., Shkapenyuk, V. (2008). *Tracking icebergs in data streams.* ACM TODS 33(4).
20. Datar, M., Immorlica, N., Indyk, P., Mirrokni, V. S. (2004). *Locality-sensitive hashing scheme based on p-stable distributions.* SoCG. [doi:10.1145/997817.997857](https://doi.org/10.1145/997817.997857)
21. Drineas, P., Kannan, R., Mahoney, M. W. (2006). *Fast Monte Carlo Algorithms for Matrices I–III.* TCS. [doi:10.1016/j.tcs.2005.10.013](https://doi.org/10.1016/j.tcs.2005.10.013)
22. Drineas, P., Mahoney, M. W. (2016). *RandNLA: randomized numerical linear algebra.* Commun. ACM 59(6):80–90. [doi:10.1145/2842602](https://doi.org/10.1145/2842602)
23. Dubhashi, D. P., Panconesi, A. (2009). *Concentration of Measure for the Analysis of Randomized Algorithms.* Cambridge Univ. Press.
24. Dunning, T. (2019). *Computing Extremely Accurate Quantiles Using t-Digests.* arXiv:1902.04023. [link](https://arxiv.org/abs/1902.04023)
25. Fan, B., Andersen, D. G., Kaminsky, M., Mitzenmacher, M. D. (2014). *Cuckoo Filter: Practically Better Than Bloom.* CoNEXT. [doi:10.1145/2674005.2674994](https://doi.org/10.1145/2674005.2674994)
26. Flajolet, P., Fusy, É., Gandouet, O., Meunier, F. (2007). *HyperLogLog: the analysis of a near-optimal cardinality estimation algorithm.* DMTCS Proc. AH:127–146. [link](https://algo.inria.fr/flajolet/Publications/FlFuGaMe07.pdf)
27. Gelman, A., Carlin, J. B., Stern, H. S., Dunson, D. B., Vehtari, A., Rubin, D. B. (2013). *Bayesian Data Analysis.* 3rd ed., CRC.
28. Geman, S., Geman, D. (1984). *Stochastic Relaxation, Gibbs Distributions, and the Bayesian Restoration of Images.* IEEE TPAMI PAMI-6(6):721–741. [doi:10.1109/TPAMI.1984.4767596](https://doi.org/10.1109/TPAMI.1984.4767596)
29. Greenwald, M., Khanna, S. (2001). *Space-Efficient Online Computation of Quantile Summaries.* SIGMOD. [doi:10.1145/375663.375670](https://doi.org/10.1145/375663.375670)
30. Halko, N., Martinsson, P.-G., Tropp, J. A. (2011). *Finding Structure with Randomness.* SIAM Review 53(2):217–288. [doi:10.1137/090771806](https://doi.org/10.1137/090771806)
31. Har-Peled, S., Sharir, M. (2011). *Relative (p, ε)-approximations in geometry.* DCG 45:462–496. [doi:10.1007/s00454-010-9283-3](https://doi.org/10.1007/s00454-010-9283-3)
32. Harchol-Balter, M. (2013). *Performance Modeling and Design of Computer Systems.* Cambridge Univ. Press.
33. Hastings, W. K. (1970). *Monte Carlo sampling methods using Markov chains and their applications.* Biometrika 57(1):97–109. [doi:10.1093/biomet/57.1.97](https://doi.org/10.1093/biomet/57.1.97)
34. Heule, S., Nunkesser, M., Hall, A. (2013). *HyperLogLog in Practice.* EDBT. [doi:10.1145/2452376.2452456](https://doi.org/10.1145/2452376.2452456)
35. Hoeffding, W. (1963). *Probability Inequalities for Sums of Bounded Random Variables.* JASA 58(301):13–30. [doi:10.2307/2282952](https://doi.org/10.2307/2282952)
36. Howard, S. R., Ramdas, A., McAuliffe, J., Sekhon, J. (2021). *Time-uniform, nonparametric, nonasymptotic confidence sequences.* Ann. Statist. 49(2):1055–1080. [doi:10.1214/20-AOS1998](https://doi.org/10.1214/20-AOS1998)
37. Indyk, P. (2000). *Stable distributions, pseudorandom generators, embeddings, and data stream computation.* FOCS. [doi:10.1109/SFCS.2000.892008](https://doi.org/10.1109/SFCS.2000.892008)
38. Kingman, J. F. C. (1961). *The single server queue in heavy traffic.* Math. Proc. Cambridge Phil. Soc. 57(4):902–904. [doi:10.1017/S0305004100036094](https://doi.org/10.1017/S0305004100036094)
39. Kipf, A., Kipf, T. (2018). *Cardinality Estimation with Deep Neural Networks.* arXiv:1805.02244. [link](https://arxiv.org/abs/1805.02244)
40. Kleinrock, L. (1975). *Queueing Systems, Vol. I: Theory.* Wiley.
41. Levin, D. A., Peres, Y., Wilmer, E. L. (2017). *Markov Chains and Mixing Times.* 2nd ed., AMS.
42. Little, J. D. C. (1961). *A Proof for the Queuing Formula L = λW.* Operations Research 9(3):383–387. [doi:10.1287/opre.9.3.383](https://doi.org/10.1287/opre.9.3.383)
43. Lovász, L., Simonovits, M. (1993). *Random walks in a convex body and an improved volume algorithm.* Random Structures & Algorithms 4(4):359–412. [doi:10.1002/rsa.3240040403](https://doi.org/10.1002/rsa.3240040403)
44. Mahoney, M. W. (2011). *Randomized Algorithms for Matrices and Data.* FnT ML 3(2):123–224. [doi:10.1561/2200000035](https://doi.org/10.1561/2200000035)
45. McDiarmid, C. (1989). *On the method of bounded differences.* Surveys in Combinatorics 141:148–188, Cambridge UP.
46. Metropolis, N., Rosenbluth, A. W., Rosenbluth, M. N., Teller, A. H., Teller, E. (1953). *Equation of State Calculations by Fast Computing Machines.* J. Chem. Phys. 21(6):1087–1092. [doi:10.1063/1.1699114](https://doi.org/10.1063/1.1699114)
47. Neyman, J. (1934). *On the two different aspects of the representative method.* JRSS 97(4):558–625. [doi:10.2307/2342192](https://doi.org/10.2307/2342192)
48. Neyman, J., Pearson, E. S. (1933). *On the Problem of the Most Efficient Tests of Statistical Hypotheses.* Phil. Trans. R. Soc. A 231:289–337. [doi:10.1098/rsta.1933.0009](https://doi.org/10.1098/rsta.1933.0009)
49. Thorup, M. (2013). *Bottom-k and priority sampling, set similarity and subset sums with minimal independence.* STOC.
50. Tzoumas, K., Deshpande, A., Hellerstein, J. M. (2013). *Statistics-based Cardinality Estimation for Complex Queries.* ACM TODS 38(3):17. [doi:10.1145/2512453](https://doi.org/10.1145/2512453)
51. Valiant, L. G. (1984). *A Theory of the Learnable.* Commun. ACM 27(11):1134–1142. [doi:10.1145/1968.1972](https://doi.org/10.1145/1968.1972)
52. Vapnik, V. N., Chervonenkis, A. Ya. (1971). *On the Uniform Convergence of Relative Frequencies of Events to Their Probabilities.* Theory Probab. Appl. 16(2):264–280. [doi:10.1137/1116025](https://doi.org/10.1137/1116025)
53. Wald, A. (1945). *Sequential Tests of Statistical Hypotheses.* Ann. Math. Statist. 16(2):117–186. [doi:10.1214/aoms/1177731118](https://doi.org/10.1214/aoms/1177731118)
54. Wald, A., Wolfowitz, J. (1948). *Optimum Character of the Sequential Probability Ratio Test.* Ann. Math. Statist. 19(3):326–339. [doi:10.1214/aoms/1177730204](https://doi.org/10.1214/aoms/1177730204)
55. Welch, B. L. (1947). *The generalization of Student's problem when several different population variances are involved.* Biometrika 34(1-2):28–35. [doi:10.1093/biomet/34.1-2.28](https://doi.org/10.1093/biomet/34.1-2.28)
56. Woodruff, D. P. (2014). *Sketching as a Tool for Numerical Linear Algebra.* FnT TCS 10(1-2):1–157. [doi:10.1561/0400000060](https://doi.org/10.1561/0400000060)
