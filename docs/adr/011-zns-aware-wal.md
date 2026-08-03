# ADR-011: ZNS-aware WAL via io_uring

## Status
Accepted

> **⚠️ Implementation note (Wave 54):** The ZNS-aware WAL described in this
> ADR is **NOT** the production WAL. The actual WAL lives in
> `src/storage/recovery.rs` and is a simple append-only file with base64-
> encoded SQL records (see Wave 51 fix). The ZNS/io_uring implementation
> was never completed — `src/storage/wal.rs` is a stub. This ADR describes
> the *design intent* for a future production WAL, not the current code.

## Confidence
85%

## Context

The WAL (Write-Ahead Log) must fsync after every transaction group commit. On conventional NVMe SSDs, fsync latency is ~10–30 µs but has high variance (GC-induced tail latency up to 100+ ms).

ZNS (Zoned Namespace) SSDs expose zones that must be written sequentially. This eliminates the FTL's garbage collection, giving:
- 4–5× lower write amplification
- 57% better p99 latency (Bjørling ATC 2021)
- Predictable performance (no GC spikes)

## Decision

**Use ZNS NVMe for the WAL, accessed via `io_uring` (Linux async I/O).**

- Detect ZNS at startup via `ioctl(BLKGETZONESZ)`
- Allocate zones explicitly via `ioctl(BLKOPENZONE)`
- Write sequentially within a zone
- Finish a zone when full (`ioctl(BLKFINISHZONE)`)
- Never overwrite — old zones are reset in bulk after checkpoint

For non-ZNS SSDs, fall back to conventional `io_uring` with `O_DIRECT`.

## Consequences

### Positive
- p99 fsync latency < 30 µs (vs ~100 µs on conventional NVMe)
- 4–5× lower write amplification (extends SSD lifespan)
- Predictable tail latency (critical for OLTP SLAs)
- `io_uring` gives kernel-bypass async I/O (no syscall per write)

### Negative
- ZNS hardware is not universally available (enterprise SSDs only)
- Zone management adds complexity (zone finish boundary mid-transaction)
- `io_uring` requires Linux 5.1+ (not portable to macOS/Windows)

## Alternatives considered

1. **SPDK (Storage Performance Development Kit)** — user-space NVMe driver, best performance. But pins the entire NVMe device, preventing sharing. Rejected for v1; consider for dedicated OLTP appliance.
2. **Conventional NVMe with `O_DIRECT`** — works everywhere but has GC tail latency. Kept as fallback.
3. **`fsync` + buffered I/O** — too slow (50–100 µs per fsync). Rejected.

## Compatibility

- Compatible with ADR-002 (WAL pages are 4 KB, matching the page format)
- Compatible with ADR-013 (WAL is replicated cross-rack via Raft — ZNS is local only)
- Compatible with ADR-020 (Kingman admission control needs predictable fsync latency)

## References
- Bjørling et al., "ZNS: Avoiding the Block Interface Tax" ATC 2021
- atlarge group, "ZNS characterization" CLUSTER 2023
- Linux `io_uring` documentation
