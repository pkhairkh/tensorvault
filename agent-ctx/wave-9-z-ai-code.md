# Wave 9: Protocol Coordinator (ADR-013, ADR-014) — Work Record

**Task ID:** wave-9
**Agent:** Z.ai Code (single-agent execution)
**Status:** ✅ Complete
**Date:** 2026-07-31

## Summary

Implemented Wave 9 of the turboGP database engine: a Hybrid Logical Clock
(ADR-014), and real `CxlCoordinator` / `RaftCoordinator` implementations
(ADR-013) with OQ-02 (CXL→WAL fallback) and OQ-04 (Raft leader/quorum
simulation) behavior. The wave touches three protocol modules:

- `src/protocol/clock.rs` — **new module.** `HlcTimestamp` (a totally-ordered
  `(physical_ns, logical)` pair) and `HlcClock` (a single-threaded HLC with
  `now()` / `observe()` / `last()` / `physical_now()`).
- `src/protocol/cxl.rs` — **rewritten.** Replaced the stub that returned a
  hardcoded `250u64` latency with a real coordinator that takes an
  `Option<Arc<Wal>>`, detects CXL via `/sys/bus/cxl/`, falls back to a WAL
  append+sync on non-CXL hardware, and returns `Result<HlcTimestamp>`.
- `src/protocol/raft.rs` — **rewritten.** Replaced the stub that returned a
  hardcoded `10_000u64` with a real coordinator that takes an
  `Option<Arc<Wal>>`, enforces the leader invariant (`Error::Protocol` on
  follower commits), appends to the local WAL on the leader path, simulates
  quorum replication (logged via `tracing::debug!`), and returns
  `Result<HlcTimestamp>`.
- `src/protocol/mod.rs` — registers `pub mod clock;` and re-exports
  `HlcClock` / `HlcTimestamp` alongside the existing `CxlCoordinator` /
  `RaftCoordinator` re-exports.
- `examples/smoke.rs` — updated to the new API: `CxlCoordinator::new(None)`,
  `RaftCoordinator::new_as_leader(3, 0, None)`, and the new
  `commit() -> Result<HlcTimestamp>` return type.

All DoD gates pass:

- `cargo fmt --check` — clean (only nightly-only config warnings, no diff).
- `cargo clippy --all-targets -- -D warnings` — clean, debug and release.
- `cargo test` — 289 tests (282 unit + 7 integration), debug and release
  modes both green. This is 267 baseline + 22 new (9 HLC + 6 CXL + 7 Raft).
- All 10 spec tests covered (see mapping below).
- No `unsafe` blocks added (none were needed — the WAL fallback uses safe
  `Wal::append` / `Wal::sync`).

## Files Created / Modified

