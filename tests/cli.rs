//! End-to-end tests for the `range-replay` binary.
//!
//! Every test spawns the compiled binary against small temporary fixtures and
//! asserts the deterministic stdout contract, the fail-closed error contract,
//! or both.
#![expect(
    unused_crate_dependencies,
    reason = "the CLI is exercised through the compiled binary, not through library links"
)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::{env, fs, process};

const DATA: &[u8] = b"0123456789abcdef";

/// A temporary fixture file removed when the test ends.
struct Fixture {
    path: PathBuf,
}

impl Fixture {
    #[expect(
        clippy::expect_used,
        reason = "test helpers panic with diagnostics like the tests they serve"
    )]
    fn new(test: &str, name: &str, contents: &[u8]) -> Self {
        let path = fixture_path(test, name);
        fs::write(&path, contents).expect("fixture file is writable");

        Self { path }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _removed = fs::remove_file(&self.path);
    }
}

fn fixture_path(test: &str, name: &str) -> PathBuf {
    env::temp_dir().join(format!("range-replay-cli-{test}-{name}-{}", process::id()))
}

#[expect(
    clippy::expect_used,
    reason = "test helpers panic with diagnostics like the tests they serve"
)]
fn run_cli(args: &[&OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_range-replay"))
        .args(args)
        .output()
        .expect("the compiled binary runs")
}

fn run_on(data: &Path, schedule: &Path) -> Output {
    run_cli(&[data.as_os_str(), schedule.as_os_str()])
}

#[expect(
    clippy::expect_used,
    reason = "test helpers panic with diagnostics like the tests they serve"
)]
fn stderr_text(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("diagnostics are UTF-8")
}

#[test]
fn help_describes_both_positional_files() {
    let output = run_cli(&[OsStr::new("--help")]);

    let stdout = String::from_utf8(output.stdout).expect("help is UTF-8");
    assert!(output.status.success());
    assert!(stdout.contains("<DATA_FILE>"));
    assert!(stdout.contains("<SCHEDULE_FILE>"));
}

#[test]
fn version_prints_the_package_version() {
    let output = run_cli(&[OsStr::new("--version")]);

    let stdout = String::from_utf8(output.stdout).expect("version is UTF-8");
    assert!(output.status.success());
    assert_eq!(
        stdout.trim_end(),
        concat!("range-replay ", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn missing_arguments_fail_with_a_usage_diagnostic() {
    let output = run_cli(&[]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr_text(&output).contains("Usage"));
}

#[test]
fn a_missing_schedule_argument_fails_with_a_usage_diagnostic() {
    let output = run_cli(&[OsStr::new("only-one-path")]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr_text(&output).contains("Usage"));
}

#[test]
fn an_extra_argument_fails_with_a_usage_diagnostic() {
    let output = run_cli(&[OsStr::new("a"), OsStr::new("b"), OsStr::new("c")]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr_text(&output).contains("Usage"));
}

#[test]
fn the_hand_calculated_fixture_prints_exact_canonical_output() {
    let data = Fixture::new("hand-calculated", "data", DATA);
    let schedule = Fixture::new("hand-calculated", "schedule", b"10,4\n2,3\n");

    let output = run_on(&data.path, &schedule.path);

    assert!(output.status.success());
    assert_eq!(output.stdout, b"2,3,323334\n10,4,61626364\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn arbitrary_binary_bytes_render_as_exact_lowercase_hex() {
    let data = Fixture::new("binary", "data", &[0x41, 0x0a, 0x00, 0xff]);
    let schedule = Fixture::new("binary", "schedule", b"0,4\n");

    let output = run_on(&data.path, &schedule.path);

    assert!(output.status.success());
    assert_eq!(output.stdout, b"0,4,410a00ff\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn an_invalid_schedule_fails_with_empty_stdout_and_line_context() {
    let data = Fixture::new("invalid-schedule", "data", DATA);
    let schedule = Fixture::new("invalid-schedule", "schedule", b"10,4\nabc,5\n");

    let output = run_on(&data.path, &schedule.path);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains(&schedule.path.display().to_string()));
    assert!(stderr.contains("line 2"));
    assert!(stderr.contains("invalid offset"));
}

#[test]
fn a_non_utf8_schedule_fails_with_empty_stdout_and_path_context() {
    let data = Fixture::new("non-utf8-schedule", "data", DATA);
    let schedule = Fixture::new("non-utf8-schedule", "schedule", &[0xff, 0xfe, 0x0a]);

    let output = run_on(&data.path, &schedule.path);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("cannot read schedule file"));
    assert!(stderr.contains(&schedule.path.display().to_string()));
}

#[test]
fn an_empty_schedule_fails_before_the_backend_runs() {
    let missing_data = fixture_path("empty-schedule", "missing-data");
    let schedule = Fixture::new("empty-schedule", "schedule", b"\n  \n");

    let output = run_on(&missing_data, &schedule.path);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("cannot plan an empty schedule"));
    assert!(!stderr.contains(&missing_data.display().to_string()));
}

#[test]
fn a_later_range_crossing_eof_fails_closed_with_empty_stdout() {
    let data = Fixture::new("fail-closed", "data", DATA);
    let schedule = Fixture::new("fail-closed", "schedule", b"0,4\n100,4\n");

    let output = run_on(&data.path, &schedule.path);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("unexpected end of file"));
}

#[test]
fn a_missing_schedule_path_keeps_path_aware_context() {
    let data = Fixture::new("missing-schedule", "data", DATA);
    let missing_schedule = fixture_path("missing-schedule", "missing");

    let output = run_on(&data.path, &missing_schedule);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("cannot read schedule file"));
    assert!(stderr.contains(&missing_schedule.display().to_string()));
}

#[test]
fn a_missing_data_path_keeps_path_aware_context() {
    let missing_data = fixture_path("missing-data", "missing");
    let schedule = Fixture::new("missing-data", "schedule", b"0,4\n");

    let output = run_on(&missing_data, &schedule.path);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("cannot open data file"));
    assert!(stderr.contains(&missing_data.display().to_string()));
}
