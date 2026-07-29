# Protocol Boundary Problems

> Problems related to the transaction coordinator that runs at protocol
> boundaries: CXL for single-rack, Raft over RoCEv2 for cross-rack, async
> for cross-region.
>
> **Research source**: `docs/cpu_energy_kb.md` §4 (interconnects),
> `docs/research/category_theory_topology_db.md` (linear types, sheaves).

---

## P-04-01: Protocol boundary type safety 🔴

**Layer**: Protocol
**Status**: 🔴 open
**Math**: V (category theory — linear type theory)
**Effort**: M (2 months per `docs/math_enhancements.md` Enhancement 5)
**Impact**: critical

### Problem

This is **Enhancement 5** from `docs/math_enhancements.md`. Protocol safety
is currently a runtime check. It should be a compile-time proof via linear
types:

- `CxlRef<T>` — linear; cannot be duplicated; cannot escape the rack scope
- `RaftRef<T>` — affine; can be dropped; cannot be duplicated
- `LocalRef<T>` — unconstrained

A CXL-resident region's reference cannot leak into a cross-rack transaction;
the type system prevents it.

### Open questions

- Rust's affine type system does 80% of the work. Can we get the remaining
  20% via `Drop` impls and `PhantomData` markers?
- How do we handle the case where a CXL reference must be sent to another
  rack (explicit serialization + Raft commit)?

### Success criteria

- `CxlRef<T>` and `RaftRef<T>` types in `src/types/`.
- A compile-time test: code that tries to send a `CxlRef` across a Raft
  boundary fails to compile.

---

## P-04-02: CXL 3.0 fabric integration 🔴

**Layer**: Protocol
**Status**: 🔴 open (🟡 stub exists in `src/protocol/cxl.rs`)
**Math**: none
**Effort**: L (3–6 months)
**Impact**: critical

### Problem

This is the **must-solve** problem for single-rack transactions. The CXL
coordinator stub returns a hardcoded 250 ns latency. Real integration needs:

1. Detect CXL 3.0 fabric devices via `/sys/bus/cxl/`
2. Allocate CXL-resident commit records
3. Issue `CXL.cache` flush + `MFENCE` on commit
4. Handle CXL link contention (fall back to Raft if CXL is saturated)

### Open questions

- Do we need a CXL switch (CXL 3.0 multi-level) or is a single CXL device
  enough for single-rack coherence?
- How do we test without CXL hardware? (Emulate via shared memory + `mfence`?)

### Success criteria

- `CxlCoordinator::commit()` issues a real CXL flush on CXL hardware.
- Single-rack transactions commit in < 500 ns p99.
- A test that runs 1M transactions and verifies linearizability.

---

## P-04-03: Raft over RoCEv2 🔴

**Layer**: Protocol
**Status**: 🔴 open (🟡 stub exists in `src/protocol/raft.rs`)
**Math**: none
**Effort**: L
**Impact**: high

### Problem

The Raft coordinator stub returns a hardcoded 10 µs latency. Real
integration needs:

1. RDMA transport (RoCEv2 or IB) via `libibverbs` / `rdma-core`
2. Raft log replication via RDMA writes (not sends — lower latency)
3. Leader election with heartbeats over RDMA
4. Integration with the local NVMe WAL (log entries are durable locally
   before replication)

### Open questions

- Should we use an existing Raft implementation (openraft, dragonraft) or
  write our own?
- RDMA write vs RDMA send: write is lower latency but requires the leader to
  know follower buffer addresses.

### Success criteria

- `RaftCoordinator::commit()` replicates to a quorum via RDMA.
- Cross-rack transactions commit in < 15 µs p99.
- A 3-node test cluster that survives a leader failure.

---

## P-04-04: Cross-rack serialization format 🔴

**Layer**: Protocol
**Status**: 🔴 open
**Math**: I (source coding — entropy-optimal serialization)
**Effort**: M
**Impact**: medium

### Problem

When data crosses a rack boundary (via Raft), it must be serialized. The
format should be:
1. **Compact** (entropy-coded via ANS — see P-03-03)
2. **Schema-aware** (use Slepian-Wolf coding with side information at the
   receiver — see `docs/research/info_theory_for_db.md` §5)
3. **SIMD-decodable** at the receiver

### Open questions

- Can the sender assume the receiver has a cached version of the data
  (Slepian-Wolf)? If so, only the delta needs to be sent.
- How do we handle schema mismatches between sender and receiver?

### Success criteria

- A `WireFormat` that achieves < 50% of the raw data size on typical
  transactions.
- Decode at the receiver uses AVX-512 kernels.

---

## P-04-05: Distributed transaction isolation 🔴

**Layer**: Protocol
**Status**: 🔴 open
**Math**: IV (optimization — game theory for multi-query coordination)
**Effort**: XL
**Impact**: high

