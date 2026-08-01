//! Educational library for planning and executing file-range read schedules.
//!
//! Modules will grow here as the project takes shape. Keep the binary thin.
#![expect(
    unused_crate_dependencies,
    reason = "`clap` is used by the binary target of this single-package application"
)]

mod checksum;
mod execution;
mod plan;
mod pread;
mod range;
mod schedule;

pub use crate::checksum::{RangeChecksum, checksum};
pub use crate::execution::{
    BudgetError, ByteBudget, ExecutionPlan, ExecutionPlanError, PlannedRange,
};
pub use crate::plan::{PlanError, ReadPlan, coalesce};
pub use crate::pread::{RangeOutput, ReadError, read_plan};
pub use crate::range::{RangeError, ReadRange};
pub use crate::schedule::{ScheduleError, parse_schedule};
