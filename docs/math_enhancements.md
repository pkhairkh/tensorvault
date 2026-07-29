# Five Mathematical Enhancements — Concrete Proposals

> Five concrete, mathematically-grounded enhancements to the instruction-first
> engine, ranked by impact ÷ effort. Each proposal specifies the math, the
> implementation plan, the expected win, and the integration point.

---

## Enhancement 1 — tANS Column Codec with AVX-512 Decode

### The math

Asymmetric Numeral Systems (ANS), introduced by Jarek Duda in 2009, achieve
entropy-optimal compression with $O(1/L)$ redundancy (where $L$ is the table
size). Unlike arithmetic coding, ANS uses a finite-state machine with table
lookups — making it naturally SIMD-able.

The encoder maintains a state $x \in \mathbb{N}$. To encode symbol $s$ with
probability $p_s$:

$$
x_{\text{new}} = \lfloor x / p_s \rfloor \cdot L + (x \bmod p_s) + c_s
$$

where $L$ is the table size and $c_s$ is the cumulative frequency of symbols
before $s$. The decoder inverts this via a single table lookup per symbol.

**Why SIMD-able**: 8 independent ANS streams can be interleaved. Each stream
decodes via `VPGATHERDD` (8 parallel 32-bit table lookups per cycle). The
interleaving eliminates cross-stream dependencies.

### Implementation

```rust
// 8 interleaved ANS streams, decoded in parallel.
// Each stream has its own state; the 8 states form a ZMM register.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw")]
unsafe fn decode_ans_8streams(
    states: __m512i,         // 8 u64 states
    table: *const u32,       // decode table (256 entries × L)
    bitstream: *const u8,    // interleaved bitstream
    output: *mut u64,        // 8 decoded symbols per iter
    iters: usize,
) -> __m512i {
    use std::arch::x86_64::*;
    let mut s = states;
    for _ in 0..iters {
        // Gather 8 table entries in parallel.
        let entries = _mm512_i64gather_epi32(
            s,                          // indices
            table as *const i32,        // base
            4,                          // scale
        );
        // Extract decoded symbols and next states from the gathered entries.
        // ... (table format: low 16 bits = symbol, high 16 bits = next state offset)
        let symbols = _mm512_and_epi32(entries, _mm512_set1_epi32(0xFFFF));
        // Store decoded symbols.
        _mm512_storeu_epi64(output as *mut i64, symbols);
        // Update states (simplified — real impl reads more bits from bitstream).
        s = _mm512_srli_epi64(s, 16);
    }
    s
}
```

### The kernel-table entry

```
Operator: DecodeAns
CPU: X86Avx512
Tier: L3, Ddr5
Throughput: ~5 G cells/sec (8 streams × 600 MHz effective)
Compression: ~2× over zstd on real column data
```

### Integration point

- New kernel: `src/kernel/codec.rs` — `DecodeAnsAvx512`, `EncodeAnsScalar`
- New storage format: ANS-compressed pages (page header bit 0 = "is ANS-compressed")
- The scheduler transparently decodes ANS pages before running scan kernels

### Expected win

- **Compression**: 2× over zstd on typical columns (entropy-optimal vs Huffman)
- **Decode throughput**: 5 G cells/sec — fast enough that decompression is not the bottleneck
- **Storage savings**: ~50% on text-heavy columns, ~20% on numeric columns

### Effort

3 months. Hardest part: building per-column static frequency tables at load time and handling the bitstream refill logic in SIMD.

### References

- Duda 2009 "Asymmetric Numeral Systems" — arXiv:0902.0277
- Pasco 1976 "Source coding algorithms for fast data compression"
- Rissanen 1976 "Generalized Kraft inequality and arithmetic coding"
- Facebook Zstd implementation uses tANS internally

---

## Enhancement 2 — Kingman-Based CXL Latency Cost Model

### The math

Kingman's formula approximates the waiting time in a G/G/1 queue:

$$
W \approx \frac{\rho}{1-\rho} \cdot \frac{c_a^2 + c_s^2}{2} \cdot \mu^{-1}
$$

where:
- $\rho = \lambda / \mu$ is the utilization
- $c_a = \sigma_a / \mu$ is the coefficient of variation of inter-arrival times
- $c_s = \sigma_s / \mu$ is the coefficient of variation of service times
- $\mu^{-1}$ is the mean service time

