//! The executor: a scheduler of instruction streams.
//!
//! The executor receives a logical plan, lowers it to a DAG of kernel
//! invocations, and schedules them respecting data dependencies and tier
//! bandwidth.

pub mod plan;
pub mod scheduler;

pub use plan::{KernelInvocation, LogicalPlan, PlanNode};
pub use scheduler::Scheduler;
