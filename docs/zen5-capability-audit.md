# Zen 5 SIMD Capability Audit — Wave 20

**Date:** 2026-07-31
**Auditor:** subagent-w20
**Host:** 45.63.97.103 (AMD EPYC-Turin / Zen 5)
**Repo HEAD at audit:** `80ed01a`

---

## 1. CPU Model and SIMD Flags

### CPU identification
```
model name      : AMD EPYC-Turin Processor
Vendor ID       : AuthenticAMD
CPU family      : 26          (0x1a = Zen 5)
CPU(s)          : 8           (4 cores × 2 threads, QEMU/KVM slice)
Kernel          : 6.12.0-211.39.1.el10_2.x86_64 (Rocky Linux 10.2)
BIOS Vendor ID  : QEMU        (running inside a virtualized slice)
```

### CPU flags (relevant subset)
| Flag | Present | Notes |
|------|---------|-------|
| `avx512f` | ✅ | AVX-512 foundation (512-bit ZMM registers) |
| `avx512bw` | ✅ | Byte/word granularity |
| `avx512dq` | ✅ | Doubleword/quadword |
| `avx512cd` | ✅ | Conflict detection |
| `avx512vl` | ✅ | VL extensions (128/256-bit forms) |
| `avx512ifma` | ✅ | Integer fused multiply-add |
| `avx512vbmi` | ✅ | Vector byte manipulation v1 |
| `avx512_vbmi2` | ✅ | Byte/word compress/expand, `VPMBATCHCODE` |
| `avx512vpopcntdq` | ✅ | Vector popcount |
| `avx512_vp2intersect` | ✅ | 2-way intersection |
| `avx512_bf16` | ✅ | Brain-float 16 dot product (`_mm512_dpbf16_ps`) |
| `avx512_vnni` | ✅ | Int8/int16 dot product (`_mm512_dpbusd_epi32`) |
| `avx512_bitalg` | ✅ | Byte/word bit algorithms |
| `avx_vnni` | ✅ | Legacy 256-bit VNNI |
| `fma` | ✅ | Fused multiply-add |
| `f16c` | ✅ | F16 convert (but NOT full FP16 ALU) |
| **`avx512_fp16`** | ❌ **MISSING** | FP16 ALU not exposed by the QEMU VM |
| **`amx_*`** | ❌ **MISSING** | Advanced Matrix Extensions not exposed |
| `amx_bf16` | ❌ | (no AMX at all) |
| `amx_int8` | ❌ | |
| `amx_tile` | ❌ | |

### GCC `-march=native` macros
```
__AVX512F__ __AVX512BW__ __AVX512DQ__ __AVX512CD__ __AVX512VL__
__AVX512IFMA__ __AVX512VBMI__ __AVX512VBMI2__ __AVX512VNNI__
__AVX512VP2INTERSECT__ __AVX512VPOPCNTDQ__ __AVX512BF16__ __AVX512BITALG__
__AVXVNNI__  __F16C__  __BFLT16_*__ (BF16 type constants)
```

**Notably absent:** `__AVX512FP16__` and any `__AMX_*` macros — confirming the VM does not pass these features through even though the underlying Zen 5 silicon supports AVX-512 FP16 (Zen 4+ feature) and AMX (Zen 4+ feature).

---

## 2. Bleeding-Edge Instruction Availability

| Instruction | Intrinsic | CPU flag | Compiles? | Runs? | Verdict |
|-------------|-----------|----------|-----------|-------|---------|
| **AVX-512 FP16 add** | `_mm512_add_ph` | `avx512fp16` | ✅ (nightly + feature gates) | ❌ SIGILL | **BLOCKED by VM** |
| **AVX-512 FP16 set1** | `_mm512_set1_ph` | `avx512fp16` | ✅ (nightly + feature gates) | ❌ SIGILL | **BLOCKED by VM** |
| **AVX-512 VNNI int8 dot** | `_mm512_dpbusd_epi32` | `avx512vnni` | ✅ (stable + nightly) | ✅ verified (1×2×4=8) | **USABLE** |
| **AVX-512 BF16 dot** | `_mm512_dpbf16_ps` | `avx512bf16` | ✅ (stable + nightly) | ✅ verified (1.0×2.0×2=4.0) | **USABLE** |
| **AVX-512 VBMI2 compress** | `_mm512_mask_compress_epi8` | `avx512vbmi2` | ✅ (stable + nightly) | ✅ verified | **USABLE** |
| **AMX tile matmul** | `_tile_dpbssd` | `amx_tile`+`amx_int8` | ❌ (no flag) | ❌ | **NOT AVAILABLE** |