| File | Change |
|------|--------|
| `src/protocol/clock.rs` | **New file (289 lines).** `pub struct HlcTimestamp { physical_ns: u64, logical: u64 }` deriving `Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash` (lexicographic `(physical_ns, logical)` order gives the total order). `pub const HlcTimestamp::ZERO` sentinel. `impl Display` (renders `"phys_ns+logical"`) and `debug_string()` (renders `"phys.phys_ns+logical"`). `pub struct HlcClock { last: HlcTimestamp }` with: (1) `pub fn new() -> Self` (starts at `ZERO`); (2) `pub fn now(&mut self) -> HlcTimestamp` — if `physical_now() > last.physical_ns`, reset to `(pt, 0)`; else `(last.physical_ns, last.logical + 1)`; (3) `pub fn observe(&mut self, other: &HlcTimestamp) -> HlcTimestamp` — ADR-014 algorithm: if `other.physical_ns > last.physical_ns`, adopt `(other.physical_ns, other.logical + 1)`; if equal, `(last.physical_ns, max(last.logical, other.logical) + 1)`; if less, `(last.physical_ns, last.logical + 1)`; (4) `fn physical_now() -> u64` — `SystemTime::now().duration_since(UNIX_EPOCH)` in nanoseconds, returning 0 on pre-epoch error; (5) `pub fn last(&self) -> HlcTimestamp` — non-advancing peek for tests. `impl Default for HlcClock` (= `new()`). 9 unit tests. |
| `src/protocol/cxl.rs` | **Rewritten (was 63 lines → 233 lines).** `pub struct CxlCoordinator { available: bool, clock: HlcClock, wal: Option<Arc<Wal>> }`. `pub fn new(wal: Option<Arc<Wal>>) -> Self` — detects CXL via `crate::memory::numa::cxl_available()` (which checks for `/sys/bus/cxl`). `pub fn is_available(&self) -> bool`. `pub fn commit_tier(&self) -> MemoryTier` — returns `Cxl` / `Nvme` / `Ddr5` depending on availability and WAL presence (kept from the Wave 1 stub for backward compat). `pub fn commit(&mut self, txn_id: u64) -> Result<HlcTimestamp>` — issues `clock.now()` first (linearization point), then: if `available`, return the timestamp (simulated CXL.cache flush + MFENCE — no-op in this prototype); else, if `wal.is_some()`, append a `WalRecord { txn_id, record_type: 0 (commit), payload: ts.physical_ns.to_le_bytes() }`, sync, return the timestamp; else (no WAL, no CXL), just return the timestamp (in-memory mode). `const WAL_RECORD_TYPE_COMMIT: u8 = 0`. `impl Default for CxlCoordinator` (= `new(None)`). 7 unit tests (6 new + 1 legacy regression kept from Wave 1). |
| `src/protocol/raft.rs` | **Rewritten (was 53 lines → 318 lines).** `pub struct RaftCoordinator { pub cluster_size: usize, pub node_id: u64, clock: HlcClock, wal: Option<Arc<Wal>>, is_leader: bool }`. `pub fn new(cluster_size, node_id, wal) -> Self` — starts as follower (`is_leader = false`). `pub fn new_as_leader(cluster_size, node_id, wal) -> Self` — convenience constructor that starts as leader (sets `is_leader = true` after `new`). `pub fn quorum(&self) -> usize` — `cluster_size / 2 + 1`. `pub fn is_leader(&self) -> bool`. `pub fn become_leader(&mut self)`. `pub fn become_follower(&mut self)`. `pub fn commit(&mut self, txn_id: u64) -> Result<HlcTimestamp>` — if `!is_leader`, return `Error::Protocol("node {node_id} is not the leader; cannot commit txn {txn_id}")`; else issue `clock.now()`, append a commit record to the WAL (if present), `tracing::debug!` the simulated quorum replication, sync the WAL (if present), return the timestamp. `const WAL_RECORD_TYPE_COMMIT: u8 = 0`. 8 unit tests (the original `raft_quorum_is_majority` was kept and extended with even-cluster-size edge cases; 7 new tests). |
| `src/protocol/mod.rs` | Added `pub mod clock;` (alphabetical, before `cxl`). Added `pub use clock::{HlcClock, HlcTimestamp};` to the re-exports. Updated the module-level doc comment to mention HLC timestamps. |
| `examples/smoke.rs` | Updated step 3 (`CxlCoordinator::new(None)`, `cxl.commit(0).unwrap()`, prints the timestamp) and step 4 (`RaftCoordinator::new_as_leader(3, 0, None)`, `raft.commit(0).unwrap()`, prints the timestamp and `raft.is_leader()`). Both `cxl` and `raft` are now `let mut` (because `commit` takes `&mut self` to advance the HLC clock). |

## Design Decisions

### Task 9-1: HLC clock

**`HlcTimestamp` derives `Hash`.** The spec only requires `Debug, Clone, Copy,
PartialEq, Eq, PartialOrd, Ord`, but adding `Hash` is free (the fields are
both `u64`) and makes the type usable as a `HashMap` key — useful for
snapshot-isolation read-sets keyed by commit timestamp.

**`HlcTimestamp::ZERO` is exposed as a public constant.** Real timestamps
produced by `HlcClock::now` always have `physical_ns >= 1` (since 1970 was
56+ years ago, the nanosecond count is ~1.78e18). The zero timestamp is
therefore strictly less than any real one, which makes it a useful sentinel
for "uninitialized" or "before all real timestamps" in snapshot isolation.

**`now()` uses `>` not `>=` for the physical-time-advance check.** If the
loop body is faster than 1 nanosecond (which it is, on modern hardware),
`physical_now()` returns the same value on consecutive calls. Using `>`
means: "if physical time strictly advanced, reset logical to 0; else,
increment logical". This preserves strict monotonicity even when the
clock resolution is coarser than the call rate.

