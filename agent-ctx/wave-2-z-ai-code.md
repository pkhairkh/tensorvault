# Wave 2: Storage Hardening — Work Record

**Task ID:** wave-2
**Agent:** Z.ai Code (single-agent execution)
**Status:** ✅ Complete
**Date:** 2026-07-29

## Summary

Implemented ADR-012 (CRC32C + XOR parity page integrity with single-bit
correction) and ADR-009 (huge-page-backed regions via `mmap(MAP_HUGETLB)`
with THP fallback) and ADR-006 (`ptr::copy_nonoverlapping` for migration).
All DoD gates pass: `cargo build`, `cargo test` (93 = 79 baseline + 14 new),
`cargo clippy -- -D warnings`, `cargo fmt --check`.

## Files Modified

| File | Change |
|------|--------|
| `Cargo.toml` | Added `libc = "0.2"` dependency (mmap/madvise/munmap for ADR-009) |
| `src/storage/page.rs` | Replaced xxh3 with CRC32C (SSE4.2 + scalar fallback); added `compute_parity`; renamed header field `reserved` → `parity`; rewrote `verify_checksum` to use CRC32C; added `verify_and_correct` for single-bit correction; added 10 new tests |
| `src/memory/region.rs` | Replaced `Arc<Mutex<Vec<u8>>>` with `Arc<Mutex<RegionBacking>>` (raw `*mut u8` from mmap with `Drop` calling `munmap`); `Region::allocate` now tries `mmap(MAP_HUGETLB)` → `mmap` + `madvise(MADV_HUGEPAGE)` → `Vec` fallback; `Region::migrate_to` uses `ptr::copy_nonoverlapping`; added 4 new tests |
| `src/executor/scheduler.rs` | `execute_invocation` now locks the backing once and passes `as_slice().as_ptr()` to the kernel — avoids the previous 2 MB `Vec::clone` on every kernel call |

## Files Created

None. All changes are in existing files. No new modules were needed.

## Design Decisions

### Task 2-1 / 2-2 / 2-3: Page integrity (ADR-012)

**CRC32C implementation.**
- `pub fn compute_crc32c(cells: &[u8]) -> u32` is the public entry point.
  On x86-64 it runtime-dispatches to `crc32c_sse42` if
  `is_x86_feature_detected!("sse4.2")` is true; otherwise it falls back to
  `crc32c_scalar`.
- The hardware path uses `_mm_crc32_u64` for 8-byte chunks, then
  `_mm_crc32_u32` / `_mm_crc32_u16` / `_mm_crc32_u8` for the tail so the
  result matches the scalar reference bit-for-bit on any input length.
- The scalar fallback is a branchless bit-by-bit loop using the reversed
  Castagnoli polynomial `0x82F63B78` (`CRC32C_POLY_REVERSED`). The trick
  `mask = 0u32.wrapping_sub(crc & 1)` produces `0xFFFFFFFF` when the low
  bit is set and `0` otherwise, allowing `(crc >> 1) ^ (poly & mask)` with
  no branch — important on CPUs without SSE4.2 (typically low-end SKUs
  where every cycle counts).
- Initial value `0xFFFFFFFF` + final XOR with `0xFFFFFFFF` matches the
  iSCSI / ext4 / btrfs / NVMe T10 PI convention. Verified against the
  canonical CRC32C check value: `compute_crc32c(b"123456789") == 0xE3069283`
  (test `crc32c_known_check_value`).

**Header layout.**
The `checksum: u64` field is preserved for ABI compatibility, but only its
low 32 bits are used. The high 32 bits are always zero. The 64-bit storage
wastes 4 bytes per page (0.1% overhead) but avoids touching the `Pod` /
`Zeroable` derive layout and keeps `PageHeader::SIZE` at exactly 64 bytes.
Future waves can tighten this to `checksum: u32` + a 4-byte pad if needed.

**Parity scheme.**
`compute_parity` XORs all 8-byte words position-wise into a single `u64`.
If a single bit flips in any word, exactly that bit position in the parity
flips — giving us a *bit-position-within-word* syndrome. We do NOT learn
*which* word was flipped from the parity alone; the ADR's wording
("localizes the error to a single word") is slightly imprecise. The actual
correction algorithm in `verify_and_correct`:

1. If `verify_checksum()` passes → `Ok(false)` (no error).
2. Compute `syndrome = stored_parity ^ computed_parity`.
3. If `syndrome.count_ones() != 1` → `Err(uncorrectable)` (multi-bit
   corruption, or the parity/CRC fields themselves are corrupt).
