//! Linux orchestration and TSV rendering for the fixed measurement matrix.

use std::collections::TryReserveError;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, ValueEnum};
use range_replay::{
    ByteBudget, ExecutionConfig, ExecutionConfigError, ExecutionPlan, ExecutionPlanError,
    PreadExecutionError, RangeOutput, ReadSize, ReadSizeError, UringExecutionError,
    UringQueueDepth, UringQueueDepthError, execute_pread, execute_uring,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::coalescing;
use super::matrix::{
    BACKENDS, BackendSpec, LOGICAL_BYTES, MAX_BUDGET_DEPTH, MEASURED_REPETITIONS, MatrixError,
    PlanMetrics, WARMUPS_PER_ROW, Workload, WorkloadSpec, backends_for_repetition, build_workload,
    workload_specs,
};
use super::proc_stat::{ProcStatError, ProcessCpuTicks};

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Runs the fixed warm-cache matrix against one existing data file.
#[derive(Debug, Parser)]
#[command(version)]
struct CliArgs {
    /// Linux clock ticks per second, recorded from `getconf CLK_TCK`.
    #[arg(long, value_name = "HERTZ")]
    clock_ticks_per_second: u64,

    /// Fixed experiment to execute; the comparison matrix remains the default.
    #[arg(long, value_enum, default_value_t = Experiment::Comparison)]
    experiment: Experiment,

    /// Existing regular file at least as large as every predeclared range.
    data_file: PathBuf,
}

/// One fixed terminal measurement mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum Experiment {
    /// Backend, logical-range-size, access-pattern, and queue-depth matrix.
    #[default]
    Comparison,
    /// Separate 4 KiB reads versus fixed groups of 16 with explicit over-read.
    Coalescing,
}

/// Whether an observation warms the row or belongs to the measured sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Warmup,
    Measured,
}

impl Phase {
    const fn label(self) -> &'static str {
        match self {
            Self::Warmup => "warmup",
            Self::Measured => "measured",
        }
    }
}

/// One raw observation emitted only after the whole matrix succeeds.
#[derive(Debug)]
struct Observation {
    workload: WorkloadSpec,
    backend: BackendSpec,
    phase: Phase,
    repetition: u32,
    read_size_bytes: u64,
    byte_budget_bytes: u64,
    plan: PlanMetrics,
    elapsed_ns: u128,
    user_cpu_ticks: u64,
    system_cpu_ticks: u64,
    cpu_ns: u128,
    throughput_bytes_per_second: u128,
    output_sha256: String,
}

/// One completed timed execution and its still-owned logical outputs.
struct Sample {
    observation: Observation,
    outputs: Vec<RangeOutput>,
}

/// Immutable inputs for one timed execution.
struct SampleRequest {
    workload: WorkloadSpec,
    backend: BackendSpec,
    phase: Phase,
    repetition: u32,
    read_size_bytes: u64,
    byte_budget_bytes: u64,
    plan_metrics: PlanMetrics,
    output_sha256: String,
}