**`observe()` follows the spec literally — it does NOT consult
`physical_now()`.** The standard HLC paper (Kulkarni et al., 2014) checks
both `other` and local physical time in `observe()`. The Wave 9 spec,
however, says only: "if received PT > local, adopt it; if equal, take max
of logical and increment; if less, increment local logical". The spec
omits the local-physical-time check, so the implementation does too. The
tests (which compare against fixed `(physical_ns, logical)` sentinels)
would fail if `physical_now()` overrode the spec'd behavior. This is a
deliberate, documented deviation from the canonical HLC algorithm —
noted as future work below.

**`observe()` always produces a strictly greater timestamp than `last`.**
Tracing through the three cases: (1) `other.physical > last.physical` →
new physical > old physical → new > old regardless of logical; (2)
`other.physical == last.physical` → new logical = `max + 1 > last.logical`;
(3) `other.physical < last.physical` → new logical = `last.logical + 1`.
This is what makes `hlc_timestamps_are_totally_ordered` pass: a mix of
`now()` and `observe()` calls can never produce a tie or a regression.

**`HlcClock` uses `&mut self`, not `AtomicU64`.** The spec defines
`now(&mut self)` and `observe(&mut self, ...)`. A real production HLC would
use a `seqlock` or CAS loop on `AtomicU64`s for lock-free multi-threaded
access. This prototype enforces single-threaded access via `&mut self` —
the coordinator itself is the serialization point. A future wave could
swap the interior mutability for atomics without changing the API.

**`physical_now()` returns `u64`, not `Duration`.** The HLC algorithm
compares physical times with `>` and `<`, which is simplest on raw `u64`
nanoseconds. Returning `Duration` would require `.as_nanos() as u64` at
every call site. Defensive: returns 0 on pre-epoch error (which shouldn't
happen on a functioning system, but the unwrap-free path is cheap).

**`HlcClock::last()` is exposed for tests.** Production code should use
`now()` (which advances the clock); `last()` is a non-advancing peek that
lets tests assert "after observing `(100, 5)`, the clock is at `(100, 6)`"
without issuing another `now()` that would change the state.

### Task 9-2: CXL coordinator

**`commit()` takes `&mut self`, not `&self`.** The Wave 1 stub took `&self`
and returned a hardcoded `250u64`. The real implementation needs to
advance the HLC clock, which requires `&mut self`. The smoke example
was updated to `let mut cxl = CxlCoordinator::new(None);` to match.

**CXL detection reuses `crate::memory::numa::cxl_available()`.** That
function already checks for `/sys/bus/cxl` existence — no need to
duplicate the probe. The result is cached in `self.available` at
construction time; `is_available()` is a cheap `bool` read.

**The CXL path is a no-op (returns `clock.now()`).** We don't have CXL
hardware in the test environment, so the "CXL.cache flush + MFENCE" is
simulated as a no-op. The `available` flag is the liveness signal — when
`/sys/bus/cxl` exists, we trust that the fabric is present and skip the
WAL fallback. A future wave with real CXL hardware would replace the
no-op with the actual fence instruction.

**The WAL fallback writes the HLC timestamp as the payload.** The commit
record's payload is the 8-byte LE encoding of `ts.physical_ns`. This lets
a recovery loop reconstruct the commit order from the WAL alone (the
logical counter is implicit in the append order). The `txn_id` is also
stored, so a recovery loop can map timestamps back to transactions.

**The WAL fallback tolerates `wal == None`.** When `wal` is `None` (the
default), `commit()` still issues a timestamp and returns `Ok(ts)`, but
the commit is not durable across a crash. This is useful for in-memory
tests and for the smoke example, where durability isn't required. The
`commit_tier()` method returns `MemoryTier::Ddr5` in this case (instead
of `Cxl` or `Nvme`) so callers can tell.

**The HLC timestamp is issued BEFORE the WAL append.** This makes the
timestamp the linearization point: even if the WAL append fails, the
timestamp has been "spent" (the clock has advanced). In a real
implementation we'd want to undo the timestamp on durability failure
(e.g. by rolling back the logical counter), but for this prototype the
simpler "timestamp-first" model is fine — the next `commit()` will
issue a strictly greater timestamp regardless.

**The legacy `cxl_coordinator_doesnt_crash` test is kept.** Wave 1 had a
single regression test that just called `commit(0)` and checked it didn't
panic. I kept it (with `let mut c` and `c.commit(0).unwrap()`) so any
external CI configs that grep for the test name still work. The new
tests exercise the full OQ-02 contract.

