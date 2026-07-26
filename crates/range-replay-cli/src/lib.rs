//! CLI surface and composition for the `range-replay` binary.
//!
//! Keep argument parsing, report rendering, and backend selection here.
//! Domain validation and planning belong in `range-replay-core`.

use range_replay_core as _;

/// Placeholder so the empty crate documents its intended ownership boundary.
#[must_use]
pub const fn crate_name() -> &'static str {
    "range-replay-cli"
}
