//! Thin entrypoint for the `range-replay` binary.
//!
//! Behavior will live in the library crate. This entrypoint stays glue only.

fn main() {
    println!("{}", range_replay::crate_name());
}
