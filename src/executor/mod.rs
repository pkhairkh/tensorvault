//! The executor: a scheduler of instruction streams.
//!
//! The executor receives a logical plan, lowers it to a DAG of kernel
//! invocations, and schedules them respecting data dependencies and tier
//! bandwidth.
//!
//! ## Wave 6: morsel-driven execution (ADR-018)
//!
//! The original [`Scheduler`] dispatches kernel invocations sequentially, one
//! per region. The morsel-driven executor (this module's `morsel`, `worker`,
//! `pipeline`, and `dispatcher` submodules) breaks each region's work into
//! 1024-cell morsels and dispatches them across a pool of NUMA-pinned workers.
//! Each worker runs a [`Pipeline`] of stages (scan → filter → aggregate) on
//! one morsel at a time, keeping intermediate data in L1/L2 — 5–10× faster
//! than the Volcano pull model (Leis 2014).
//!
//! The two execution paths coexist: `Scheduler` is the v1 sequential path
//! (used by `lib.rs`'s public API and existing integration tests), while the
//! morsel-driven types are the v2 path that downstream waves will wire into
//! the query planner.
//!
//! ## Wave 16: adaptive execution / eddies
//!
//! The [`eddy`] submodule implements adaptive tuple routing (Avnur &
//! Hellerstein, SIGMOD 2000): per-morsel, the eddy picks the next operator
//! based on observed selectivity (most selective first = principle of least
//! work). The [`adaptive`] submodule implements runtime plan switching based
//! on observed vs. estimated cardinality divergence. Both are alternative
//! execution modes that coexist with the fixed-order [`Pipeline`].

pub mod adaptive;
pub mod dispatcher;
pub mod eddy;
pub mod morsel;
pub mod pipeline;
pub mod plan;
pub mod scheduler;
pub mod worker;

pub use adaptive::AdaptiveExecutor;
pub use dispatcher::MorselDispatcher;
pub use eddy::{Eddy, EddyOperator, DEFAULT_LEARNING_RATE};
pub use morsel::{Morsel, MORSEL_SIZE};
pub use pipeline::{Pipeline, PipelineBreaker};
pub use plan::{KernelInvocation, LogicalPlan, PlanNode};
pub use scheduler::Scheduler;
pub use worker::WorkerThread;
