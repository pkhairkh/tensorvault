# ADR-012: CRC32C + per-page XOR parity for checksum

## Status
Accepted

## Confidence
85%

## Context

Each 4 KB page needs corruption detection (silent data corruption is a real problem at scale — Google reports ~5% of drives experience it per year). The current xxh3 checksum detects corruption but can't correct it.

We need:
- Detection at line rate (>30 GB/s)
- Single-bit error correction (the most common corruption type)
- Low overhead (< 5% of page read time)

## Decision

**Use CRC32C for detection + a per-page 8-byte XOR parity for single-bit correction.**

- **CRC32C** (Castagnoli polynomial): hardware-accelerated via `SSE4.2` `_mm_crc32_u64` instruction. ~30 GB/s throughput, ~0.1 nJ/byte.
- **XOR parity**: XOR all 8-byte words in the page into a single 8-byte value stored in the page header. If the CRC mismatches but the XOR parity localizes the error to a single word, we can correct it by XOR-ing the expected and actual parity.

Page header layout (64 bytes):
```
[0..4)   CRC32C of cells (4 bytes)
[4..12)  XOR parity of cells (8 bytes)
[12..64) Other header fields
```

## Consequences

### Positive
- Detection at 30 GB/s (SSE4.2 hardware CRC) — essentially free
- Single-bit correction for the most common corruption type
- ~0.1 nJ/byte energy overhead — negligible
- CRC32C is the industry standard (ext4, btrfs, NVMe T10 PI all use it)

### Negative
- Can't correct multi-bit errors (but those are rare; report and recover from backup)
- XOR parity adds 8 bytes per page (0.2% overhead)
- CRC32C is weaker than xxh3 for collision resistance — acceptable for corruption detection (not security)

## Alternatives considered

1. **LDPC (Low-Density Parity-Check)** — much stronger correction but ~100× the energy of CRC32C. Overkill for single-bit correction. Rejected.
2. **Hamming code** — single-error correction, double-error detection. But computing it is slower than CRC32C + XOR parity. Rejected.
3. **xxh3 (current)** — fast detection but no correction. Kept for non-page data (WAL records, etc.).

## References
- Intel, "CRC32 Instruction" whitepaper (SSE4.2)
- MacKay, "Information Theory, Inference, and Learning Algorithms" ch. 13
- `src/storage/page.rs` (current xxh3 implementation — to be updated)
