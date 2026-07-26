//! Thin entrypoint for the `range-replay` binary.
//!
//! Behavior will live in testable modules. This entrypoint stays glue only.

#![expect(
    unused_crate_dependencies,
    reason = "binary targets receive every package dependency; this entrypoint only drives the range_replay_cli library"
)]

fn main() {
    println!("{}", range_replay_cli::crate_name());
}
