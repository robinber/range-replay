//! Thin entrypoint for the `range-replay` binary.
//!
//! `main` stays glue: parse arguments, validate the execution configuration
//! through the library constructors, run the library pipeline, and render the
//! outcome. Domain behavior lives in the library crate; this file only wires
//! filesystem inputs into it and presents the result.
//!
//! Clap owns syntax only — presence and decimal `u64` shape of the options.
//! Semantic invariants (non-zero read size and budget, read size not
//! exceeding the budget) belong to [`ReadSize`], [`ByteBudget`], and
//! [`ExecutionConfig`], and are checked before any filesystem I/O, so an
//! invalid configuration wins deterministically over an unreadable path.
//!
//! The rendering contract is fail-closed: nothing reaches stdout unless the
//! whole physical plan executed successfully through [`execute_pread`], so
//! any domain output visible on stdout came from a completely successful run.
#![expect(
    unused_crate_dependencies,
    reason = "`sha2` is used by the library target of this single-package application"
)]

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use range_replay::{
    BudgetError, ByteBudget, ExecutionConfig, ExecutionConfigError, ExecutionPlan,
    ExecutionPlanError, PlanError, PreadExecutionError, RangeOutput, ReadPlan, ReadRange, ReadSize,
    ReadSizeError, ScheduleError, execute_pread, parse_schedule,
};
use thiserror::Error;

/// Replays a textual read schedule against a data file and prints each
/// canonical range as `offset,length,hex`.
#[derive(Debug, Parser)]
#[command(version)]
struct CliArgs {
    /// Maximum length of one physical read, as a required decimal byte count.
    #[arg(long, value_name = "OCTETS")]
    read_size: u64,

    /// Total in-flight byte capacity, as a required decimal byte count.
    #[arg(long, value_name = "OCTETS")]
    byte_budget: u64,

    /// File whose bytes the schedule reads.
    data_file: PathBuf,

    /// UTF-8 schedule with one `offset,length` line per requested range.
    schedule_file: PathBuf,
}

/// Reason a parsed command could not produce its output.
///
/// Every variant names the failing stage and carries the underlying failure
/// as its source, so diagnostics keep both the actionable context (which
/// value, which file, which stage) and the root cause.
#[derive(Debug, Error)]
enum CliError {
    #[error("invalid --read-size value {read_size}")]
    ReadSize {
        read_size: u64,
        source: ReadSizeError,
    },
    #[error("invalid --byte-budget value {byte_budget}")]
    ByteBudget {
        byte_budget: u64,
        source: BudgetError,
    },
    #[error("invalid execution configuration")]
    Config { source: ExecutionConfigError },
    #[error("cannot read schedule file `{}`", path.display())]
    ReadSchedule { path: PathBuf, source: io::Error },
    #[error("invalid schedule in `{}`", path.display())]
    ParseSchedule {
        path: PathBuf,
        source: ScheduleError,
    },
    #[error("cannot plan schedule from `{}`", path.display())]
    Plan { path: PathBuf, source: PlanError },
    #[error("cannot derive the physical plan for `{}`", path.display())]
    DeriveExecutionPlan {
        path: PathBuf,
        source: ExecutionPlanError,
    },
    #[error("cannot open data file `{}`", path.display())]
    OpenData { path: PathBuf, source: io::Error },
    #[error("cannot execute the physical plan against `{}`", path.display())]
    ExecutePlan {
        path: PathBuf,
        source: PreadExecutionError,
    },
    #[error("cannot write output to stdout")]
    WriteStdout { source: io::Error },
}

fn main() -> ExitCode {
    let args = CliArgs::parse();

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report(&error);
            ExitCode::FAILURE
        }
    }
}

/// Executes the whole pipeline, rendering only after complete success.
fn run(args: &CliArgs) -> Result<(), CliError> {
    let outputs = execute(args)?;

    let mut stdout = io::BufWriter::new(io::stdout().lock());
    render_outputs(&mut stdout, &outputs)
        .and_then(|()| stdout.flush())
        .map_err(|source| CliError::WriteStdout { source })
}

