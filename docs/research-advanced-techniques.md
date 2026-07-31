# Advanced Database Techniques — Research Synthesis for turboGP

## Sources Researched
1. **CedarDB** — "Simple, Efficient, and Robust Hash Tables for Join Processing" (DaMoN'24)
2. **Cockroach Labs** — "40x faster hash joiner with vectorized execution" (2019)
3. **CMU 15-721** — Parallel Hash Join Algorithms (Andy Pavlo)
4. **Chen et al.** — "Improving Hash Join Performance through Prefetching" (TODS 2007, 314 citations)
5. **DuckDB** — Push-Based Execution / Morsel-Driven Parallelism (Raasveldt)
6. **database-doctor.com** — TPC-H Q5 bushy join plan analysis
7. **VLDB** — "Analyzing Vectorized Hash Tables Across CPU Architectures" (Böther et al.)
8. **DuckDB discussion #18983** — Linear probing hash join 2× faster than DuckDB

---

## Technique 1: Bloom-Filter-Tagged Chaining Hash Table (CedarDB)

**The trick**: Squeeze a 16-bit Bloom filter tag into the *unused upper 16 bits* of 64-bit pointers. This makes the probe fast path only **10 x86 instructions** and filters >99% of non-matching keys without touching the key data.

### Current turboGP problem
`HashMap<u64, Vec<usize>>` does:
- `HashMap::get(&key)` → hash + bucket lookup + Vec comparison
- Each probe: ~50-100ns due to HashMap overhead + Vec heap access
- For Q5: 6M probes × 100ns = 600ms just in probe lookups

### CedarDB layout
```rust
struct CedarHashTable {
    directory: Vec<u64>,       // upper 16 bits = bloom tag, lower 48 bits = pointer
    // ... chained entries ...
}

fn lookup(&self, key: u64, hash: u64) -> bool {
    let slot = hash >> shift;                    // shr (1 instr)
    let entry = self.directory[slot];            // mov (1 instr, random access)
    if !could_contain(entry as u16, hash) {      // 3 instrs (andn + table lookup)
        return false;                            // 99% of probes exit here
    }
    // Slow path: follow pointer, compare key
    ...
}
```

**Key insight**: The Bloom filter check is *fused* with the null check. An empty slot has tag=0, which fails `could_contain` for any non-zero hash. So we get null-check + bloom-check in 3 instructions.

### Implementation for turboGP
Replace `HashMap<u64, Vec<usize>>` with:
```rust
struct FastHashTable {
    directory: Vec<u64>,        // tag(16) | ptr(48), power-of-2 size
    entries: Vec<Entry>,        // chaining entries: {key, row_idx, next}
}
```

**Expected impact**: Q5 probe phase 600ms → ~60ms (10× from probe alone).

---

## Technique 2: Software Pipelining / Group Prefetching (Chen et al. 2007)

**The trick**: Hash join probes do random memory accesses (~200-400 cycle latency). Hide this by prefetching the *next* batch of slots while processing the *current* batch.

### Two variants
1. **Group prefetching**: Prefetch K slots, then process them after a delay
2. **Software pipelining**: Interleave prefetch and process across iterations

```rust
// Group prefetching (simpler):
const GROUP: usize = 8;
let mut slots = [0usize; GROUP];
for chunk in probe_keys.chunks(GROUP) {
    // Phase 1: compute slots and prefetch
    for (i, &key) in chunk.iter().enumerate() {
        slots[i] = (hash(key) >> shift) as usize;
        _mm_prefetch::<_MM_HINT_T0>(&directory[slots[i]] as *const _ as *const i8);
    }
    // Phase 2: process (latency hidden by other prefetches)
    for (i, &key) in chunk.iter().enumerate() {
        let entry = directory[slots[i]];
        if could_contain(entry, hash(key)) {
            // ... full check ...
        }
    }
}
```

**Expected impact**: Hides 200-cycle cache miss latency → 2-3× probe speedup on large tables.

---

## Technique 3: Vectorized Probe (CockroachDB / Zukowski)

**The trick**: Process 8-16 probe keys at once using AVX-512 gather + masked comparison, instead of one-at-a-time.

### Algorithm (Zukowski 2005)
```
1. Compute bucket for each of N probe keys (vectorized hash)
2. lookupInitial: gather first entry in each bucket → groupIdV[0..N]
3. Loop while any toCheck:
   a. check: gather keys from hash table, compare with probe keys (vectorized)
   b. selectMisses: compact non-matching indices
   c. findNext: follow chain pointers for non-matching
4. gather: project output columns using groupIdV
```

### Why it's fast
- Each loop iteration is a tight loop over a single column
- SIMD gather (`_mm512_i64gather_epi64`) loads 8 entries in one instruction
- Mask compaction (`_mm512_mask_compress_epi64`) removes non-matching rows
- No per-row branching — branches become mask operations

**Expected impact**: 4-8× over scalar probe on AVX-512 hardware.

---

## Technique 4: CRC32 Hash Instead of xxh3 (CedarDB)

**The trick**: For hash table keys, use hardware CRC32 (`_mm_crc32_u64`) instead of xxh3. It's 3-4× faster and produces well-distributed hashes.

```rust
#[cfg(target_arch = "x86_64")]
unsafe fn hash_crc32(key: u64, seed: u32) -> u64 {
    let crc = _mm_crc32_u64(seed as u64, key) as u32;
    let k = 0x8648DBDBu64;
    (crc as u64).wrapping_mul((k << 32) + 1)
}
```

**Why it matters**: When the hash table fits in L2/L3 cache, hashing becomes the bottleneck. xxh3 needs ~20 instructions; CRC32 needs 2.

**Expected impact**: 2× hash speedup → 30% probe speedup when cache-resident.

---

## Technique 5: Bushy Join Plans for Q5 (database-doctor analysis)

**The trick**: Q5's 6-table join should NOT be left-deep. DuckDB and SQL Server use a **bushy** plan:

```
        JOIN
       /    \
     JOIN    JOIN
    /   \   /   \
  (c-o) (l-s) (n-r)
```

### Why left-deep is bad for Q5
turboGP's `join_tables_smart` does left-to-right joins:
```
((((((region ⋈ nation) ⋈ supplier) ⋈ customer) ⋈ orders) ⋈ lineitem))
```
This materializes intermediate results that grow then shrink. A bushy plan:
- Joins region⋈nation first (5 × 25 = 125 rows, tiny)
- Joins customer⋈orders in parallel
- Joins lineitem⋈supplier in parallel
- Merges at the end

**Expected impact**: Q5 from 14.7s → ~2s (7×) just from plan shape, before any kernel work.

---

## Technique 6: Defer Expression Evaluation (DuckDB Q5 trick)

**The trick**: Don't compute `l_extendedprice * (1 - l_discount)` until *after* all joins reduce the row count.

### Current turboGP
Computes the expression during/after join materialization on 6M rows.

### DuckDB approach
Defers to the final PROJECT where row count is ~7K (after all joins filter).

**Impact for Q5**: 6M expression evaluations → 7K = 857× reduction in arithmetic work.

---

## Technique 7: Morsel-Driven Push Pipeline (DuckDB)

**The trick**: Replace pull-based Volcano execution with push-based pipelines that process 8K-64K row morsels through filter→project→aggregate in one pass, avoiding intermediate materialization.

### Current turboGP
```
scan lineitem (6M) → materialize → filter → materialize → join → materialize → agg
```
Each arrow = full table copy.

### DuckDB morsel pipeline
```
source → [filter] → [project] → [hash_join_build sink] → ...
                  ↘ [hash_join_probe] → [aggregate sink] → result
```
No intermediate materialization. Each operator processes a morsel and pushes to the next.

**Expected impact**: Q3 from 1.2s → ~200ms (6×), Q18 from 3.6s → ~500ms.

---

## Priority Implementation Order

| Priority | Technique | Target Query | Expected Impact | Effort |
|----------|-----------|-------------|-----------------|--------|
| **P0** | Bushy join plan (Technique 5) | Q5 | 14.7s → 2s | Medium |
| **P0** | CedarDB bloom-tagged HT (Technique 1) | Q5, Q3, Q10 | 10× probe | High |
| **P1** | CRC32 hash (Technique 4) | All joins | 2× hash | Low |
| **P1** | Group prefetching (Technique 2) | Q5 probe | 2-3× probe | Medium |
| **P2** | Defer expression eval (Technique 6) | Q5, Q1, Q14 | 6M→7K evals | Medium |
| **P2** | Morsel pipeline (Technique 7) | Q3, Q18 | 6× | Very High |
| **P3** | Vectorized probe (Technique 3) | All joins | 4-8× | Very High |

## Recommended Next Steps
1. **Implement P0 (bushy + CedarDB HT)** — this alone should bring Q5 from 14.7s to <1s
2. **Add CRC32 hash + prefetching (P1)** — incremental wins on all joins
3. **Defer expression eval (P2)** — benefits Q1/Q5/Q14 aggregation
4. **Morsel pipeline (P2)** — biggest architectural change, defer until P0/P1 proven
