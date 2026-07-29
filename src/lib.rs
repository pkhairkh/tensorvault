//! # TensorVault
//!
//! An instruction-first, memory-centric relational database engine.
//!
//! The thesis: design the database from the silicon up. Pick the cheapest
//! instructions per joule, place data in the memory tier that feeds them,
//! and treat every protocol boundary as a first-class design axis.
//!
//! ## Modules
//!
//! - [`kernel`] — the kernel table: hand-tuned instruction sequences per
//!   (CPU, tier) tuple. The engine's competitive moat.
//! - [`memory`] — tier-aware memory manager. Placement, migration, NUMA.
//! - [`storage`] — instruction-shaped storage format (4 KB page, 2 MB region,
//!   2 GB tablet).
//! - [`executor`] — scheduler of instruction streams.
//! - [`protocol`] — protocol boundary coordinator (CXL, Raft/RoCEv2).
//! - [`schema`] — the last layer: SQL parser, MDL schema selection.

#![warn(rust_2018_idioms, missing_docs)]

pub mod executor;
pub mod kernel;
pub mod memory;
pub mod protocol;
pub mod schema;
pub mod storage;

pub use error::{Error, Result};

mod error {
    use thiserror::Error;

    /// Top-level error type.
    #[derive(Debug, Error)]
    pub enum Error {
        /// I/O error.
        #[error("io error: {0}")]
        Io(#[from] std::io::Error),

        /// JSON failure.
        #[error("json error: {0}")]
        Json(#[from] serde_json::Error),

        /// Dimension mismatch.
        #[error("dimension mismatch: expected {expected}, got {actual}")]
    DimMismatch { expected: usize, actual: usize },

        /// Invalid argument.
        #[error("invalid argument: {0}")]
        InvalidArg(String),

        /// Corruption.
        #[error("corruption: {0}")]
        Corruption(String),

        /// Not found.
        #[error("not found: {0}")]
        NotFound(String),

        /// Unsupported on this hardware.
        #[error("unsupported: {0}")]
        Unsupported(String),

        /// Generic.
        #[error("{0}")]
        Other(String),
    }

    /// Convenience Result alias.
    pub type Result<T> = std::result::Result<T, Error>;
}
