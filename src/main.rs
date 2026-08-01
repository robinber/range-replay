//! Thin entrypoint for the `range-replay` binary.
//!
//! `main` stays glue: parse arguments, run the library pipeline, and render
//! the outcome. Domain behavior lives in the library crate; this file only
//! wires filesystem inputs into it and presents the result.
//!
//! The rendering contract is fail-closed: nothing reaches stdout unless the
//! whole plan executed successfully, so any domain output visible on stdout
//! came from a completely successful run.
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
    PlanError, RangeOutput, ReadError, ReadPlan, ReadRange, ScheduleError, parse_schedule,
    read_plan,
};
use thiserror::Error;

/// Replays a textual read schedule against a data file and prints each
/// canonical range as `offset,length,hex`.
#[derive(Debug, Parser)]
#[command(version)]
struct CliArgs {
    /// File whose bytes the schedule reads.
    data_file: PathBuf,

    /// UTF-8 schedule with one `offset,length` line per requested range.
    schedule_file: PathBuf,
}

/// Reason a parsed command could not produce its output.
///
/// Every variant names the failing stage and carries the underlying failure
/// as its source, so diagnostics keep both the actionable context (which file,
/// which stage) and the root cause.
#[derive(Debug, Error)]
enum CliError {
    #[error("cannot read schedule file `{}`", path.display())]
    ReadSchedule { path: PathBuf, source: io::Error },
    #[error("invalid schedule in `{}`", path.display())]
    ParseSchedule {
        path: PathBuf,
        source: ScheduleError,
    },
    #[error("cannot plan schedule from `{}`", path.display())]
    Plan { path: PathBuf, source: PlanError },
    #[error("cannot open data file `{}`", path.display())]
    OpenData { path: PathBuf, source: io::Error },
    #[error("cannot execute the read plan against `{}`", path.display())]
    ExecutePlan { path: PathBuf, source: ReadError },
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

/// Runs schedule reading, parsing, planning, and backend execution.
///
/// The data file is opened only after the schedule has produced a valid
/// non-empty canonical plan, so schedule failures are reported even when the
/// data path is unusable.
fn execute(args: &CliArgs) -> Result<Vec<RangeOutput>, CliError> {
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

    let data_file = File::open(&args.data_file).map_err(|source| CliError::OpenData {
        path: args.data_file.clone(),
        source,
    })?;

    read_plan(&data_file, &plan).map_err(|source| CliError::ExecutePlan {
        path: args.data_file.clone(),
        source,
    })
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