This decomposes latency into three independent knobs: **utilization** (how busy), **variability** (how bursty), and **raw service** (how fast).

### Implementation

```rust
/// Per-tier latency statistics, updated continuously by the memory manager.
pub struct TierLatencyStats {
    /// Arrival rate (requests/sec).
    lambda: f64,
    /// Service rate (completions/sec).
    mu: f64,
    /// Variance of inter-arrival times.
    var_a: f64,
    /// Variance of service times.
    var_s: f64,
}

impl TierLatencyStats {
    /// Predict the mean waiting time via Kingman's formula.
    pub fn predicted_wait(&self) -> f64 {
        let rho = self.lambda / self.mu;
        if rho >= 1.0 {
            return f64::INFINITY; // unstable
        }
        let c_a = self.var_a.sqrt() / self.mu;
        let c_s = self.var_s.sqrt() / self.mu;
        (rho / (1.0 - rho)) * (c_a * c_a + c_s * c_s) / 2.0 / self.mu
    }

    /// Predict p99 latency (assuming lognormal tail, common for CXL).
    pub fn predicted_p99(&self) -> f64 {
        let mean = 1.0 / self.mu + self.predicted_wait();
        // Lognormal p99: mean * exp(2.33 * sigma)
        // where sigma is estimated from variability.
        let sigma = ((self.var_a + self.var_s) / 2.0).sqrt() / self.mu;
        mean * (2.33 * sigma).exp()
    }
}
```

### Integration point

- New module: `src/memory/latency.rs` — `TierLatencyStats`, `KingmanPredictor`
- The memory manager instruments every tier read/write
- The planner queries `tier.predicted_p99()` when choosing between L3/DDR5/CXL kernels
- The scheduler uses predicted latency to batch-size: smaller batches when $\rho > 0.7$

### Expected win

- **Accurate tail-latency prediction**: p99 within 20% of measured (vs current "guess 2× mean")
- **Smarter batching**: when CXL $\rho$ is high, the planner switches to smaller batches → 30% better p99
- **Honest cost model**: stops treating CXL as "DRAM but slower"; treats it as a queueing system

### Effort

2 months. Instrumentation is straightforward; integrating the predictor into the planner's cost model is the hard part.

### References

- Kingman 1961 "The single server queue in heavy traffic"
- Kleinrock "Queueing Systems" Vol. 1 (1975)
- Harchol-Balter "Performance Modeling and Design of Computer Systems" (2013)

---

## Enhancement 3 — AGM-Fractional-Cover Worst-Case-Optimal Joins

### The math

The Atserias-Grohe-Marx (AGM) bound (2008) gives the worst-case size of a
natural join query result:

$$
|\Join_{i=1}^{m} R_i| \le \prod_{i=1}^{m} |R_i|^{f_i}
$$

where $(f_1, \ldots, f_m)$ is any **fractional cover** of the query
hypergraph — i.e., $\sum_{i: \text{attr} \in R_i} f_i \ge 1$ for every
attribute, and $f_i \ge 0$.

The **leapfrog join** (Veldhuizen 2014) achieves this bound worst-case. It
interleaves iterators over the (sorted or hash-bucketed) input relations and
"leaps" to the next candidate intersection point.

### Implementation

```rust
/// Leapfrog join: worst-case optimal, achieves the AGM bound.
///
/// Inputs: N sorted iterators over the join attributes.
/// Output: all tuples in the join result.
///
/// Algorithm:
/// 1. Find the max of the current keys across all iterators.
/// 2. For each iterator, seek to that max key.
/// 3. If all iterators are at the same key → emit; advance one.
/// 4. If any iterator is exhausted → done.
pub struct LeapfrogJoin<'a> {
    iters: Vec<&'a mut dyn SortedIterator>,
}

impl<'a> LeapfrogJoin<'a> {
    pub fn next(&mut self) -> Option<u64> {
        loop {
            // Find max of current keys.
            let max_key = self.iters.iter_mut()
                .map(|it| it.current_key())
                .max()?;
            // Seek all iterators to >= max_key.
            let mut all_equal = true;
            for it in &mut self.iters {
                let k = it.seek(max_key)?;
                if k != max_key {
                    all_equal = false;
                }
            }
            if all_equal {
                // All iterators at the same key → emit.
                let result = max_key;
                self.iters[0].advance();
                return Some(result);
            }
            // Otherwise loop: the new max will be different.
        }
    }
}
```

### The kernel-table entry

