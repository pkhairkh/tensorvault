//! # bitcell
//!
//! Phase-1 prototype of the niche-filled NaN-boxed `Cell` type described in
//! the position paper "Bit-Uniform Relational Storage".
//!
//! Every value in every column is stored as a single 64-bit word. The IEEE-754
//! double bit-space is partitioned into tagged namespaces (NULL, i32, bool,
//! pointer, date, short-string, real f64). Niche-filling reclaims NULL and
//! variant tags from impossible bit patterns.
//!
//! See the [`bitcell`] module for the encoding table and the [`Cell`] type.

pub mod bitcell;

pub use bitcell::{Cell, CellColumn, TypeTag};
