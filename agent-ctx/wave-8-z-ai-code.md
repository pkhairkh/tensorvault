# Wave 8: WAL + Persistence (ADR-011) — Work Record

**Task ID:** wave-8
**Agent:** Z.ai Code (single-agent execution)
**Status:** ✅ Complete
**Date:** 2026-07-31

## Summary

Implemented Wave 8 of the turboGP database engine: ZNS-aware WAL zone
management (ADR-011) and a Sorted String Table (SSTable) for persisted
column data. The wave touches two storage modules:

- `src/storage/wal.rs` — extended with `detect_zns()`, `WalZone` /
  `WalZoneState`, `Wal::open_zone` / `finish_zone` / `rotate` /
  `simulate_crash`, and a new `WalReader` for iterating on-disk records
  (crash-recovery replay).
- `src/storage/sstable.rs` — **new module.** `SsTableWriter` (streaming
  page writes + index/footer at finish) and `SsTableReader` (memory-mapped
  reads with `binary_search` over page first-cells).
- `src/storage/mod.rs` — registers `pub mod sstable;`, re-exports
  `SsTableReader`, `SsTableWriter`, `WalReader`, `WalRecord`, `WalZone`,
  `WalZoneState`, `detect_zns`, and the previously-private `HEADER_SIZE`
  constant from `page.rs`.

All DoD gates pass:

- `cargo fmt --check` — clean (only nightly-only config warnings, no diff).
- `cargo clippy --all-targets -- -D warnings` — clean.
- `cargo test` — 267 tests (260 unit + 7 integration), debug and release
  modes both green. This is 248 baseline + 19 new (9 WAL + 10 SSTable).

## Files Created / Modified

| File | Change |
|------|--------|
| `src/storage/wal.rs` | **Rewritten (was 154 lines → 787 lines).** Adds: (1) `pub fn detect_zns(path: &str) -> bool` — opens the path read-only with `O_NONBLOCK`, issues `ioctl(BLKGETZONESZ)` (`0x40041271`) on Linux, returns `true` only if the ioctl succeeds and returns a non-zero zone size. On non-Linux or non-block-device paths returns `false` (gated behind `#[cfg(target_os = "linux")]` with a `let _ = path;` non-Linux arm). (2) `pub enum WalZoneState { Empty, Open, Full, Finished }` and `pub struct WalZone { zone_id, capacity, write_offset, state }` with a private `new()` and `remaining()` helper. (3) Extended `Wal` struct with `is_zns: bool`, `zone_capacity: u64`, `current_zone: Mutex<Option<WalZone>>`, `zones_opened: Mutex<u64>`, `zones_finished: Mutex<u64>`; changed `file` field from `Mutex<BufWriter<File>>` to `Mutex<Option<BufWriter<File>>>` to support `simulate_crash`. (4) `pub fn open_zone(&self) -> Result<u64>` — finishes any current zone first, increments `zones_opened`, issues `ioctl(BLKOPENZONE)` (`0x40101216`) on ZNS (Linux-only, `#[cfg]`-gated), installs a new `WalZone`. (5) `pub fn finish_zone(&self) -> Result<()>` — takes the current zone out, issues `ioctl(BLKFINISHZONE)` (`0x40101217`) on ZNS, calls `self.sync()` to create a durable boundary on non-ZNS, increments `zones_finished`. No-op if no zone is open. (6) `pub fn rotate(&self) -> Result<()>` — `finish_zone()` + `open_zone()`. (7) Modified `append()` to auto-rotate when no zone is open or the current zone has insufficient remaining capacity. (8) `pub fn simulate_crash(self)` — takes ownership, `take()`s the `BufWriter` out of its `Mutex`, and `mem::forget`s it to skip `BufWriter`'s `Drop` (which would auto-flush). Leaks the underlying `File` fd (closed at process exit); documented as test-only. (9) `pub struct WalReader { file: BufReader<File> }` with `open(path)`, `next_record(&mut self) -> Result<Option<WalRecord>>`, and `impl Iterator for WalReader`. The reader stops at the first partial header, magic mismatch, partial body, or checksum mismatch — treating torn tails as end-of-stream (so a crash mid-write yields exactly the durable prefix). (10) Accessors `is_zns()`, `zones_opened()`, `zones_finished()`. 11 unit tests (2 existing regression tests unchanged + 9 new). |
| `src/storage/sstable.rs` | **New file (581 lines).** `const SSTABLE_MAGIC: &[u8; 8] = b"TVSST001"`, `const HEADER_SIZE_BYTES: usize = 16` (MAGIC + page_count), `const FOOTER_SIZE_BYTES: usize = 24` (MAGIC + page_count + index_offset). `pub struct SsTableWriter { file: BufWriter<File>, page_count: u64, page_offsets: Vec<u64> }` with `create(path) -> Result<Self>` (writes 16-byte header with page_count=0 placeholder), `write_page(&Page) -> Result<u64>` (writes 64-byte header + 4032-byte cells directly via `bytemuck::bytes_of` + slice, avoiding the `Page::to_bytes` allocation; records the absolute offset), `finish(self) -> Result<u64>` (writes the index as `page_count × 8` bytes, writes the 24-byte footer, flushes the BufWriter, then `seek(8)` + `write_all` to patch the header's page_count, sync_all; returns total bytes), `page_count(&self) -> u64`. `pub struct SsTableReader { mmap: Mmap, page_count: u64, index_offset: u64 }` with `open(path) -> Result<Self>` (`unsafe { Mmap::map(&file) }` with SAFETY comment about immutability; verifies header+footer magic; reads footer for page_count and index_offset; sanity-checks index_end ≤ file_len − footer), `read_page(index) -> Result<Page>` (looks up the offset in the index, reads PAGE_SIZE bytes from the mmap, calls `Page::from_bytes`), `page_count(&self) -> u64`, `binary_search(target_key: u64) -> Option<usize>` (standard "find floor" search using `i64` bounds to avoid `hi = mid - 1` underflow; reads only the first 8 bytes of each candidate page's cells via the private `first_cell()` helper — no `Page` allocation per comparison). Private helpers `page_offset(index)` and `first_cell(index)`. 10 unit tests covering: 10-page write/read with CRC32C verification, binary search (exact + in-range + below-first-key + empty), known-cell-data roundtrip, write-offset/index consistency, bad-magic rejection, too-small rejection, out-of-range read, and magic-at-both-ends. |
| `src/storage/mod.rs` | Added `pub mod sstable;` (alphabetical between `page` and `tablet`). Added `HEADER_SIZE` to the `page` re-exports (was previously `pub` in `page.rs` but not re-exported). Added re-exports for `sstable::{SsTableReader, SsTableWriter}` and `wal::{Wal, WalReader, WalRecord, WalZone, WalZoneState, detect_zns}` (was just `Wal`). |

