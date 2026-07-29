# ADR-017: Similarity search — brute VPOPCNTDQ for ≤10⁶, LSH above

## Status
Accepted

## Confidence
85%

## Context

Similarity search (Hamming distance) needs to scale from small (thousands of cells) to large (billions). The right algorithm depends on the dataset size:
- **Brute force** (scan all cells, compute Hamming distance): O(n), exact, fast for small n
- **LSH** (locality-sensitive hashing): sublinear for large n, but approximate with setup overhead

The crossover point depends on the kernel throughput. With AVX-512 `VPOPCNTDQ`, brute force achieves ~8 G cells/sec. For n = 10⁶, that's ~125 µs — fast enough. For n = 10⁹, it's ~125 ms — too slow, use LSH.

## Decision

**Use a two-tier similarity search:**
- **n ≤ 10⁶ cells**: brute-force scan with AVX-512 `VPOPCNTDQ` kernel
- **n > 10⁶ cells**: LSH (Andoni-Indyk) with re-ranking

The brute-force kernel:
```asm
; Process 8 u64s per cycle
vpxorq   zmm2, zmm0, zmm1       ; XOR target with data
vpopcntq zmm3, zmm2             ; popcount per lane
vpcmpgtq k1, zmm3, zmm_threshold ; mask of lanes within distance
kmovq    rax, k1                ; mask to register
popcnt   rax, rax               ; count matches
```

For LSH: use random hyperplane LSH (for cosine/Hamming). Parameters: L = 10 tables, k = 12 hash bits (matching the LSH index in `src/index/lsh.rs`).

## Consequences

### Positive
- **Exact results for small datasets** (no approximation error)
- **8 G cells/sec** for brute force — fast enough for most interactive queries
- **Sublinear for large datasets** (LSH achieves O(n^{1/c}) for approximation factor c)
- The `VPOPCNTDQ` kernel is the engine's signature instruction — showcases the instruction-first thesis

### Negative
- The 10⁶ threshold is a heuristic — may need tuning per workload
- LSH has a setup cost (building the hash tables) — amortized over queries
- LSH is approximate (may miss some matches) — the re-ranking step mitigates this

## Alternatives considered

1. **Always brute force** — too slow for n > 10⁷. Rejected.
2. **Always LSH** — unnecessary overhead for small n, and approximate when exact is affordable. Rejected.
3. **HNSW (Hierarchical Navigable Small World)** — excellent recall but complex to implement and not bit-native. Deferred.
4. **GPU acceleration** — 10× faster but adds a GPU dependency. Out of scope for the instruction-first thesis.

## Compatibility

- Compatible with ADR-001 (64-bit word): Hamming distance operates on u64 bit patterns
- Compatible with ADR-003 (CPUID dispatch): `VPOPCNTDQ` is gated on `avx512vpopcntdq`
- Compatible with ADR-007 (1024 batch): brute force processes 1024 cells per kernel invocation
- Compatible with ADR-018 (morsel executor): each morsel is a brute-force scan unit

## References
- Andoni & Indyk, "Near-Optimal Hashing Algorithms" CACM 2008
- Intel, "AVX-512 VPOPCNTDQ Instruction" ISA reference
- `src/kernel/similarity.rs` (HammingAvx512 kernel implementation)
- `src/index/lsh.rs` (LSH index implementation)