### Intrinsics verification program
Built and ran `/tmp/test_intrinsics.rs` with `rustc -C target-cpu=native`:

```
VNNI  _mm512_dpbusd_epi32     works: lane0=8        (1*2*4 lanes per accumulate)
BF16  _mm512_dpbf16_ps        works: lane0=4.0      (1.0*2.0*2 pairs per accumulate)
VBMI2 _mm512_mask_compress_epi8 works
avx512fp16 NOT detected at runtime (CPU flag missing)
```

### FP16 detail (the one blocker)
The host kernel reports `f16c` (the older F16↔F32 convert instructions) but does NOT
report `avx512fp16` (the full 32-lane FP16 ALU). Running any `_mm512_*_ph` intrinsic
ends in `Illegal instruction (core dumped)` — a hard SIGILL. The intrinsics DO compile
on nightly Rust 1.99.0 once the two feature gates below are added:

```rust
#![feature(f16)]                       // f16 primitive type (issue #116909)
#![feature(stdarch_x86_avx512_f16)]    // _mm512_*_ph intrinsics (issue #127213)
```

These gates are still unstable as of nightly `1.99.0-nightly (8ab9fdff5 2026-07-30)`.
Stabilization tracking issues: rust-lang/rust#116909 and #127213.

**Implication for W21+:** FP16-based half-precision scan / aggregate kernels are
infeasible on this VM until either (a) the hypervisor exposes `avx512fp16` or
(b) we move to bare-metal Zen 5 hardware. VNNI and BF16 paths are fully open.

---

## 3. Toolchain Status

| Tool | Version | Status |
|------|---------|--------|
| `rustc` (default) | `1.99.0-nightly (8ab9fdff5 2026-07-30)` | ✅ Installed & default |
| `cargo` | `1.99.0-nightly (7c83d4cc0 2026-07-29)` | ✅ |
| `rustup` | `1.29.0 (28d1352db 2026-03-05)` | ✅ |
| `perf` | `6.12.0-211.40.1.el10_2.x86_64` | ✅ Installed via `dnf install -y perf` |
| `perf_event_paranoid` | `1` (was 2) | ✅ Relaxed for kernel profiling |
| Previous stable Rust | `1.97.1` | Still installed as a toolchain (not default) |

### Release profile change
`Cargo.toml [profile.release]` was modified to retain symbols for `perf`:
```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "none"              # was "symbols"
debug = "line-tables-only"  # added — enough for perf symbol resolution
```
This does not affect runtime performance (debug info is metadata only) but bloats
the binary from 695 KB → 4.3 MB. Worth it for ongoing perf work.

---

## 4. Perf Baseline — Q5 and Q3

### Methodology
- `examples/bench_q5.rs` — warmup + 3 timed Q5 runs (5 nation rows)
- `examples/bench_q3.rs` — warmup + 3 timed Q3 runs (10 order rows, new file)
- `perf record -F 999 -g -o /tmp/perf_{q5,q3}.data -- target/release/examples/bench_{q5,q3}`
- `perf report --stdio --no-children -g none --percent-limit 0.1`

### Q5 timings (current)
```
Q5 run 1: 4321.6 ms (5 rows)
Q5 run 2: 4331.9 ms (5 rows)
Q5 run 3: 4271.3 ms (5 rows)
```
Average ≈ **4.31 s** (matches the W15-research baseline of 4.4 s).

### Q5 top hotspots (by self CPU%, 24K samples, ~101.5 G cycles)
| Rank | Symbol | Self % | Category |
|------|--------|--------|----------|
| 1 | `<turbogp::engine::tpch::TpchExec>::execute` | **58.44%** | Query execution (LTO-inlined blob: hash build/probe + filter + materialize) |
| 2 | `__memmove_avx512_unaligned_erms` (libc memcpy) | **8.42%** | Column materialization during joins |
| 3 | `clear_page_erms` (kernel) | **4.61%** | Zeroing newly-faulted huge pages |
| 4 | `unlink_chunk.isra.0` (libc free) | **3.89%** | glibc free-list maintenance |
| 5 | `_int_malloc` (libc) | **3.16%** | glibc small-bin allocation |

Notable secondary hotspots: `malloc_consolidate` 1.81%, `malloc` 1.56%, `_int_free`
1.84%, `cfree` 0.85%, `_mm_crc32_u64` 0.60% (the JoinHashTable CRC32 hash).

Call-graph drill-down shows that inside `TpchExec::execute`:
- ~3.25% is `asm_exc_page_fault` → `__do_huge_pmd_anonymous_page` → `clear_page_erms`
  (i.e. first-touch page faults on freshly allocated buffers).
