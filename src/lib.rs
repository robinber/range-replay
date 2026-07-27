//! Educational library for planning and executing file-range read schedules.
//!
//! Modules will grow here as the project takes shape. Keep the binary thin.

mod plan;
mod range;
mod schedule;

pub use crate::plan::{PlanError, coalesce};
pub use crate::range::{RangeError, ReadRange};
pub use crate::schedule::{ScheduleError, parse_schedule};