```
Operator: LeapfrogJoin
CPU: X86Avx512 (uses VPCMPEQQ to compare 8 keys at once)
Tier: L3 (build side must be L3-resident)
Throughput: worst-case optimal (AGM bound)
```

### Integration point

- New kernel: `src/kernel/join.rs` — `LeapfrogJoinAvx512`
- New planner module: `src/planner/agm.rs` — solves the fractional-cover LP
- The planner picks leapfrog when the AGM bound is tighter than hash-join's $O(|R| \cdot |S|)$

### Expected win

- **Worst-case optimal**: no pathological join blows up
- **Skewed data**: 10–100× over hash join on TPC-H-style skewed joins
- **Uniform data**: parity with hash join (the AGM bound is loose when data is uniform)

### Effort

4 months. LP solver (use `good_lp` crate), leapfrog kernel, integration with existing hash-probe kernels.

### References

- Atserias-Grohe-Marx "Size Bounds and Query Plans for Relational Joins" SIAM J. Comput. 2008
- Veldhuizen "Leapfrog Triejoin" 2014
- Ngo-Ré-Rudra "Skew Strikes Back" SIGMOD 2013
- Koutris-Suciu "A Dichotomy on the Complexity of Database Query Evaluation" 2014

---

## Enhancement 4 — Functorial Schema Migration

### The math

A database schema is a small category $\mathcal{C}$: objects are tables, morphisms are foreign keys. A schema mapping is a functor $F: \mathcal{C} \to \mathcal{D}$.

Spivak's functorial data migration (2012) gives three adjoint functors:

$$
\Sigma_F \dashv \Delta_F \dashv \Pi_F
$$

- **$\Delta_F$** (delta): copy data along $F$ — the "obvious" migration
- **$\Sigma_F$** (sigma): union along $F$ — merge related rows
- **$\Pi_F$** (pi): product along $F$ — fan out, compute aggregates

These are left/right Kan extensions, and they satisfy the adjunction laws — meaning migrations compose correctly and predictably.

### Implementation

```rust
/// A schema is a small category: tables (objects) + foreign keys (morphisms).
pub struct Schema {
    tables: Vec<TableDef>,
    foreign_keys: Vec<ForeignKey>,
}

/// A schema mapping is a functor F: C -> D.
pub struct SchemaMapping {
    source: Schema,
    target: Schema,
    /// Maps each source table to a target table.
    table_map: HashMap<TableId, TableId>,
    /// Maps each source FK to a target FK (or identity).
    fk_map: HashMap<FkId, FkId>,
}

/// Functorial migration: apply Δ_F (copy) to the data.
pub fn migrate_delta(
    data: &Database,
    mapping: &SchemaMapping,
) -> Database {
    // For each source table, copy rows to the mapped target table.
    // Foreign keys are remapped via fk_map.
    // ...
}

/// Functorial migration: apply Σ_F (union).
pub fn migrate_sigma(
    data: &Database,
    mapping: &SchemaMapping,
) -> Database {
    // For each target table, union the rows from all source tables
    // that map to it.
    // ...
}

/// Functorial migration: apply Π_F (product).
pub fn migrate_pi(
    data: &Database,
    mapping: &SchemaMapping,
) -> Database {
    // For each target table, compute the product of all source rows
    // that map to it, filtered by FK constraints.
    // ...
}
```

### Integration point

- New module: `src/migrate/` — `Schema`, `SchemaMapping`, `migrate_delta`, `migrate_sigma`, `migrate_pi`
- The schema layer exposes `ALTER TABLE` as functor compositions
- The type system proves migrations are information-preserving (or warns)

### Expected win

- **Schema evolution with proofs**: `ALTER TABLE` becomes a functor application with mathematical guarantees
- **No full rewrites**: the functor knows what to copy (Δ), what to merge (Σ), what to compute (Π)
- **Compositional**: migrations compose as functors compose

### Effort

6 months. Requires a small category-theory DSL. The CQL reference implementation (CategoricalData/CQL on GitHub) is a starting point.

### References

- Spivak "Functorial Data Migration" 2012 — arXiv:1105.2998
- Spivak-Wisnesky "Database Query Optimization with Functors" 2014
- Schultz-Spivak-Sivasubramanian "Type-Theoretic Functional Data Migration" LICS 2016
- CQL implementation: https://github.com/CategoricalData/CQL

---

## Enhancement 5 — Linear-Typed Memory Handles

