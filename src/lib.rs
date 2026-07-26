//! Educational library for planning and executing file-range read schedules.
//!
//! Modules will grow here as the project takes shape. Keep the binary thin.

mod range;

pub use crate::range::{RangeError, ReadRange};