### Problem

Single-rack transactions use CXL coherence (snapshot isolation for free).
Cross-rack transactions need explicit isolation:

- **2PC** (Two-Phase Commit): slow, blocking
- **Calvin** (deterministic): fast, requires pre-ordering
- **Saga**: eventual consistency, not suitable for OLTP

The engine should pick the protocol per transaction based on the
participant set:

```
if all participants in one rack:
    use CXL coherence (free)
elif all participants in one region:
    use Raft-based 2PC
else:
    use saga (async)
```

### Open questions

- How do we detect the participant set before the transaction starts?
  (Static analysis of the query? Runtime discovery?)
- Can we use Calvin's deterministic ordering for cross-rack transactions
  to avoid 2PC?

### Success criteria

- A `TransactionCoordinator` that picks the protocol automatically.
- Cross-rack transactions achieve serializability with < 50 µs p99 commit.

---

## P-04-06: Consistency models per tier 🟡

**Layer**: Protocol
**Status**: 🟡 partial (documented, not enforced)
**Math**: V (category theory — sheaf conditions for gluing)
**Effort**: L
**Impact**: medium

### Problem

Each tier has a different consistency model:
- **L3/DDR5/HBM**: strongly consistent (single machine, cache-coherent)
- **CXL**: strongly consistent within a rack (CXL.cache coherence)
- **NVMe**: durable but not coherent (each host has its own page cache)
- **Network**: eventually consistent (async replication)

The engine should expose this to the user via the query syntax (see
`06-query-syntax-approach.md`):

```sql
SELECT * FROM orders CONSISTENCY STRONG;       -- CXL or local
SELECT * FROM orders CONSISTENCY READ_COMMITTED; -- NVMe with flush
SELECT * FROM orders CONSISTENCY EVENTUAL;      -- cross-region async
```

### Open questions

- How do we map SQL consistency levels to protocol choices?
- Can we use sheaf theory (`docs/research/category_theory_topology_db.md` §10)
  to formalize the gluing of consistency models across tiers?

### Success criteria

- A `ConsistencyLevel` enum in the query parser.
- The planner maps consistency levels to protocol/tier choices.

---

## P-04-07: Replication log shipping 🔴

**Layer**: Protocol
**Status**: 🔴 open
**Math**: I (fountain codes — Luby, RaptorQ)
**Effort**: L
**Impact**: high

### Problem

Cross-region replication is async — the WAL is shipped to remote regions
in the background. The shipping should be:

1. **Fountain-coded** (RaptorQ): the sender generates an infinite stream of
   parity shards; any receiver that collects enough shards can recover.
   No need for per-receiver state.
2. **Bandwidth-adaptive**: throttle shipping based on available cross-region
   bandwidth
3. **Loss-tolerant**: handle packet loss without stalling the primary

### Open questions

- Should we use RaptorQ (RFC 6330) or a simpler fountain code?
- How do we handle cross-region failover (promote a replica to primary)?

### Success criteria

- A `ReplicationShipper` that streams WAL records via RaptorQ.
- Cross-region replication lag < 1 second on typical WAN links.

---

## P-04-08: Clock synchronization 🔴

**Layer**: Protocol
**Status**: 🔴 open
**Math**: III (probability — Hoeffding bounds on clock skew)
**Effort**: M
**Impact**: medium

### Problem

Distributed transactions need a consistent clock for snapshot isolation and
conflict detection. Options:

1. **NTP / PTP**: hardware-assisted clock sync (~100 µs accuracy with PTP)
2. **Hybrid Logical Clocks (HLC)**: Kulkarni 2014 — combines physical time
   with Lamport clocks
3. **TrueTime** (Google Spanner): dedicated time servers with bounded uncertainty

### Open questions

- Is PTP available in modern datacenters? (AWS supports it.)
- Can we use HLC to avoid the need for precise clock sync?

### Success criteria

- A `Clock` trait with implementations: `LocalClock`, `HlcClock`,
  `PtpClock`.
- The transaction coordinator uses the clock for snapshot isolation.

---

## Summary

| # | Problem | Status | Effort | Impact |
|---|---------|--------|--------|--------|
| 01 | Protocol boundary type safety | 🔴 | M | critical |
| 02 | CXL 3.0 fabric integration | 🔴 | L | critical |
| 03 | Raft over RoCEv2 | 🔴 | L | high |
| 04 | Cross-rack serialization format | 🔴 | M | medium |
| 05 | Distributed transaction isolation | 🔴 | XL | high |
| 06 | Consistency models per tier | 🟡 | L | medium |
| 07 | Replication log shipping | 🔴 | L | high |
| 08 | Clock synchronization | 🔴 | M | medium |
