//! Compression module: lossy / lossless data compression for turboGP's
//! memory tiers (Wave 17).
//!
//! ## Submodules
//!
//! - [`tensor_train`] — Tensor-Train decomposition for multi-column data
//!   (Oseledets 2011). Compresses a `d`-mode tensor from `O(n^d)` to
//!   `O(d · n · r²)` parameters, where `r` is the TT-rank.
//!
//! ## Motivation
//!
//! The compression module bridges the planner's tensor-network model
//! (see [`crate::planner::tensor`]) with the storage layer's column
//! representations. A relation whose join hypergraph has low treewidth
//! admits a low-TT-rank factorization — the same structural property
//! that makes the join tractable also makes the data compressible.

pub mod tensor_train;

pub use tensor_train::TensorTrain;
