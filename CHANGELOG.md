# Changelog

All notable changes to TensorVault are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

## [0.2.0] — 2025-07-30

### Added
- Instruction-first, memory-centric architecture (25 ADRs)
- Kernel table with 16 AVX-512/AVX2/scalar kernels
- Tier-aware memory manager (8 tiers, NUMA detection, LRU migration)
- Storage format: 4 KB page / 2 MB region / 2 GB tablet
- ZNS-aware WAL with CRC32C checksums
- Data-centric morsel-driven executor (ADR-018)
- DPccp join ordering (ADR-019)
- Approximate SQL with (ε,δ) guarantees (ADR-015, ADR-024)
- Similarity search via VPOPCNTDQ + LSH (ADR-017)
- rANS compression for cold-tier columns (ADR-025)
- Calibrated analytic cost model (ADR-023, measured on Zen 5)
- Formal specification (SPECIFICATION.md, 755 lines)
- Problem catalog: 99 problems across 10 files
- 5-wave research corpus with per-problem solution evaluations
- CCL-X 1.2 license

### Measured performance (AMD EPYC-Turin / Zen 5)
- scan_eq AVX-512: 24.1 G cells/sec
- sum_f64 AVX-512: 29.8 G cells/sec
- hamming VPOPCNTDQ: 24.2 G cells/sec
- Memory bandwidth: 40.6 GB/s

## [0.1.0] — 2025-07-28

### Added
- Initial NaN-boxed Cell prototype (superseded by instruction-first architecture)
- Basic encoders: TF-IDF, char n-gram, color histogram, DCT, FFT, feature hashing, random projection
- Non-ML tensor storage with int8 quantization and sparse CSR
- LSM-style storage (WAL + SSTable)
- LSH and brute-force indexes
- axum HTTP server + clap CLI