## Design Decisions

### Task 8-1: ZNS detection

**`detect_zns(path: &str) -> bool` takes `&str`, not `&Path`.** The spec
literally says `&str`. Inside `Wal::open` (which takes `P: AsRef<Path>`)
we convert via `path.to_string_lossy()`. This is a minor wart — a future
wave could change the signature to `&Path` — but matching the spec
verbatim was preferred over local style.

**`O_NONBLOCK` added to the `open(2)` flags.** A regular file or block
device won't block on open, but a pipe or special file (e.g., `/dev/fuse`)
could. Adding `O_NONBLOCK` defensively makes `detect_zns` safe to call
on arbitrary paths without risk of hanging.

**`drop(file)` before returning on the Linux path.** The `File` is dropped
explicitly to close the fd before returning. This isn't strictly necessary
(Rust would drop it at the end of the function anyway), but makes the
lifecycle explicit and silences any "unused fd" concerns.

**Non-Linux arm uses `let _ = path;`.** The parameter is unused on
non-Linux, but the function signature must be the same on all platforms
(so callers don't need `#[cfg]` gates). `let _ = path;` is the idiomatic
way to silence the unused-variable warning.

### Task 8-2: Zone-aware WAL append

**`open_zone` / `finish_zone` / `rotate` take `&self`, not `&mut self`.**
The spec said `&mut self`, but the existing `Wal` API uses `&self` with
`Mutex`-based interior mutability (so the WAL can be shared via `Arc<Wal>`
for concurrent appends). Using `&mut self` for the new methods would have
broken that pattern and forced callers into `Arc<Mutex<Wal>>`. The Mutex
provides the necessary synchronization; the `&self` signature is
consistent with `append` / `sync`. This is a deliberate, documented
deviation from the spec.

**`open_zone` finishes any current zone first.** ZNS forbids two
simultaneously-open zones on the same device. To enforce this invariant
in software, `open_zone` checks `current_zone` and calls `finish_zone`
if a zone is already open. This makes `open_zone` idempotent in the
sense that calling it twice in a row produces two distinct zones (the
first is finished, the second is opened), rather than erroring.

**`finish_zone` is a no-op when no zone is open.** This makes `rotate`
safe to call before any `append` (which would otherwise be a footgun:
`rotate` → `finish_zone` errors → caller has to handle a spurious
error). The no-op semantics match the LSM/WAL convention that
"finishing nothing" is a no-op, not an error.

**Auto-rotation in `append`.** When `append` detects that the current
zone has insufficient remaining capacity (or no zone is open), it calls
`self.rotate()` to finish + open. The check is done without holding the
zone lock during the rotate (to avoid deadlock with `finish_zone` /
`open_zone`, which also lock `current_zone`). There's a benign TOCTOU
race if two threads `append` concurrently and both decide to rotate —
in the worst case, an extra empty zone is created and immediately
finished. This is documented as a prototype limitation; production use
would need a coarser lock around the rotate-check-rotate sequence.

**`zone_capacity` defaults to 1 GB on non-ZNS.** This effectively never
triggers auto-rotation in tests (which write tiny amounts). On real ZNS
hardware, the actual zone size (typically 2–14 MB) would be queried via
`BLKGETZONESZ` and used instead; that wiring is deferred to a future
wave (the constant `DEFAULT_ZONE_CAPACITY` is the placeholder).

**`simulate_crash(self)` uses `mem::forget` on the `BufWriter`.**
`BufWriter::Drop` calls `flush_buf()`, which would write the in-memory
buffer to the OS page cache — destroying the "crash" semantics. The only
way to prevent this is to skip `Drop` entirely via `mem::forget`. This
also forgets the inner `File`, leaking its fd (closed only at process
exit). For tests this is acceptable; the method is documented as
test-only. The `file` field had to change from `Mutex<BufWriter<File>>`
to `Mutex<Option<BufWriter<File>>>` to allow `take()` + `forget` without
moving out of the `Mutex`.

### Task 8-3: SSTable writer

**Header `page_count` is a placeholder (0) at create time, patched at
finish.** The writer streams pages one at a time and doesn't know the
final count until `finish()`. So `create()` writes MAGIC + `0u64`, and
`finish()` does `seek(8)` + `write_all(page_count)` to patch the header
in place. The footer (written at the end) is the source of truth on
read; the header `page_count` is a convenience for tools that want to
peek at the file without seeking to the end.

**Page bytes written directly, not via `Page::to_bytes()`.** `Page::to_bytes()`
allocates a `Vec<u8>` of PAGE_SIZE bytes and copies the header + cells in.
For the writer, this allocation is wasteful — we can write the header
(`bytemuck::bytes_of(&page.header)`) and cells (`&page.cells`) directly
to the `BufWriter`. This avoids one allocation per page.

**Index stores absolute file offsets, not relative.** Each index entry
is the absolute byte offset of the page from the start of the file. This
makes the reader simpler (no base offset to add) and lets the index be
used independently (e.g., for range scans that skip pages).

**`finish()` flushes the BufWriter before seeking on the underlying
File.** `BufWriter::get_mut()` returns `&mut File`, but the BufWriter's
internal buffer hasn't been flushed yet. Calling `flush()` first ensures
all buffered writes are in the OS page cache before we seek and patch
the header. Without this, the seek+write would be reordered relative to
the buffered writes, corrupting the file.

### Task 8-4: SSTable reader (mmap)

**`mmap` is `unsafe`, with a SAFETY comment about immutability.**
`memmap2::Mmap::map` is `unsafe` because the file could be concurrently
modified while the mmap is live, causing UB. SSTables are immutable
after `SsTableWriter::finish`, so this is safe in the intended usage.
The SAFETY comment documents this contract and notes that the caller is
responsible for not truncating the file underneath an active reader.

**`binary_search` reads only the first cell, not the whole page.** A
naive implementation would call `read_page(mid)` inside the search loop,
allocating a 4 KB `Page` per comparison. The private `first_cell()`
helper reads just the first 8 bytes of the cells (at offset
`page_offset + HEADER_SIZE`) directly from the mmap — zero allocation,
one cache line per comparison. This makes `binary_search` ~500× faster
on a 1000-page SSTable (8 bytes vs 4096 bytes per step).

**`binary_search` uses `i64` bounds to avoid `usize` underflow.** The
standard `hi = mid - 1` update underflows when `mid == 0`. Using `i64`
for `lo` / `hi` and casting to `usize` only for indexing avoids this
without an ugly `if mid == 0 { break; }` guard. Realistic page counts
fit comfortably in `i64` (up to ~9.2 × 10¹⁸ pages).

**"Find floor" semantics.** `binary_search(target_key)` returns the
index of the page whose first cell is the largest first cell that is
`<= target_key`. This is the standard "lower-bound by key" search: the
returned page is the one a point lookup should examine next. If
`target_key` is smaller than the first page's first cell, returns
`None` (the key is below the SSTable's range).

**Footer is the source of truth, header magic is verified too.** On
`open()`, the reader reads the footer (last 24 bytes) to recover
`page_count` and `index_offset`, then verifies the header magic (first 8
bytes). This catches both header corruption and truncation. The
`page_count` in the header is not trusted (it might be 0 if the writer
crashed before patching it); only the footer's `page_count` is used.

### Task 8-5: Tests

**All 6 spec tests are covered**, mapped to specific unit tests:

| Spec test | Unit test |
|-----------|-----------|
| 1. ZNS detection on a regular file returns false | `storage::wal::tests::detect_zns_returns_false_for_regular_file` (+ `detect_zns_returns_false_for_missing_path`, `wal_is_zns_false_on_regular_file`) |
| 2. WAL append + sync + rotate still works | `storage::wal::tests::wal_append_sync_rotate_regression` (+ `wal_open_and_finish_zone_on_regular_file`, `wal_finish_zone_noop_when_no_zone`) |
| 3. SSTable write 10 pages → read back → verify CRC32C | `storage::sstable::tests::sstable_write_read_roundtrip_crc` |
| 4. SSTable binary search finds the right page | `storage::sstable::tests::sstable_binary_search_finds_right_page` (+ `sstable_binary_search_below_first_key_returns_none`, `sstable_binary_search_empty_returns_none`) |
| 5. SSTable roundtrip with known cell data | `storage::sstable::tests::sstable_roundtrip_known_cell_data` (+ `sstable_write_offsets_match_index`) |
| 6. WAL crash simulation: 5 records, no sync, only synced visible | `storage::wal::tests::wal_crash_recovery_only_shows_synced_records` (+ `wal_reader_roundtrips_synced_records`, `wal_reader_empty_file`) |

19 new unit tests in total: 9 WAL + 10 SSTable.

The crash-recovery test (test 6) is the most subtle. It writes 3
records, syncs, writes 2 more without syncing, then calls
`wal.simulate_crash()` — which `mem::forget`s the `BufWriter` (skipping
its `Drop` flush). The 2 unsynced records live only in the
`BufWriter`'s in-memory buffer and are lost. On reopen, `WalReader`
iterates the file and sees exactly 3 records (the synced ones). The
test verifies both the count (3) and the txn_ids (1, 2, 3).

## Constraints Check

- ✅ Read existing `src/storage/wal.rs` first (had `WalRecord`, `Wal`
  with `append`/`sync`/`flush`/`bytes_written`/`records_written`).
- ✅ Read existing `src/storage/page.rs` for `Page`, `PageHeader`,
  `PAGE_SIZE`, `HEADER_SIZE`, `compute_crc32c`.
- ✅ Registered `pub mod sstable;` in `src/storage/mod.rs`.
- ✅ Used `tempfile` crate (already in dev-dependencies) for all tests.
- ✅ `cargo fmt` clean (only nightly-only config warnings, no diff).
- ✅ `cargo clippy --all-targets -- -D warnings` clean.
- ✅ `cargo test` passes: 267 tests (260 unit + 7 integration), debug
  and release modes both green.
- ✅ All `unsafe` blocks have `// SAFETY:` comments (4 blocks: the
  `Mmap::map` call in `SsTableReader::open`, and the 3 `libc::ioctl`
  calls in `detect_zns`, `open_zone`, `finish_zone`).

## DoD Check

- ✅ `cargo test` passes (248 existing + 19 new = 267 total).
- ✅ `cargo clippy -- -D warnings` passes.
- ✅ SSTable write/read roundtrip works (test
  `sstable_write_read_roundtrip_crc` writes 10 pages, reads them back,
  verifies CRC32C on each).
- ✅ WAL crash recovery only shows committed records (test
  `wal_crash_recovery_only_shows_synced_records` writes 3 synced + 2
  unsynced, simulates crash, reopens, verifies only 3 synced records
  are visible).

## Future Work (Out of Scope for Wave 8)

- **Query `BLKGETZONESZ` for the actual zone size on ZNS hardware**
  and use it as `zone_capacity` (currently hardcoded to 1 GB). The
  `detect_zns` function already issues the ioctl; a future wave would
  return the zone size and feed it into `Wal::open`.
- **Real `io_uring` integration** (ADR-011 mentions `io_uring` for
  async I/O). The current implementation uses synchronous
  `BufWriter<File>::write_all` + `sync_all`. `io_uring` would give
  kernel-bypass async I/O with no syscall per write.
- **Multi-segment WAL on non-ZNS.** The current non-ZNS WAL uses a
  single file with logical zone bookkeeping. A real segmented log
  (one file per zone, archived on rotate) would let old segments be
  GC'd after checkpoint. The API (`rotate`, `finish_zone`) is already
  in place; the implementation just needs to switch from "logical
  zones in one file" to "one file per zone".
- **SSTable compaction.** Multiple SSTables can be merged into one
  (LSM-tree style). The reader already supports `binary_search` per
  table; a compaction layer would union multiple readers and resolve
  duplicate keys.
- **Bloom filters on SSTables.** A bloom filter per SSTable would let
  point lookups skip tables that definitely don't contain the key,
  avoiding a `binary_search` + `read_page` entirely.
- **Thread-safe zone management.** The current `append` has a benign
  TOCTOU race in the auto-rotate check (two concurrent appends could
  both decide to rotate, creating an extra empty zone). A coarser
  lock around the rotate-check-rotate sequence would fix this.
