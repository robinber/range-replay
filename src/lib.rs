//! Educational library for planning and executing file-range read schedules.
//!
//! Modules will grow here as the project takes shape. Keep the binary thin.
#![expect(
    unused_crate_dependencies,
    reason = "`clap` is used by the binary target of this single-package application"
)]

mod budget;
mod checksum;
mod completion;
mod execution;
mod output;
mod plan;
mod pread;
mod range;
mod schedule;
mod scheduler;

pub use crate::budget::{BudgetError, BudgetLimiter, ByteBudget, Reservation, ReservationError};
pub use crate::checksum::{RangeChecksum, checksum};
pub use crate::completion::CompletedRead;
pub use crate::execution::{
    ExecutionConfig, ExecutionConfigError, ExecutionPlan, ExecutionPlanError, PlannedRange,
    ReadSize, ReadSizeError,
};
pub use crate::output::{AssemblyError, OutputAssembler, RangeOutput};
pub use crate::plan::{PlanError, ReadPlan, coalesce};
pub use crate::pread::{ReadError, read_plan, read_scheduled};
pub use crate::range::{RangeError, ReadRange};
pub use crate::schedule::{ScheduleError, parse_schedule};
pub use crate::scheduler::{
    OperationId, ScheduleDecision, ScheduledRead, Scheduler, SchedulerError,
};