**`commit_tier()` returns `Ddr5` when there's no WAL.** This is a slight
abuse of the tier enum — `Ddr5` is really a memory tier, not a "no
durability" marker. But it's the closest fit: the commit lives only in
DRAM (the HLC clock's `last` field), and `Ddr5` is the DRAM tier. The
alternative would be to return `Option<MemoryTier>` or add a new
`MemoryTier::Volatile` variant, both of which would ripple through the
codebase. The current choice is documented in the method's doc comment.

### Task 9-3: Raft coordinator

**`commit()` takes `&mut self`.** Same rationale as CXL: the HLC clock
needs `&mut self`. The smoke example was updated to `let mut raft = ...`.

**Follower commit returns `Error::Protocol`, not `Error::Other`.** The
existing `Error::Protocol` variant is documented as "protocol boundary
violation (e.g., CXL data leaked to a Raft txn)" — calling `commit()` on
a non-leader is a protocol violation in the same spirit. The error
message is `"node {node_id} is not the leader; cannot commit txn
{txn_id}"` so callers can distinguish it from other protocol errors.

**`new_as_leader` constructor.** The spec defines `new(cluster_size,
node_id, wal)` which starts as a follower. But every test that wants to
exercise `commit()` needs to call `become_leader()` first — tedious. The
`new_as_leader` convenience constructor does both in one call. It's
also useful for single-node bootstrap (a 1-node Raft cluster is trivially
its own leader).

**Quorum replication is simulated as a `tracing::debug!` log.** The spec
says "the 'replicate to quorum' is simulated (no real network). Just log
it and return." Using `tracing::debug!` (not `println!`) means the log
is silent by default in tests, but can be enabled with `RUST_LOG=debug`
for debugging. The log message includes the node ID, txn ID, and quorum
size for grep-ability.

**The WAL is synced AFTER the simulated replication, not before.** This
matches the real Raft commit path: (1) append to local log, (2)
replicate to followers, (3) wait for quorum ack, (4) mark committed
(which syncs the local log). Syncing before replication would make the
leader's log durable but wouldn't commit the entry (a quorum hasn't
acknowledged it yet). The current order — append, replicate (simulated),
sync — preserves the "commit point" semantics: once `sync()` returns,
the entry is durably committed on a quorum.

**The follower-doesn't-touch-WAL invariant is explicitly tested.** Test
`raft_non_leader_commit_does_not_touch_wal` constructs a coordinator with
a WAL, calls `commit()` on the follower, asserts the error, then asserts
`wal.records_written() == 0`. This catches a subtle bug: if the leader
check happened after the WAL append (instead of before), a follower
would write spurious commit records to its WAL, which a recovery loop
might misinterpret as committed entries.

**`quorum()` uses `cluster_size / 2 + 1`, matching the Wave 1 stub.**
This is `floor(N/2) + 1` (integer division rounds down). For the
standard Raft sizes (1, 3, 5, 7) this gives 1, 2, 3, 4 — all correct
majorities. The test also covers even cluster sizes (2, 7) to verify
the math doesn't break: 2 → 2 (no failure tolerance), 7 → 4 (tolerates
3 failures).