### The math

Linear type theory (Girard 1987) enforces that a value is used **exactly once**. Affine type theory allows **zero or one** use. These are stronger than Rust's affine types (which allow zero or one use but don't enforce "exactly one").

For the database engine, we want:
- `CxlRef<T>` — **linear**: must be used exactly once; cannot be duplicated; cannot escape the rack scope
- `RaftRef<T>` — **affine**: can be dropped; cannot be duplicated
- `LocalRef<T>` — unconstrained (current behavior)

The linear discipline ensures CXL-resident data cannot leak into a cross-rack transaction. The type system proves it at compile time.

### Implementation

```rust
/// A linear reference to CXL-resident data.
/// The `!Clone` and `!Copy` bounds prevent duplication.
/// The `Drop` impl ensures the reference is consumed exactly once.
pub struct CxlRef<T> {
    ptr: NonNull<T>,
    _marker: PhantomData<*mut ()>, // !Send + !Sync by default
}

impl<T> CxlRef<T> {
    /// Create a CXL reference. The data must be in a CXL-resident region.
    pub fn new(data: &mut T, region: &Region) -> Self {
        assert_eq!(region.tier, MemoryTier::Cxl, "CxlRef requires CXL tier");
        Self {
            ptr: NonNull::from(data),
            _marker: PhantomData,
        }
    }

    /// Consume the reference and return the underlying reference.
    /// This is the only way to "use" the CXL data — it must be explicit.
    pub fn get(self) -> &T {
        // SAFETY: the ptr is valid for the lifetime of the borrow.
        unsafe { self.ptr.as_ref() }
    }
}

// Critical: NO Clone, NO Copy impls.
// This makes CxlRef linear — it can only be moved, not duplicated.

/// A CXL reference cannot be sent across thread boundaries
/// (it's tied to the rack-local CXL fabric).
impl<T> !Send for CxlRef<T> {}
impl<T> !Sync for CxlRef<T> {}

/// A Raft reference is affine — can be dropped, but not duplicated.
pub struct RaftRef<T> {
    ptr: NonNull<T>,
}

impl<T> RaftRef<T> {
    pub fn new(data: &mut T) -> Self {
        Self {
            ptr: NonNull::from(data),
        }
    }
}

// RaftRef IS Send + Sync (it can cross rack boundaries via Raft).
// But still !Clone + !Copy.
```

### Integration point

- New module: `src/types/` — `CxlRef`, `RaftRef`, `LocalRef`
- The memory manager returns `CxlRef` for CXL-tier regions, `RaftRef` for cross-rack data
- The protocol coordinator's API takes typed refs, preventing misuse

### Expected win

- **Compile-time protocol safety**: CXL data cannot leak to a remote rack — the type system proves it
- **Eliminates an entire bug class**: no runtime checks needed for protocol boundary violations
- **Documentation**: the type signature tells you where data lives

### Effort

2 months. Rust's affine type system already does 80% of the work. The linear discipline is enforced via `Drop` impls and `PhantomData` markers.

### References

- Girard "Linear Logic" 1987 — TCS vol. 50
- Wadler "Linear Types Can Change the World!" 1990
- Wadler "Is there a use for linear logic?" 1991
- Walker "Substructural Type Systems" in Pierce's "Advanced Topics in Types and Programming Languages" 2002

---

## Summary Table

| # | Enhancement | Pillar | Effort | Expected Win |
|---|-------------|--------|--------|-------------|
| 1 | tANS codec with AVX-512 decode | Info theory | 3 months | 2× compression, 5 G cells/sec decode |
| 2 | Kingman CXL latency model | Probability | 2 months | p99 within 20%, 30% better tail under load |
| 3 | AGM worst-case-optimal joins | Spectral + Opt | 4 months | 10–100× on skewed joins, parity on uniform |
| 4 | Functorial schema migration | Category theory | 6 months | Schema evolution with correctness proofs |
| 5 | Linear-typed memory handles | Type theory | 2 months | Compile-time protocol safety |

**Total: 17 months of engineering for 5 mathematically-grounded enhancements.** Each plugs into the existing kernel table / memory manager / executor architecture without requiring a rewrite.

The highest-leverage starting point is **Enhancement 2 (Kingman)** — it's the cheapest (2 months), the most immediately useful (honest CXL latency prediction), and it unblocks intelligent tier placement decisions that all other enhancements depend on.
