//! # Bit-Uniform Cell Module
//!
//! Phase-1 prototype of the niche-filled NaN-boxed `Cell` type described in
//! the position paper "Bit-Uniform Relational Storage".
//!
//! Every value in every column is stored as a single 64-bit word. The IEEE-754
//! double bit-space is partitioned into:
//!
//! | Pattern | Meaning |
//! |---------|---------|
//! | `0x0000_0000_0000_0000` | NULL |
//! | exponent ≠ `0x7FF` | real f64 (identity boxing, zero overhead) |
//! | `0x7FF0_0000_0000_0000` | canonical NaN sentinel |
//! | `0xFFF0_xxxx_xxxx_xxxx` | tagged i32 in low 32 bits |
//! | `0xFFF1_xxxx_xxxx_xxxx` | tagged bool / small enum in low 8 bits |
//! | `0xFFF2_xxxx_xxxx_xxxx` | tagged 48-bit pointer |
//! | `0xFFF3_xxxx_xxxx_xxxx` | tagged date (i32 days since epoch) |
//! | subnormal (`exp=0, mantissa≠0`) | short string: up to 6 ASCII bytes |
//!
//! Niche-filling: NULL is reclaimed from the NaN tag, the variant discriminant
//! is the high 16 bits, and short strings live in the subnormal payload. A
//! nullable `Optional<Union<i32, f64, &str>>` is exactly 8 bytes per value.
//!
//! ## Modules
//!
//! - [`cell`] — the `Cell` type, constructors, accessors, type tag
//! - [`column`] — `CellColumn`, a `Vec<Cell>` with batch homogeneity tracking
//! - [`scan`] — AVX-512 scan kernel (`_mm512_popcnt_epi64`, `_mm512_xor_si512`)
//! - [`bsi`] — bit-sliced index: 64 Roaring-style bitmaps per column
//! - [`hash`] — SwissTable-style hash join probe on bit-uniform keys
//! - [`mdl`] — MDL-driven schema selection (skeleton)
//!
//! ## Status
//!
//! This is a research prototype. The encodings are stable; the kernels work
//! on x86-64 with AVX-512VPOPCNTDQ (Ice Lake+, Zen 4) and fall back to scalar
//! on other targets.

pub mod bsi;
pub mod cell;
pub mod column;
pub mod hash;
pub mod mdl;
pub mod scan;

pub use cell::{Cell, TypeTag};
pub use column::{Batch, CellColumn, Homogeneity};