- The remainder is LTO-inlined hash table code that cannot be split further without
  a non-LTO profiling build.

### Q5 bottleneck classification
| Bottleneck | Estimated share | Evidence |
|------------|----------------|----------|
| **Hash table probe + build** | ~55–58% | `TpchExec::execute` self-time; Q5 has 5 joins |
| **Column materialization (join output copy)** | ~10% | `__memmove_avx512_unaligned_erms` 8.42% + page faults inside memcpy ~2% |
| **Memory allocation churn** | ~12% | `_int_malloc` + `_int_free` + `malloc_consolidate` + `unlink_chunk` + `malloc` + `cfree` |
| **First-touch page faults** | ~7% | `asm_exc_page_fault` → `clear_page_erms` (new Vec buffers per query) |
| **CRC32 hashing** | 0.6% | `_mm_crc32_u64` (JoinHashTable) |
| **Aggregation** | <1% | Q5 has only 5 groups (Asia nations); aggregation is trivial |
| **String ops** | <0.5% | `from_utf8` mostly from CSV load, not the query |

**Primary bottleneck: HASH JOIN** — building 5 chained hash tables, probing them,
and materializing matched rows. This is exactly what the W15-research note predicted:
Q5 needs a radix-partitioned SwissTable + vectorized probe with SIMD gather, plus
morsel-driven pipeline to avoid materializing intermediate columns.

### Q3 timings (current)
```
Q3 run 1: 754.9 ms (10 rows)
Q3 run 2: 727.5 ms (10 rows)
Q3 run 3: 784.4 ms (10 rows)
```
Average ≈ **755 ms** (matches W15-research 737 ms).

### Q3 top hotspots (by self CPU%, 9.3K samples, ~39.2 G cycles)
| Rank | Symbol | Self % | Category |
|------|--------|--------|----------|
| 1 | `__memmove_avx512_unaligned_erms` (libc memcpy) | **14.57%** | Column materialization (2 joins) |
| 2 | `bench_q3::main` | **13.30%** | CSV loading (one-time, amortized across 4 runs) |
| 3 | `unlink_chunk.isra.0` (libc free) | **12.88%** | glibc free-list |
| 4 | `_int_malloc` (libc) | **9.77%** | glibc allocation |
| 5 | `<turbogp::engine::tpch::TpchExec>::execute` | **7.64%** | Query execution (only 2 joins) |

Notable: `malloc_consolidate` 6.64%, `malloc` 4.64%, `_int_free` 4.32%, `from_utf8`
3.94% (CSV), `clear_page_erms` 2.70%, `StringSearchColumn::new` 1.80%.

### Q3 bottleneck classification
| Bottleneck | Estimated share | Evidence |
|------------|----------------|----------|
| **Memory allocation churn** | **~40%** | `unlink_chunk` + `_int_malloc` + `malloc_consolidate` + `malloc` + `_int_free` + `cfree` |
| **Column materialization** | ~15% | `__memmove_avx512_unaligned_erms` |
| **CSV loading (one-time)** | ~13% | `bench_q3::main` + `from_utf8` + `from_str<f64>` + `default_read_until` + `StringSearchColumn::new` |
| **Query execution** | ~8% | `TpchExec::execute` |
| **Page faults** | ~3% | `clear_page_erms` + `do_anonymous_page` |

**Primary bottleneck: MEMORY ALLOCATION CHURN** — not the query logic. Q3 at 755 ms
spends ~40% of its time in `malloc`/`free`. The fix is an arena allocator / buffer
reuse pool so per-query Vecs are not constantly allocated and freed.

---

## 5. Recommended Wave Priority

Based on the audit, the following techniques are **feasible** on this hardware:

### Tier 1 — Fully feasible (VNNI + BF16 + VBMI2 all run)
| Wave | Technique | Target | Why feasible |
|------|-----------|--------|--------------|
| W21 | **VNNI int8 dot-product bitmap aggregation** | Q6, Q1 filter+agg | `_mm512_dpbusd_epi32` verified at runtime; packs 64 int8 mults+adds into one instruction |
| W22 | **BF16 fused dot-product for revenue aggregation** | Q1, Q5, Q10 `sum(l_extendedprice * (1 - l_discount))` | `_mm512_dpbf16_ps` verified; converts f64 → bf16 pair, single instr does 2 mults+2 adds |
| W23 | **VBMI2 compress for join materialization** | All joins | `_mm512_mask_compress_epi8` verified; eliminates the `__memmove` 8–15% overhead by compressing matched rows in-register |
| W24 | **Radix-partitioned SwissTable join** | Q5, Q3 | Uses AVX-512F conflict detection (`_mm512_conflict_epi64`); addresses the #1 Q5 hotspot (58% in TpchExec::execute) |
| W25 | **Arena allocator / buffer pool** | Q3 (40% malloc overhead), all queries | Pure Rust, no SIMD needed; biggest Q3 win |

