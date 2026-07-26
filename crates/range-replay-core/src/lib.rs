//! Pure domain types and planning for `range-replay`.
//!
//! This crate owns schedule validation, range coalescing, typed errors, and
//! deterministic planning. Keep filesystem and `io_uring` adapters out of
//! this boundary.

/// Placeholder so the empty crate documents its intended ownership boundary.
#[must_use]
pub const fn crate_name() -> &'static str {
    "range-replay-core"
}
