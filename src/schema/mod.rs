//! The schema layer — the LAST layer of the architecture.
//!
//! The schema is metadata about the instruction streams. It exists to:
//! 1. Map SQL column references to (tablet list, kernel id) pairs
//! 2. Validate queries at parse time
//! 3. Provide type information for kernels that need it
//!
//! This module is a stub — a full SQL parser is out of scope for the prototype.

pub mod mdl;
pub mod table_schema;

pub use mdl::{schema_select, schema_select_with_diagnostics, TypeInterpretation};
