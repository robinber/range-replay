//! Linux-only entrypoint for the bounded `v0.1` comparison matrix.

#![cfg_attr(
    not(target_os = "linux"),
    expect(
        unused_crate_dependencies,
        reason = "the binary dependencies are consumed only by the Linux measurement implementation"
    )
)]
#![cfg_attr(
    target_os = "linux",
    expect(
        unused_crate_dependencies,
        reason = "the io-uring dependency is consumed through the range_replay library facade"
    )
)]

use std::process::ExitCode;

#[cfg(target_os = "linux")]
mod app;
#[cfg(any(target_os = "linux", test))]
#[cfg_attr(
    not(target_os = "linux"),
    expect(
        dead_code,
        reason = "portable coalescing tests compile without their Linux runtime consumer"
    )
)]
mod coalescing;
#[cfg(any(target_os = "linux", test))]
#[cfg_attr(
    not(target_os = "linux"),
    expect(
        dead_code,
        reason = "portable matrix tests compile without their Linux runtime consumer"
    )
)]
mod matrix;
#[cfg(any(target_os = "linux", test))]
#[cfg_attr(
    not(target_os = "linux"),
    expect(
        dead_code,
        reason = "portable proc-stat parser tests compile without their Linux runtime consumer"
    )
)]
mod proc_stat;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    app::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("error: range-replay-measure requires Linux with io_uring support");
    ExitCode::FAILURE
}