/// Reason the measurement matrix could not complete and render atomically.
#[derive(Debug, Error)]
enum MeasureError {
    #[error("--clock-ticks-per-second must be greater than zero")]
    ZeroClockTicks,
    #[error("cannot open data file `{}`", path.display())]
    OpenData { path: PathBuf, source: io::Error },
    #[error("cannot inspect data file `{}`", path.display())]
    InspectData { path: PathBuf, source: io::Error },
    #[error("data path `{}` is not a regular file", path.display())]
    DataNotRegular { path: PathBuf },
    #[error(
        "data file `{}` has {actual} bytes but the matrix needs at least {required}",
        path.display()
    )]
    DataTooShort {
        path: PathBuf,
        actual: u64,
        required: u64,
    },
    #[error("cannot reserve raw observation storage")]
    ObservationReservation(#[source] TryReserveError),
    #[error("matrix row count does not fit this machine")]
    UnrepresentableRowCount(#[source] std::num::TryFromIntError),
    #[error("cannot derive byte budget for workload `{workload}`")]
    ByteBudgetOverflow { workload: &'static str },
    #[error("invalid read size for workload `{workload}`")]
    ReadSize {
        workload: &'static str,
        #[source]
        source: ReadSizeError,
    },
    #[error("invalid byte budget for workload `{workload}`")]
    ByteBudget {
        workload: &'static str,
        #[source]
        source: range_replay::BudgetError,
    },
    #[error("invalid execution configuration for workload `{workload}`")]
    Config {
        workload: &'static str,
        #[source]
        source: ExecutionConfigError,
    },
    #[error("cannot derive execution plan for workload `{workload}`")]
    ExecutionPlan {
        workload: &'static str,
        #[source]
        source: ExecutionPlanError,
    },
    #[error("pread failed for workload `{workload}`")]
    Pread {
        workload: &'static str,
        #[source]
        source: PreadExecutionError,
    },
    #[error("invalid io_uring depth {queue_depth}")]
    QueueDepth {
        queue_depth: u32,
        #[source]
        source: UringQueueDepthError,
    },
    #[error("io_uring depth {queue_depth} failed for workload `{workload}`")]
    Uring {
        workload: &'static str,
        queue_depth: u32,
        #[source]
        source: UringExecutionError,
    },
    #[error(
        "{backend} depth {queue_depth} output differs from the pread reference for workload `{workload}` repetition {repetition}"
    )]
    OutputMismatch {
        workload: &'static str,
        backend: &'static str,
        queue_depth: u32,
        repetition: u32,
    },
    #[error("elapsed time was zero for workload `{workload}`")]
    ZeroElapsed { workload: &'static str },
    #[error("throughput arithmetic overflowed for workload `{workload}`")]
    ThroughputOverflow { workload: &'static str },
    #[error("cannot write completed observations")]
    WriteOutput(#[source] io::Error),
    #[error(transparent)]
    Matrix(#[from] MatrixError),
    #[error(transparent)]
    ProcStat(#[from] ProcStatError),
    #[error(transparent)]
    Coalescing(#[from] coalescing::RunError),
}

pub(super) fn main() -> ExitCode {
    let args = CliArgs::parse();

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report(&error);
            ExitCode::FAILURE
        }
    }
}

fn run(args: &CliArgs) -> Result<(), MeasureError> {
    if args.clock_ticks_per_second == 0 {
        return Err(MeasureError::ZeroClockTicks);
    }

    if args.experiment == Experiment::Coalescing {
        return coalescing::run(&args.data_file, args.clock_ticks_per_second)
            .map_err(MeasureError::from);
    }

    let workloads = build_workloads()?;
    let required_data_bytes = workloads
        .iter()
        .map(Workload::required_data_bytes)
        .max()
        .unwrap_or(0);
    let (file, data_bytes) = open_data(&args.data_file, required_data_bytes)?;
    let observations = execute_matrix(&file, &workloads, args.clock_ticks_per_second)?;

    let mut stdout = io::BufWriter::new(io::stdout().lock());
    render_observations(
        &mut stdout,
        args,
        data_bytes,
        required_data_bytes,
        &observations,
    )?;
    stdout.flush().map_err(MeasureError::WriteOutput)
}

fn build_workloads() -> Result<Vec<Workload>, MeasureError> {
    workload_specs()
        .into_iter()
        .map(build_workload)
        .collect::<Result<Vec<_>, _>>()
        .map_err(MeasureError::from)
}

fn open_data(path: &Path, required: u64) -> Result<(File, u64), MeasureError> {
    let file = File::open(path).map_err(|source| MeasureError::OpenData {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = file
        .metadata()
        .map_err(|source| MeasureError::InspectData {
            path: path.to_path_buf(),
            source,
        })?;
    if !metadata.file_type().is_file() {
        return Err(MeasureError::DataNotRegular {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() < required {
        return Err(MeasureError::DataTooShort {
            path: path.to_path_buf(),
            actual: metadata.len(),
            required,
        });
    }

    Ok((file, metadata.len()))
}

fn execute_matrix(
    file: &File,
    workloads: &[Workload],
    clock_ticks_per_second: u64,
) -> Result<Vec<Observation>, MeasureError> {
    let backend_count =
        u64::try_from(BACKENDS.len()).map_err(MeasureError::UnrepresentableRowCount)?;
    let workload_count =
        u64::try_from(workloads.len()).map_err(MeasureError::UnrepresentableRowCount)?;
    let rows_per_workload = u64::from(WARMUPS_PER_ROW)
        .checked_add(u64::from(MEASURED_REPETITIONS))
        .and_then(|runs| runs.checked_mul(backend_count))
        .ok_or(MeasureError::ThroughputOverflow {
            workload: "matrix row count",
        })?;
    let total_rows =
        rows_per_workload
            .checked_mul(workload_count)
            .ok_or(MeasureError::ThroughputOverflow {
                workload: "matrix row count",
            })?;
    let capacity = usize::try_from(total_rows).map_err(MeasureError::UnrepresentableRowCount)?;
    let mut observations = Vec::new();
    observations
        .try_reserve_exact(capacity)
        .map_err(MeasureError::ObservationReservation)?;

    for workload in workloads {
        execute_workload(file, workload, clock_ticks_per_second, &mut observations)?;
    }

    Ok(observations)
}

fn execute_workload(
    file: &File,
    workload: &Workload,
    clock_ticks_per_second: u64,
    observations: &mut Vec<Observation>,
) -> Result<(), MeasureError> {
    let spec = workload.spec();
    let read_size =
        ReadSize::try_new(spec.range_bytes()).map_err(|source| MeasureError::ReadSize {
            workload: spec.id(),
            source,
        })?;
    let byte_budget_bytes = spec.range_bytes().checked_mul(MAX_BUDGET_DEPTH).ok_or(
        MeasureError::ByteBudgetOverflow {
            workload: spec.id(),
        },
    )?;
    let byte_budget =
        ByteBudget::try_new(byte_budget_bytes).map_err(|source| MeasureError::ByteBudget {
            workload: spec.id(),
            source,
        })?;
    let config = ExecutionConfig::try_new(read_size, byte_budget).map_err(|source| {
        MeasureError::Config {
            workload: spec.id(),
            source,
        }
    })?;
    let execution =
        ExecutionPlan::try_from_read_plan(workload.plan(), config).map_err(|source| {
            MeasureError::ExecutionPlan {
                workload: spec.id(),
                source,
            }
        })?;
    let plan_metrics = PlanMetrics::try_from_execution(&execution)?;

    let reference_request = SampleRequest {
        workload: spec,
        backend: BackendSpec::Pread,
        phase: Phase::Warmup,
        repetition: 0,
        read_size_bytes: spec.range_bytes(),
        byte_budget_bytes,
        plan_metrics,
        output_sha256: String::new(),
    };
    let mut reference = execute_sample(
        file,
        execution.clone(),
        reference_request,
        clock_ticks_per_second,
    )?;
    let reference_digest = output_digest(&reference.outputs);
    reference
        .observation
        .output_sha256
        .clone_from(&reference_digest);
    observations.push(reference.observation);

    for &backend in &BACKENDS[1..] {
        let request = SampleRequest {
            workload: spec,
            backend,
            phase: Phase::Warmup,
            repetition: 0,
            read_size_bytes: spec.range_bytes(),
            byte_budget_bytes,
            plan_metrics,
            output_sha256: reference_digest.clone(),
        };
        let sample = execute_sample(file, execution.clone(), request, clock_ticks_per_second)?;
        ensure_equal(&reference.outputs, &sample)?;
        observations.push(sample.observation);
    }

    for repetition in 1..=MEASURED_REPETITIONS {
        for backend in backends_for_repetition(repetition) {
            let request = SampleRequest {
                workload: spec,
                backend,
                phase: Phase::Measured,
                repetition,
                read_size_bytes: spec.range_bytes(),
                byte_budget_bytes,
                plan_metrics,
                output_sha256: reference_digest.clone(),
            };
            let sample = execute_sample(file, execution.clone(), request, clock_ticks_per_second)?;
            ensure_equal(&reference.outputs, &sample)?;
            observations.push(sample.observation);
        }
    }

    Ok(())
}

fn execute_sample(
    file: &File,
    execution: ExecutionPlan,
    request: SampleRequest,
    clock_ticks_per_second: u64,
) -> Result<Sample, MeasureError> {
    let cpu_start = ProcessCpuTicks::read()?;
    let wall_start = Instant::now();
    let outputs = match request.backend {
        BackendSpec::Pread => {
            execute_pread(file, execution).map_err(|source| MeasureError::Pread {
                workload: request.workload.id(),
                source,
            })?
        }
        BackendSpec::Uring { queue_depth } => {
            let depth = UringQueueDepth::try_new(queue_depth).map_err(|source| {
                MeasureError::QueueDepth {
                    queue_depth,
                    source,
                }
            })?;
            execute_uring(file, execution, depth).map_err(|source| MeasureError::Uring {
                workload: request.workload.id(),
                queue_depth,
                source,
            })?
        }
    };
    let elapsed_ns = wall_start.elapsed().as_nanos();
    let cpu_end = ProcessCpuTicks::read()?;
    if elapsed_ns == 0 {
        return Err(MeasureError::ZeroElapsed {
            workload: request.workload.id(),
        });
    }
    let cpu = cpu_start.delta_to(cpu_end)?;
    let cpu_ns = cpu.nanoseconds(clock_ticks_per_second)?;
    let throughput_bytes_per_second = u128::from(request.plan_metrics.logical_bytes)
        .checked_mul(1_000_000_000)
        .map(|scaled| scaled / elapsed_ns)
        .ok_or(MeasureError::ThroughputOverflow {
            workload: request.workload.id(),
        })?;

    Ok(Sample {
        observation: Observation {
            workload: request.workload,
            backend: request.backend,
            phase: request.phase,
            repetition: request.repetition,
            read_size_bytes: request.read_size_bytes,
            byte_budget_bytes: request.byte_budget_bytes,
            plan: request.plan_metrics,
            elapsed_ns,
            user_cpu_ticks: cpu.user,
            system_cpu_ticks: cpu.system,
            cpu_ns,
            throughput_bytes_per_second,
            output_sha256: request.output_sha256,
        },
        outputs,
    })
}

fn ensure_equal(reference: &[RangeOutput], sample: &Sample) -> Result<(), MeasureError> {
    if reference == sample.outputs {
        return Ok(());
    }

    Err(MeasureError::OutputMismatch {
        workload: sample.observation.workload.id(),
        backend: sample.observation.backend.label(),
        queue_depth: sample.observation.backend.submitted_depth(),
        repetition: sample.observation.repetition,
    })
}

fn output_digest(outputs: &[RangeOutput]) -> String {
    let mut hasher = Sha256::new();
    for output in outputs {
        hasher.update(output.range().offset().to_le_bytes());
        hasher.update(output.range().length().to_le_bytes());
        hasher.update(output.bytes());
    }

    let digest = hasher.finalize();
    let mut rendered = String::with_capacity(digest.len() * 2);
    for byte in digest {
        rendered.push(char::from(HEX[usize::from(byte >> 4)]));
        rendered.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    rendered
}

fn render_observations(
    writer: &mut impl Write,
    args: &CliArgs,
    data_bytes: u64,
    required_data_bytes: u64,
    observations: &[Observation],
) -> Result<(), MeasureError> {
    writeln!(writer, "# schema=range-replay-measurement/v1")
        .and_then(|()| writeln!(writer, "# complete=true"))
        .and_then(|()| writeln!(writer, "# cache_condition=warm_after_one_warmup_per_row"))
        .and_then(|()| writeln!(writer, "# data_file={}", args.data_file.display()))
        .and_then(|()| writeln!(writer, "# data_file_bytes={data_bytes}"))
        .and_then(|()| writeln!(writer, "# required_data_bytes={required_data_bytes}"))
        .and_then(|()| writeln!(writer, "# logical_bytes_per_workload={LOGICAL_BYTES}"))
        .and_then(|()| writeln!(writer, "# warmups_per_row={WARMUPS_PER_ROW}"))
        .and_then(|()| writeln!(writer, "# measured_repetitions={MEASURED_REPETITIONS}"))
        .and_then(|()| writeln!(writer, "# max_byte_budget_depth={MAX_BUDGET_DEPTH}"))
        .and_then(|()| {
            writeln!(
                writer,
                "# clock_ticks_per_second={}",
                args.clock_ticks_per_second
            )
        })
        .and_then(|()| {
            writeln!(
                writer,
                "workload\tpattern\tbackend\tsubmitted_depth\tphase\trepetition\tlogical_range_bytes\tread_size_bytes\tbyte_budget_bytes\tlogical_bytes\tphysical_bytes\tphysical_operations\telapsed_ns\tuser_cpu_ticks\tsystem_cpu_ticks\tcpu_ns\tthroughput_bytes_per_second\toutput_sha256\tbytes_equal_to_pread"
            )
        })
        .map_err(MeasureError::WriteOutput)?;

    for observation in observations {
        writeln!(
            writer,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\ttrue",
            observation.workload.id(),
            observation.workload.pattern().label(),
            observation.backend.label(),
            observation.backend.submitted_depth(),
            observation.phase.label(),
            observation.repetition,
            observation.workload.range_bytes(),
            observation.read_size_bytes,
            observation.byte_budget_bytes,
            observation.plan.logical_bytes,
            observation.plan.physical_bytes,
            observation.plan.operations,
            observation.elapsed_ns,
            observation.user_cpu_ticks,
            observation.system_cpu_ticks,
            observation.cpu_ns,
            observation.throughput_bytes_per_second,
            observation.output_sha256,
        )
        .map_err(MeasureError::WriteOutput)?;
    }

    Ok(())
}

fn report(error: &MeasureError) {
    eprintln!("error: {error}");

    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        eprintln!("caused by: {cause}");
        source = cause.source();
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{CliArgs, Experiment};

    #[test]
    fn cli_requires_the_clock_rate_and_data_file() {
        assert!(CliArgs::try_parse_from(["measure"]).is_err());
        let parsed = CliArgs::try_parse_from([
            "measure",
            "--clock-ticks-per-second",
            "100",
            "/tmp/data.bin",
        ])
        .expect("the required arguments are present");

        assert_eq!(parsed.experiment, Experiment::Comparison);
    }

    #[test]
    fn cli_selects_the_fixed_coalescing_experiment() {
        let parsed = CliArgs::try_parse_from([
            "measure",
            "--clock-ticks-per-second",
            "100",
            "--experiment",
            "coalescing",
            "/tmp/data.bin",
        ])
        .expect("coalescing is a fixed experiment value");

        assert_eq!(parsed.experiment, Experiment::Coalescing);
    }
}
