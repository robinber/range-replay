//! End-to-end tests for the `range-replay` binary.
//!
//! Every test spawns the compiled binary against small temporary fixtures and
//! asserts the deterministic stdout contract, the fail-closed error contract,
//! or both.
//!
//! The precedence tests assert which failure gets reported when several
//! stages could fail: the typed cause on stderr plus the absence of both
//! unusable paths proves reporting precedence. The stronger claim that no
//! filesystem access happened at all is a property of the statement order
//! in the binary's `execute`, confirmed by inspection, not observable from
//! the process boundary.
//!
//! `ExecutionPlan` derivation failures have no CLI test: every
//! `ExecutionPlanError` variant guards arithmetic that cannot overflow for
//! inputs already validated by `ReadPlan` and `ReadSize`, or a fallible
//! metadata allocation, so none is deterministically reachable through the
//! binary without artificial production hooks. The library tests own those
//! guards.
#![expect(
    unused_crate_dependencies,
    reason = "the CLI is exercised through the compiled binary, not through library links"
)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::{env, fs, process};

const DATA: &[u8] = b"0123456789abcdef";

/// Default valid configuration used by tests that exercise behavior other
/// than the configuration options themselves.
const READ_SIZE: &str = "4";
const BYTE_BUDGET: &str = "10";

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

/// Runs the binary with an explicit configuration and both file paths.
fn run_configured(read_size: &str, byte_budget: &str, data: &Path, schedule: &Path) -> Output {
    run_cli(&[
        OsStr::new("--read-size"),
        OsStr::new(read_size),
        OsStr::new("--byte-budget"),
        OsStr::new(byte_budget),
        data.as_os_str(),
        schedule.as_os_str(),
    ])
}

/// Runs the binary with the default valid configuration.
fn run_on(data: &Path, schedule: &Path) -> Output {
    run_configured(READ_SIZE, BYTE_BUDGET, data, schedule)
}

#[expect(
    clippy::expect_used,
    reason = "test helpers panic with diagnostics like the tests they serve"
)]
fn stderr_text(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("diagnostics are UTF-8")
}

#[test]
fn help_describes_positional_files_and_required_byte_options() {
    let output = run_cli(&[OsStr::new("--help")]);

    let stdout = String::from_utf8(output.stdout).expect("help is UTF-8");
    assert!(output.status.success());
    assert!(stdout.contains("<DATA_FILE>"));
    assert!(stdout.contains("<SCHEDULE_FILE>"));
    assert!(stdout.contains("--read-size <OCTETS>"));
    assert!(stdout.contains("--byte-budget <OCTETS>"));
    assert_eq!(
        stdout.matches("decimal byte count").count(),
        2,
        "both options document their decimal byte contract"
    );
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
fn omitting_read_size_fails_with_a_usage_diagnostic() {
    let output = run_cli(&[
        OsStr::new("--byte-budget"),
        OsStr::new(BYTE_BUDGET),
        OsStr::new("data"),
        OsStr::new("schedule"),
    ]);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("Usage"));
    assert!(stderr.contains("--read-size"));
}

#[test]
fn omitting_byte_budget_fails_with_a_usage_diagnostic() {
    let output = run_cli(&[
        OsStr::new("--read-size"),
        OsStr::new(READ_SIZE),
        OsStr::new("data"),
        OsStr::new("schedule"),
    ]);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("Usage"));
    assert!(stderr.contains("--byte-budget"));
}

#[test]
fn a_non_decimal_read_size_is_rejected_before_run() {
    let output = run_cli(&[
        OsStr::new("--read-size"),
        OsStr::new("4KiB"),
        OsStr::new("--byte-budget"),
        OsStr::new("16384"),
        OsStr::new("data"),
        OsStr::new("schedule"),
    ]);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("invalid value '4KiB'"));
    assert!(stderr.contains("--read-size"));
}

#[test]
fn a_non_decimal_byte_budget_is_rejected_before_run() {
    let output = run_cli(&[
        OsStr::new("--read-size"),
        OsStr::new("4"),
        OsStr::new("--byte-budget"),
        OsStr::new("0x10"),
        OsStr::new("data"),
        OsStr::new("schedule"),
    ]);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("invalid value '0x10'"));
    assert!(stderr.contains("--byte-budget"));
}

#[test]
fn a_value_outside_u64_is_rejected_before_run() {
    let output = run_cli(&[
        OsStr::new("--read-size"),
        OsStr::new("18446744073709551616"),
        OsStr::new("--byte-budget"),
        OsStr::new("18446744073709551616"),
        OsStr::new("data"),
        OsStr::new("schedule"),
    ]);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("invalid value '18446744073709551616'"));
}

#[test]
fn a_missing_schedule_argument_fails_with_a_usage_diagnostic() {
    let output = run_cli(&[
        OsStr::new("--read-size"),
        OsStr::new(READ_SIZE),
        OsStr::new("--byte-budget"),
        OsStr::new(BYTE_BUDGET),
        OsStr::new("only-one-path"),
    ]);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("Usage"));
    assert!(stderr.contains("<SCHEDULE_FILE>"));
}

