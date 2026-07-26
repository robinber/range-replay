//! Educational library for planning and executing file-range read schedules.
//!
//! Modules will grow here as the project takes shape. Keep the binary thin.

mod range;

pub use crate::range::{RangeError, ReadRange};

/// Placeholder so the empty package documents its crate root.
#[must_use]
pub const fn crate_name() -> &'static str {
    "range-replay"
}
