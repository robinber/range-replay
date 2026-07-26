//! Thin entrypoint for the `range-replay` binary.
//!
//! Behavior will live in the library crate. This entrypoint stays glue only.
#![expect(
    unused_crate_dependencies,
    reason = "`thiserror` is used by the library target of this single-package application"
)]

fn main() {
    println!("{}", range_replay::crate_name());
}
