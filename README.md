# TensorVault

> **Bit-Uniform Relational Storage** — a research program investigating what
> becomes possible when every value in every column of a relational database
> is stored as a single 64-bit NaN-boxed word, and that invariant is made the
> load-bearing primitive of storage, indexing, execution, incremental
> maintenance, and query optimization.

This repository contains:

- **`src/`** — Phase-1 Rust prototype of the niche-filled `Cell` type, with
  AVX-512 scan kernels, bit-sliced index, hash join, and MDL-driven schema
  selection. ~2k lines, 54 tests passing.
- **`docs/`** — Three LaTeX/PDF documents laying out the architecture:
  - `position_paper.pdf` — 8-page SIGMOD/VLDB vision-track position paper
  - `mdl_sketch.pdf` — Formal MDL schema-selection algorithm with theorems
  - `commodity_hw.pdf` — Commodity-hardware execution path (AVX-512 only,
    no FPGA/PIM/TCAM)

## Thesis

Treat every value in the database as a 64-bit NaN-boxed word, with
niche-filling (Rust/Swift style) for NULL and variant tags. Make this the
load-bearing invariant of the entire system: storage, indexing, execution,
incremental view maintenance, and query optimization all compose around it.
The result is a database with three properties no existing system has
simultaneously:

1. **Similarity search is a first-class SQL operator on any column type** —
   not just numeric vectors.
2. **Schema-on-read is free and MDL-optimal** — type interpretation is
   chosen at load time by minimizing description length.
3. **One physical format serves every operator and every index** — the
   `Cell` word is the universal substrate.

## Quick start

```bash
# Build and test the Rust prototype
cargo test
cargo run --release --example smoke

# Benchmark (requires AVX-512 for full speed)
cargo bench
```

## Measured throughput (single core, AVX-512)

| Operation | Throughput |
|-----------|-----------|
| `count_eq` over 1M cells | 1.8 B cells/sec |
| `sum_f64` over 1M cells | 2.2 B cells/sec |
| `count_similar` (Hamming ≤8) | ~1.5 B cells/sec |

## Repository layout

```
tensorvault/
├── README.md                ← you are here
├── Cargo.toml               ← crate manifest (tensorvault-bitcell)
├── .gitignore
├── src/
│   ├── lib.rs               ← crate root
│   └── bitcell/
│       ├── mod.rs           ← module docs + encoding table
│       ├── cell.rs          ← Cell type, NaN-boxing, niche-filling
│       ├── column.rs        ← CellColumn, Batch, homogeneity
│       ├── scan.rs          ← AVX-512 kernels + scalar fallback
│       ├── bsi.rs           ← bit-sliced index (64 bitmaps/column)
│       ├── hash.rs          ← SwissTable-style hash join
│       └── mdl.rs           ← MDL-driven schema selection
├── examples/
│   └── smoke.rs             ← end-to-end demo
├── benches/
│   └── throughput.rs        ← criterion benchmarks
└── docs/
    ├── position_paper.{tex,pdf}    ← vision-track paper
    ├── mdl_sketch.{tex,pdf}        ← MDL formalization
    └── commodity_hw.{tex,pdf}      ← AVX-512 execution path
```

## The NaN-box encoding

| Bit pattern | Type |
|-------------|------|
| `0x0000_0000_0000_0000` | NULL |
| exponent ≠ `0x7FF`, ≠ `0` | real f64 (identity boxing) |
| `0x7FF8_0000_0000_0000` | canonical NaN sentinel |
| `0xFFF0_xxxx_xxxx_xxxx` | tagged i32 (low 32 bits) |
| `0xFFF1_xxxx_xxxx_xxxx` | tagged bool (low 8 bits) |
| `0xFFF2_xxxx_xxxx_xxxx` | tagged 48-bit pointer |
| `0xFFF3_xxxx_xxxx_xxxx` | tagged date (i32 days) |
| `0xFFF4_xxxx_xxxx_xxxx` | tagged timestamp (47 bits) |
| `0xFFF5_xxxx_xxxx_xxxx` | tagged f16 (16 bits) |
| subnormal (exp=0, mantissa≠0) | short string (≤6 ASCII bytes) |

## Novel contributions

1. **Unified physical format** — niche-filled NaN-boxed words for every
   column, including nullable + variant columns, with zero-overhead NULL
   and short-string optimization.
2. **Unified index** — bit-sliced bitmap index on the raw 64-bit pattern
   that answers equality / range / similarity queries on any column type.
3. **Tag-aware trace JIT** — query traces specialized on the observed tag
   distribution of the data, not just the query plan.
4. **MDL-driven automatic schema selection** at load time, with
   information-lattice equivalence proofs for safe read-time reinterpretation.
5. **Hardware stack speaking one word format** (future work — out of scope
   for the commodity-hardware design).
6. **Sum-product query optimizer with sketch messages** — exact (differential)
   and approximate (HLL/Count-Min) propagation unified in one framework.
7. **Similarity joins as first-class SQL on any column type** — strings,
   dates, UUIDs, JSON blobs, anything.

## Research agenda

| Phase | Status | Description |
|-------|--------|-------------|
| 1 | ✅ Done (this repo) | Storage: niche-filled NaN-boxed words |
| 2 | Pending | Unified bit-sliced + LSH index |
| 3 | Pending | Trace-JIT specialization with Cranelift |
| 4 | Pending | Incremental view maintenance via differential dataflow |
| 5 | Out of scope | Hardware offload (FPGA/PIM/TCAM) — explicitly excluded |
| 6 | Pending | MDL formalization + information-lattice equivalence proofs |

## Constraints

This research program is **commodity-hardware only**. No FPGA, no PIM (UPMEM),
no TCAM. The execution path targets x86-64 with AVX-512 (Ice Lake+, Zen 4)
and ARM with NEON/SVE. See `docs/commodity_hw.pdf` for the full design.

## License

MIT OR Apache-2.0.