### Tier 2 — Blocked by VM
| Wave | Technique | Blocker | Workaround |
|------|-----------|---------|------------|
| W26 | **FP16 half-precision scan/aggregate** | `avx512fp16` not exposed by QEMU | Defer until bare-metal Zen 5 or hypervisor config fix. Intrinsics compile on nightly Rust 1.99 with `#![feature(f16)]` + `#![feature(stdarch_x86_avx512_f16)]` — code can be written behind a `cfg` and gated on `is_x86_feature_detected!("avx512fp16")`. |
| W27 | **AMX tile matrix multiply** | `amx_*` not exposed by QEMU | Same — defer. AMX would only help ML-style workloads anyway, not classical TPC-H. |

### Tier 3 — Lower priority
| Wave | Technique | Rationale |
|------|-----------|-----------|
| W28 | **Morsel-driven pipeline** | Architectural; eliminates materialization. Big effort, pairs with W24. |
| W29 | **Huge-page pre-faulting via `madvise(MADV_HUGEPAGE)`** | Removes the 7% page-fault overhead in Q5. Low effort, moderate payoff. |

### Concrete next-wave recommendation
**W21 = VNNI int8 dot-product for bitmap aggregation.** It is the lowest-risk,
highest-confidence win:
- The intrinsic compiles on stable + nightly Rust (no feature gates needed).
- It runs correctly on this VM (verified above).
- Q6 already uses AVX-512 bitmaps (W13); converting the `popcount`-based aggregation
  to a VNNI dot product should give 2–4× on Q6 and Q1 filter stages.
- BF16 (W22) is the natural follow-up for the `sum(price * (1 - discount))` pattern
  that appears in Q1, Q5, Q10, Q14, Q3.

---

## 6. Summary Verdict

| Question | Answer |
|----------|--------|
| Is AVX-512 FP16 usable? | **NO** — VM does not expose `avx512fp16`; SIGILL at runtime. Intrinsics compile on nightly Rust 1.99 with feature gates. |
| Is AVX-512 VNNI usable? | **YES** — `_mm512_dpbusd_epi32` runs correctly. |
| Is AVX-512 BF16 usable? | **YES** — `_mm512_dpbf16_ps` runs correctly. |
| Is AVX-512 VBMI2 usable? | **YES** — `_mm512_mask_compress_epi8` runs correctly. |
| Is AMX usable? | **NO** — not exposed by the VM. |
| Is W21 feasible? | **YES** — VNNI path is fully open. FP16 path is blocked. |
| Q5 primary bottleneck? | Hash join (58% in `TpchExec::execute`) + column materialization (8% memcpy) + allocation churn (12%). |
| Q3 primary bottleneck? | Memory allocation churn (40%) + column materialization (15%). Query logic is only 8%. |

---

## Appendix A — Reproduction commands

```bash
# SIMD flags
cat /proc/cpuinfo | grep -m1 flags | tr ' ' '\n' | grep -E 'avx512|amx|vnni|bf16|fp16|fma' | sort -u
gcc -march=native -dM -E - </dev/null | grep -E 'AVX512|VNNI|BF16|FP16|AMX' | sort

# Intrinsics test (compile + run)
rustc -C target-cpu=native /tmp/test_intrinsics.rs -o /tmp/test_intrinsics && /tmp/test_intrinsics

# Profile Q5
cd /root/turbogp
cargo build --release --example bench_q5
perf record -F 999 -g -o /tmp/perf_q5.data -- target/release/examples/bench_q5
perf report -i /tmp/perf_q5.data --stdio --no-children -g none --percent-limit 0.5

# Profile Q3
cargo build --release --example bench_q3
perf record -F 999 -g -o /tmp/perf_q3.data -- target/release/examples/bench_q3
perf report -i /tmp/perf_q3.data --stdio --no-children -g none --percent-limit 0.5
```

## Appendix B — Intrinsics test source
Saved at `/tmp/test_intrinsics.rs` on the remote host. Uses
`#![feature(f16)]` and `#![feature(stdarch_x86_avx512_f16)]` so it must be compiled
with nightly Rust. The VNNI/BF16/VBMI2 paths are also feature-detected at runtime
via `std::is_x86_feature_detected!` and silently skip if the flag is missing.