**`become_follower` is included for symmetry.** The spec only requires
`become_leader`, but adding `become_follower` is free and supports
leader-transfer scenarios in tests (e.g., "demote the leader, verify
the next commit fails").

### Task 9-4: Tests

**All 10 spec tests are covered**, mapped to specific unit tests:

| Spec test | Unit test |
|-----------|-----------|
| 1. HLC: `now()` returns monotonically increasing timestamps | `protocol::clock::tests::hlc_now_is_monotonic` (1000 iterations) |
| 2. HLC: `observe()` with a higher physical time adopts it | `protocol::clock::tests::hlc_observe_adopts_higher_physical` |
| 3. HLC: `observe()` with same physical time increments logical | `protocol::clock::tests::hlc_observe_same_physical_takes_max_logical` (covers both lower and higher remote logical) |
| 4. HLC: timestamps are totally ordered (no ties) | `protocol::clock::tests::hlc_timestamps_are_totally_ordered` (50 `now()` + 50 `observe()` interleaved) |
| 5. CxlCoordinator: `is_available()` returns false on non-CXL hardware | `protocol::cxl::tests::cxl_coordinator_is_available_returns_false_in_ci` (skips itself on CXL hardware) |
| 6. CxlCoordinator: `commit()` with WAL fallback succeeds and returns timestamp | `protocol::cxl::tests::cxl_commit_with_wal_fallback_succeeds` (verifies WAL record round-trips with the timestamp in the payload) |
| 7. CxlCoordinator: `commit()` without WAL still works | `protocol::cxl::tests::cxl_commit_without_wal_still_works` (+ `cxl_commit_is_monotonic` for 100 iterations) |
| 8. RaftCoordinator: quorum is correct for sizes 1, 3, 5 | `protocol::raft::tests::raft_quorum_is_majority` (also covers 2 and 7) |
| 9. RaftCoordinator: leader commit succeeds | `protocol::raft::tests::raft_leader_commit_succeeds` (+ `raft_leader_commit_without_wal_succeeds`, `raft_leader_commit_is_monotonic`) |
| 10. RaftCoordinator: non-leader commit returns error | `protocol::raft::tests::raft_non_leader_commit_returns_error` (+ `raft_non_leader_commit_does_not_touch_wal`) |

22 new unit tests in total: 9 HLC + 6 CXL + 7 Raft.

The most subtle test is `hlc_timestamps_are_totally_ordered`. It issues a
mix of `now()` and `observe(remote)` calls in a loop, where `remote` is
constructed to sometimes be ahead, sometimes behind, and sometimes equal
to the local clock (using `clock.last().physical_ns + (i % 7)`). The
test then asserts every adjacent pair is strictly increasing — no ties,
no regressions. This is the strongest single-test coverage of the HLC
monotonicity invariant.

The `cxl_commit_with_wal_fallback_succeeds` test goes beyond the spec by
verifying the WAL record round-trips: it opens a `WalReader` on the same
file, reads back the commit record, and asserts that the payload (the
8-byte LE-encoded `physical_ns`) matches the returned timestamp. This
catches a class of bugs where the coordinator writes the wrong field
to the WAL (e.g. `logical` instead of `physical_ns`, or the previous
timestamp instead of the current one).

The `raft_non_leader_commit_does_not_touch_wal` test catches a subtle
ordering bug: the leader check must happen BEFORE the WAL append, not
after. If the check were after, a follower's failed `commit()` would
leave a spurious commit record in the WAL.

## Constraints Check

- ✅ Read existing `src/protocol/cxl.rs` and `src/protocol/raft.rs` first
  (the Wave 1 stubs).
- ✅ Registered `pub mod clock;` in `src/protocol/mod.rs`.
- ✅ Used `std::time::SystemTime` for `physical_now()`.
- ✅ Imported `crate::storage::wal::Wal` for the fallback (via
  `crate::storage::wal::{Wal, WalRecord}`).
- ✅ `cargo fmt` clean (only nightly-only config warnings, no diff).
- ✅ `cargo clippy --all-targets -- -D warnings` clean (debug and release).
- ✅ `cargo test` passes: 282 unit + 7 integration = 289 total (debug
  and release modes both green). 267 existing + 22 new.
- ✅ All `unsafe` blocks need `// SAFETY:` comments — **no new `unsafe`
  blocks added.** The Wave 9 code is entirely safe Rust (the WAL fallback
  uses safe `Wal::append` / `Wal::sync`).

## DoD Check

- ✅ `cargo test` passes (267 existing + 22 new = 289 total).
- ✅ `cargo clippy -- -D warnings` passes (debug and release).
- ✅ HLC timestamps are monotonic and totally ordered (tests
  `hlc_now_is_monotonic`, `hlc_timestamps_are_totally_ordered`).
- ✅ CXL coordinator falls back to WAL when CXL is unavailable (test
  `cxl_commit_with_wal_fallback_succeeds` verifies the WAL record is
  appended and synced, and the timestamp round-trips through the
  payload).
- ✅ Raft coordinator rejects commits from non-leaders (test
  `raft_non_leader_commit_returns_error` asserts `Error::Protocol` with
  "not the leader"; test `raft_non_leader_commit_does_not_touch_wal`
  verifies the WAL is untouched).

## Spec-Test to Unit-Test Mapping (cross-reference)

| Spec # | Spec description | Unit test name | Module |
|--------|------------------|----------------|--------|
| 1 | HLC: now() returns monotonically increasing timestamps | `hlc_now_is_monotonic` | `protocol::clock::tests` |
| 2 | HLC: observe() with a higher physical time adopts it | `hlc_observe_adopts_higher_physical` | `protocol::clock::tests` |
| 3 | HLC: observe() with same physical time increments logical | `hlc_observe_same_physical_takes_max_logical` | `protocol::clock::tests` |
| 4 | HLC: timestamps are totally ordered (no ties) | `hlc_timestamps_are_totally_ordered` | `protocol::clock::tests` |
| 5 | CxlCoordinator: is_available() returns false on non-CXL hardware | `cxl_coordinator_is_available_returns_false_in_ci` | `protocol::cxl::tests` |
| 6 | CxlCoordinator: commit() with WAL fallback succeeds and returns timestamp | `cxl_commit_with_wal_fallback_succeeds` | `protocol::cxl::tests` |
| 7 | CxlCoordinator: commit() without WAL still works (returns timestamp) | `cxl_commit_without_wal_still_works` | `protocol::cxl::tests` |
| 8 | RaftCoordinator: quorum is correct for sizes 1, 3, 5 | `raft_quorum_is_majority` | `protocol::raft::tests` |
| 9 | RaftCoordinator: leader commit succeeds | `raft_leader_commit_succeeds` | `protocol::raft::tests` |
| 10 | RaftCoordinator: non-leader commit returns error | `raft_non_leader_commit_returns_error` | `protocol::raft::tests` |

## Future Work (Out of Scope for Wave 9)

- **Canonical HLC `observe()` with local physical time.** The current
  `observe()` follows the Wave 9 spec literally — it considers only
  `other` and `last.physical_ns`, not `physical_now()`. The canonical
  HLC algorithm (Kulkarni et al., 2014) also checks local physical time
  in `observe()` to keep the clock close to wall time. A future wave
  could add the local-physical-time check; the test sentinels would
  need to be adjusted to not assume fixed `physical_ns` values.

- **Lock-free `HlcClock` via `AtomicU64` + seqlock.** The current
  `&mut self` API enforces single-threaded access. A production HLC
  would use a `seqlock` (a `AtomicU64` counter incremented on write,
  readers retry if the counter changes during the read) or a CAS loop
  on a packed `AtomicU128`. The API would need to change from
  `&mut self` to `&self`.

- **Real CXL.cache flush.** The CXL path is a no-op (`return clock.now()`).
  On real CXL 3.0 hardware, the flush would be a `clwb` (cache line write
  back) instruction on the modified cache lines followed by an `mfence`.
  This requires identifying the cache lines backing the transaction's
  writes — likely via the `CxlRef` linear handle's region pointer.

- **Real Raft replication over RoCEv2/IB.** The "replicate to quorum" step
  is a `tracing::debug!` log. A real implementation would issue
  `AppendEntries` RPCs over RDMA, wait for quorum ack, and handle
  retransmission / leader-election / log-matching invariants. This is
  a multi-month effort; the simulation here is enough to exercise the
  coordinator API and the WAL durability path.

- **Leader election.** `become_leader()` is a setter; there's no
  RequestVote RPC, no leader lease, no heartbeat. A real Raft
  implementation would have a background `ElectionTimer` thread that
  triggers an election if it doesn't receive a heartbeat within a
  randomized timeout. This is needed before the coordinator can be
  used in a real multi-node deployment.

- **HLC timestamp in the WAL record header.** The current WAL fallback
  stores the timestamp in the record's `payload` (an 8-byte LE `u64`).
  A more integrated design would add a dedicated `physical_ns` and
  `logical` field to `WalRecord` itself, so the WAL reader doesn't need
  to know the payload format. This is a `WalRecord` schema change that
  would ripple through Wave 8's `WalReader` — deferred to avoid
  cross-wave coupling.

- **Clock skew detection.** If `observe(remote)` consistently sees
  `remote.physical_ns >> local.physical_ns`, the local clock is skewed.
  A production HLC would log this (or trigger an NTP sync). The current
  implementation silently adopts the remote time — correct, but
  not observable.

## Reproduction

```bash
cd /home/z/turbogp
export PATH="$HOME/.cargo/bin:$PATH"

# Format check (only nightly-config warnings, no diff).
cargo fmt --check

# Lint (clean in debug and release).
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --release -- -D warnings

# Tests (282 unit + 7 integration = 289 total).
cargo test
cargo test --release

# Protocol tests only (24 tests).
cargo test protocol::

# Run the smoke example (verifies the API change doesn't break callers).
cargo run --example smoke
```

All gates green as of this commit.