/// Runs configuration validation, schedule planning, and backend execution.
///
/// The semantic configuration is validated before any filesystem I/O, so an
/// invalid read size, budget, or pairing is reported even when both paths
/// are unusable. The data file is opened only after every pure planning step
/// has produced a valid compact physical plan, so schedule and planning
/// failures are reported even when the data path is unusable.
fn execute(args: &CliArgs) -> Result<Vec<RangeOutput>, CliError> {
    let config = validate_config(args)?;

    let schedule_text =
        fs::read_to_string(&args.schedule_file).map_err(|source| CliError::ReadSchedule {
            path: args.schedule_file.clone(),
            source,
        })?;
    let schedule = parse_schedule(&schedule_text).map_err(|source| CliError::ParseSchedule {
        path: args.schedule_file.clone(),
        source,
    })?;
    let plan = ReadPlan::try_from_schedule(&schedule).map_err(|source| CliError::Plan {
        path: args.schedule_file.clone(),
        source,
    })?;
    let execution = ExecutionPlan::try_from_read_plan(&plan, config).map_err(|source| {
        CliError::DeriveExecutionPlan {
            path: args.schedule_file.clone(),
            source,
        }
    })?;

    let data_file = File::open(&args.data_file).map_err(|source| CliError::OpenData {
        path: args.data_file.clone(),
        source,
    })?;

    execute_pread(&data_file, execution).map_err(|source| CliError::ExecutePlan {
        path: args.data_file.clone(),
        source,
    })
}

/// Builds the validated execution configuration from the raw option values.
///
/// Delegates every semantic rule to the library constructors instead of
/// duplicating them in the binary.
fn validate_config(args: &CliArgs) -> Result<ExecutionConfig, CliError> {
    let read_size = ReadSize::try_new(args.read_size).map_err(|source| CliError::ReadSize {
        read_size: args.read_size,
        source,
    })?;
    let byte_budget =
        ByteBudget::try_new(args.byte_budget).map_err(|source| CliError::ByteBudget {
            byte_budget: args.byte_budget,
            source,
        })?;

    ExecutionConfig::try_new(read_size, byte_budget).map_err(|source| CliError::Config { source })
}

/// Writes one `offset,length,hex` line per output, in plan order.
fn render_outputs(writer: &mut impl Write, outputs: &[RangeOutput]) -> io::Result<()> {
    for output in outputs {
        render_range_line(writer, output.range(), output.bytes())?;
    }

    Ok(())
}

/// Writes a single `offset,length,hex` line.
///
/// Every byte renders as exactly two lowercase, zero-padded hexadecimal
/// characters; payload bytes are never interpreted as text.
fn render_range_line(writer: &mut impl Write, range: ReadRange, bytes: &[u8]) -> io::Result<()> {
    write!(writer, "{},{},", range.offset(), range.length())?;

    for byte in bytes {
        write!(writer, "{byte:02x}")?;
    }

    writeln!(writer)
}

/// Reports an error and its full source chain on stderr.
fn report(error: &CliError) {
    eprintln!("error: {error}");

    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        eprintln!("caused by: {cause}");
        source = cause.source();
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use range_replay::ReadRange;

    use super::render_range_line;

    fn range(offset: u64, length: u64) -> ReadRange {
        ReadRange::try_new(offset, length).expect("test ranges are valid")
    }

    #[test]
    fn render_range_line_writes_two_lowercase_hex_digits_per_byte() {
        let mut rendered = Vec::new();

        render_range_line(&mut rendered, range(0, 4), &[0x41, 0x0a, 0x00, 0xff])
            .expect("writing to a vector cannot fail");

        assert_eq!(rendered, b"0,4,410a00ff\n");
    }

    #[test]
    fn render_range_line_zero_pads_and_keeps_high_bytes_lowercase() {
        let mut rendered = Vec::new();

        render_range_line(&mut rendered, range(7, 3), &[0x00, 0x0f, 0xab])
            .expect("writing to a vector cannot fail");

        assert_eq!(rendered, b"7,3,000fab\n");
    }

    #[test]
    fn render_range_line_propagates_writer_errors() {
        struct FailingWriter;

        impl Write for FailingWriter {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let error = render_range_line(&mut FailingWriter, range(0, 1), &[0x41])
            .expect_err("the writer rejects every write");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