4. The single set bit gives the bit position within a word
   (`trailing_zeros()`). Walk every 8-byte word in the page, flip that
   bit, and accept the correction only if `verify_checksum()` then passes.
5. If no word flip repairs the CRC → `Err(uncorrectable)` (the syndrome
   was a single bit but the CRC field itself was the corrupt byte).

This is O(words) per correction (~504 word-flips + CRC recomputations in
the worst case), but corrections are extraordinarily rare (silent data
corruption rates are ~5%/drive/year per Google's study), so the cost is
irrelevant. The common path (`verify_checksum()` passes) is O(1).

### Task 2-4: Huge-page-backed regions (ADR-009)

**The `RegionBacking` type.**
- Owns a `*mut u8` of length `REGION_SIZE` (2 MB) plus a `BackingKind` tag
  that records whether it came from `mmap(MAP_HUGETLB)`, `mmap` + THP
  advise, or `Vec`.
- `unsafe impl Send + Sync` is required because raw pointers are `!Send`
  by default. Sound because all access goes through `as_slice` / `as_mut_slice`
  which borrow `&self` / `&mut self`, and the surrounding `Region` always
  wraps the backing in a `Mutex`.
- `Drop` calls `munmap` (Linux mmap path) or reconstructs the `Vec` via
  `Vec::from_raw_parts` (the `vec![]`-backed path used on non-Linux or when
  both mmap attempts fail).

**Allocation strategy.**
1. `mmap(MAP_HUGETLB | MAP_PRIVATE | MAP_ANONYMOUS)` — best case, gives a
   guaranteed 2 MB huge page. Length is forced to `REGION_SIZE` because the
   kernel requires a multiple of the huge-page size.
2. On failure (returns `MAP_FAILED`), retry with `mmap` (no `MAP_HUGETLB`)
   and `madvise(MADV_HUGEPAGE)` — relies on `khugepaged` to coalesce later.
3. If both mmap calls fail (very unusual — `ENOMEM`), fall back to
   `Vec::with_capacity(REGION_SIZE)` so the engine still runs.

`MAP_HUGETLB` is defined locally in `mmap_constants` because the `libc`
crate's bindings for it are inconsistent across toolchains (it's part of
the Linux UAPI but not always exposed). Value `0x040_000` is stable across
architectures.

**Non-Linux targets.** The whole `try_mmap_linux` function is
`#[cfg(target_os = "linux")]`, so macOS / Windows / BSD fall straight to
the `Vec` fallback. This keeps the engine testable in CI on any platform.

### Task 2-5: `migrate_to` using `ptr::copy_nonoverlapping` (ADR-006)

The new `migrate_to` allocates a fresh `RegionBacking` (which itself may
use mmap) and copies the source bytes via
`std::ptr::copy_nonoverlapping`. On x86-64 with ERMS (Fast Short REP MOV,
all CPUs since Ivy Bridge 2012), LLVM lowers this to `REP MOVSB` — the
~1 byte/cycle, hardware-prefetched bulk copy that ADR-006 benchmarks as
10–20% faster than hand-written AVX-512 for buffers > 128 B.

The previous implementation called `Vec::clone`, which is also a `memcpy`
under the hood — but on a `Vec` allocation rather than a mmap'd region.
The new version preserves the huge-page backing in the migrated region.

### Scheduler change (downstream impact)

The scheduler's `execute_invocation` previously did:
```rust
let data = region.data.lock().clone();  // 2 MB Vec clone!
```
This was a per-kernel-invocation 2 MB allocation + copy — catastrophically
wasteful. Replaced with a single `data.lock()` whose guard lives across
the `kernel.execute` call, passing `data.as_slice().as_ptr()` directly.
Net effect: every kernel invocation gets ~2 MB cheaper. The `Region` API
change required this; without it, `RegionBacking: Clone` would have been
needed, which would itself have been a 2 MB copy.

## Test Results

```
cargo fmt --check:        clean (exit 0)
cargo build:              clean (exit 0)
cargo build --release:    clean (exit 0)
cargo clippy -- -D warnings:    clean (exit 0)  [DoD form, no --all-targets]
cargo test:
  lib unit tests:   86 passed  (was 72, +14 new)
  integration tests: 7 passed  (unchanged)
  total:            93 passed  (was 79, +14 new)
```

