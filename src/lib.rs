//! Educational library for planning and executing file-range read schedules.
//!
//! Pure planning stays separate from I/O. Schedule parsing
//! ([`parse_schedule`]), validated ranges and deterministic coalescing
//! ([`ReadRange`], [`ReadPlan`]), physical planning ([`ExecutionConfig`],
//! [`ExecutionPlan`]), budget accounting ([`ByteBudget`], [`BudgetLimiter`]),
//! scheduling ([`Scheduler`]), and checksums ([`checksum`]) are
//! deterministic and never touch a file. The synchronous positioned-read
//! backend ([`read_plan`], [`read_scheduled`]) and the fail-closed executor
//! ([`execute_pread`]) own the I/O boundary, and logical outputs are
//! assembled from physical completions by [`OutputAssembler`]. The binary
//! stays a thin CLI over the library.
#![expect(
    unused_crate_dependencies,
    reason = "`clap` is used by the binary target of this single-package application"
)]

mod budget;
mod checksum;
mod completion;
mod execution;
mod executor;
mod output;
mod plan;
mod pread;
mod range;
mod schedule;
mod scheduler;
#[cfg(test)]
mod test_support;

pub use crate::budget::{BudgetError, BudgetLimiter, ByteBudget, Reservation, ReservationError};
pub use crate::checksum::{RangeChecksum, checksum};
pub use crate::completion::CompletedRead;
pub use crate::execution::{
    ExecutionConfig, ExecutionConfigError, ExecutionPlan, ExecutionPlanError, PlannedRange,
    ReadSize, ReadSizeError,
};
pub use crate::executor::{PreadExecutionError, execute_pread};
pub use crate::output::{AssemblyError, OutputAssembler, RangeOutput};
pub use crate::plan::{PlanError, ReadPlan, coalesce};
pub use crate::pread::{ReadError, read_plan, read_scheduled};
pub use crate::range::{RangeError, ReadRange};
pub use crate::schedule::{ScheduleError, parse_schedule};
pub use crate::scheduler::{
    OperationId, ScheduleDecision, ScheduledRead, Scheduler, SchedulerError,
};