#[test]
fn an_extra_argument_fails_with_a_usage_diagnostic() {
    let output = run_cli(&[
        OsStr::new("--read-size"),
        OsStr::new(READ_SIZE),
        OsStr::new("--byte-budget"),
        OsStr::new(BYTE_BUDGET),
        OsStr::new("a"),
        OsStr::new("b"),
        OsStr::new("c"),
    ]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr_text(&output).contains("Usage"));
}

#[test]
fn a_zero_read_size_takes_precedence_over_missing_data_and_schedule_paths() {
    let missing_data = fixture_path("zero-read-size", "missing-data");
    let missing_schedule = fixture_path("zero-read-size", "missing-schedule");

    let output = run_configured("0", "10", &missing_data, &missing_schedule);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("invalid --read-size value 0"));
    assert!(stderr.contains("read size must be greater than zero"));
    assert!(!stderr.contains(&missing_data.display().to_string()));
    assert!(!stderr.contains(&missing_schedule.display().to_string()));
}

#[test]
fn a_zero_byte_budget_takes_precedence_over_missing_data_and_schedule_paths() {
    let missing_data = fixture_path("zero-byte-budget", "missing-data");
    let missing_schedule = fixture_path("zero-byte-budget", "missing-schedule");

    let output = run_configured("4", "0", &missing_data, &missing_schedule);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("invalid --byte-budget value 0"));
    assert!(stderr.contains("byte budget must be greater than zero"));
    assert!(!stderr.contains(&missing_data.display().to_string()));
    assert!(!stderr.contains(&missing_schedule.display().to_string()));
}

#[test]
fn a_read_size_above_the_physical_maximum_is_rejected_before_any_file_access() {
    let missing_data = fixture_path("max-read-size", "missing-data");
    let missing_schedule = fixture_path("max-read-size", "missing-schedule");

    let output = run_configured("1073741825", "1073741825", &missing_data, &missing_schedule);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("invalid --read-size value 1073741825"));
    assert!(
        stderr.contains("read size of 1073741825 bytes exceeds the maximum of 1073741824 bytes")
    );
    assert!(!stderr.contains(&missing_data.display().to_string()));
    assert!(!stderr.contains(&missing_schedule.display().to_string()));
}

#[test]
fn a_read_size_exceeding_the_budget_reports_the_exact_values_and_wins_over_missing_paths() {
    let missing_data = fixture_path("oversized-read-size", "missing-data");
    let missing_schedule = fixture_path("oversized-read-size", "missing-schedule");

    let output = run_configured("16", "8", &missing_data, &missing_schedule);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("invalid execution configuration"));
    assert!(stderr.contains("read size of 16 bytes exceeds the byte budget of 8 bytes"));
    assert!(!stderr.contains(&missing_data.display().to_string()));
    assert!(!stderr.contains(&missing_schedule.display().to_string()));
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
fn different_valid_configurations_produce_identical_logical_output() {
    let data = Fixture::new("config-invariant", "data", DATA);
    let schedule = Fixture::new("config-invariant", "schedule", b"10,4\n2,3\n");

    for (read_size, byte_budget) in [("1", "1"), ("2", "5"), ("4", "10"), ("1024", "4096")] {
        let output = run_configured(read_size, byte_budget, &data.path, &schedule.path);

        assert!(
            output.status.success(),
            "configuration {read_size}/{byte_budget} succeeds"
        );
        assert_eq!(
            output.stdout, b"2,3,323334\n10,4,61626364\n",
            "configuration {read_size}/{byte_budget} preserves the logical output"
        );
        assert!(output.stderr.is_empty());
    }
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
fn overlapping_and_adjacent_ranges_render_one_coalesced_line() {
    let data = Fixture::new("coalesce", "data", DATA);
    let schedule = Fixture::new("coalesce", "schedule", b"2,2\n0,2\n1,1\n");

    let output = run_on(&data.path, &schedule.path);

    assert!(output.status.success());
    assert_eq!(output.stdout, b"0,4,30313233\n");
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
    assert!(stderr.contains("invalid digit found in string"));
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
fn a_later_physical_read_crossing_eof_fails_closed_with_empty_stdout() {
    let data = Fixture::new("fail-closed", "data", DATA);
    let schedule = Fixture::new("fail-closed", "schedule", b"0,4\n100,4\n");

    let output = run_on(&data.path, &schedule.path);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("cannot execute the physical plan"));
    assert!(stderr.contains(&data.path.display().to_string()));
    assert!(stderr.contains("executing one positioned read failed"));
    assert!(stderr.contains("unexpected end of file"));
}

#[test]
fn a_missing_schedule_path_takes_precedence_over_a_missing_data_path() {
    let missing_data = fixture_path("missing-schedule", "missing-data");
    let missing_schedule = fixture_path("missing-schedule", "missing");

    let output = run_on(&missing_data, &missing_schedule);

    let stderr = stderr_text(&output);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("cannot read schedule file"));
    assert!(stderr.contains(&missing_schedule.display().to_string()));
    assert!(!stderr.contains(&missing_data.display().to_string()));
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