New tests in `src/storage/page.rs` (10):
1. `page_crc32c_roundtrip` — write cells, compute CRC32C, store, verify.
2. `page_crc32c_detects_single_bit_corruption` — single bit flip → CRC mismatch.
3. `page_verify_and_correct_fixes_single_bit_error` — single-bit error is corrected in place.
4. `page_verify_and_correct_fails_on_double_bit_error` — two-bit error returns `Err`.
5. `page_verify_and_correct_returns_false_when_clean` — clean page returns `Ok(false)`.
6. `page_parity_correct_for_known_values` — known XOR over three explicit words.
7. `crc32c_empty_input_is_zero` — `0xFFFFFFFF ^ 0xFFFFFFFF == 0`.
8. `crc32c_hardware_matches_scalar` — SSE4.2 path == scalar path on 4032 bytes.
9. `crc32c_known_check_value` — `compute_crc32c(b"123456789") == 0xE3069283` (Castagnoli check value).
10. `parity_of_repeated_word_cancels` — 504 copies of the same word XOR to 0 (504 is even).

New tests in `src/memory/region.rs` (4):
1. `region_allocate_succeeds` — 4 back-to-back allocations; verifies size, zeroing, and that the very first/last bytes of the mapping are writable.
2. `region_from_bytes_preserves_data` — full 2 MB roundtrip with a non-trivial pattern.
3. `region_migrate_does_not_mutate_source` — the source is unchanged after migration (catches `ptr::copy` vs `ptr::copy_nonoverlapping` regressions).
4. `region_backing_reports_huge_page_status` — `is_huge_page()` accessor returns a consistent value matching the `BackingKind`.

## DoD Verification

- [x] `cargo build` passes
- [x] `cargo test` passes (93 = 79 existing + 14 new)
- [x] `cargo clippy -- -D warnings` passes
- [x] `cargo fmt --check` passes
- [x] CRC32C detects corruption (test 2)
- [x] Single-bit errors are correctable (test 3)
- [x] Double-bit errors return `Err` (test 4)
- [x] Huge pages allocate on Linux (or fall back gracefully — `region_allocate_succeeds` passes either way)
- [x] All unsafe blocks have `// SAFETY:` comments
- [x] `PageHeader.reserved` renamed to `parity` (verified by `git grep reserved src/` — no matches)

## Notes for Downstream Waves

- **`RegionBacking` is `Send + Sync`** (via explicit `unsafe impl`). Future
  waves that want to put a region behind a different synchronization
  primitive (e.g. `RwLock` instead of `Mutex`) can do so without changes.
- **`RegionBacking::is_huge_page()`** is exposed for telemetry. Wave 5
  (Memory Manager) should report the ratio of huge-page-backed regions as
  a health metric — if it's < 100% in production, huge-page exhaustion is
  happening and the operator should bump `vm.nr_hugepages`.
- **`compute_crc32c` returns `u32`**, not `u64`. Wave 8 (WAL + persistence)
  should use it for SSTable block checksums too — the function is
  general-purpose and not tied to the 4 KB page size.
- **`Page::verify_and_correct` is `&mut self`.** Wave 8 must call it on
  every page read from disk (or at least on every page read from a
  tier that doesn't already do ECC, like NVMe without T10 PI). The
  returned `Result<bool, String>` should be logged: `Ok(true)` is a
  silently-corrected single-bit flip (worrying but not fatal), `Err` is
  unrecoverable corruption (page must be re-fetched from a replica).
- **The `let _ = unsafe { madvise(...) };` discards the return value.**
  `madvise(MADV_HUGEPAGE)` is a hint — failure is non-fatal and the
  kernel will simply not coalesce. If a future wave wants strict huge-page
  behavior, it should check `is_huge_page()` after allocation and fall
  back to a different region.
- **The `xxhash-rust` dependency is still in `Cargo.toml`** because
  `src/storage/wal.rs` uses xxh3 for WAL record checksums. ADR-012
  explicitly says xxh3 is "kept for non-page data". Wave 8 may reconsider
  this if it wants a unified checksum story.
- **Pre-existing clippy warnings exist in `src/kernel/hash.rs` and
  `src/executor/scheduler.rs` test code** (`useless_vec`, `manual_range_contains`)
  that are visible only with `cargo clippy --all-targets`. The Wave 1
  work record used the DoD form (`cargo clippy -- -D warnings`, no
  `--all-targets`) and so did not catch them. They are out of scope for
  Wave 2 but Wave 3 (Kernel Expansion) should clean them up while it's
  already in those files.
