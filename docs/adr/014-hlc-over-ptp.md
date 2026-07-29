# ADR-014: HLC over PTP for clock synchronization

## Status
Accepted

## Confidence
80%

## Context

Distributed transactions need a consistent clock for snapshot isolation and conflict detection. Options:
- **NTP**: ~1–10 ms accuracy — too coarse for OLTP
- **PTP (IEEE 1588)**: ~100 µs accuracy with hardware support — available in modern datacenters (AWS supports it)
- **HLC (Hybrid Logical Clock)**: combines physical time with Lamport clocks — no commit-wait delay
- **TrueTime (Spanner)**: dedicated time servers with bounded uncertainty — requires special hardware

TrueTime's commit-wait introduces latency (7 ms in Spanner). HLC avoids this.

## Decision

**Use HLC (Hybrid Logical Clocks) over PTP for clock synchronization.**

- PTP provides the physical time base (~100 µs accuracy)
- HLC adds a logical component to break ties when physical clocks are close
- No commit-wait (unlike TrueTime) — transactions commit immediately

HLC timestamp: `(physical_time_ns, logical_counter)`. Comparison: physical first, then logical.

## Consequences

### Positive
- No commit-wait delay (unlike TrueTime) — lower OLTP latency
- PTP accuracy (~100 µs) is sufficient for snapshot isolation
- HLC is causally consistent (preserves "happened-before" relationships)
- Works with standard PTP hardware (no custom time servers)

### Negative
- 100 µs accuracy means clocks can disagree by up to 200 µs — may cause false conflicts in OLTP
- HLC adds a logical counter (8 bytes) to every timestamp — minor overhead
- PTP requires hardware support (NIC with PTP) — not available everywhere

## Alternatives considered

1. **TrueTime (Spanner style)** — tightest bounds but requires commit-wait (7 ms latency hit). Rejected for OLTP.
2. **NTP only** — ~1–10 ms accuracy, too coarse. Rejected.
3. **Vector clocks** — unbounded size (grows with node count). Rejected.
4. **Lamport clocks** — no physical time component, can't do snapshot isolation. Rejected.

## Compatibility

- Compatible with ADR-013 (linear types): HLC timestamps are plain values, no type conflict
- Compatible with ADR-020 (Kingman admission): HLC gives consistent timestamps for queue ordering
- Compatible with ADR-018 (morsel executor): morsels carry HLC timestamps for snapshot reads

## References
- Kulkarni et al., "Logical Physical Clocks" ICDCN 2014
- Corbett et al., "Spanner" OSDI 2012
- IEEE 1588 PTP standard
