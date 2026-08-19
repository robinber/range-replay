//! Fixed-payload experiment comparing separate reads with bounded over-read.

use std::collections::TryReserveError;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::{self, Write};
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::time::Instant;

use range_replay::{
    BudgetError, ExecutionConfig, ExecutionConfigError, ExecutionPlan, ExecutionPlanError,
    PlanError, RangeError, RangeOutput, ReadPlan, ReadRange, ReadSize, ReadSizeError,
};
#[cfg(target_os = "linux")]
use range_replay::{
    PreadExecutionError, UringExecutionError, UringQueueDepth, UringQueueDepthError, execute_pread,
    execute_uring,
};
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::matrix::{BackendSpec, PlanMetrics};
#[cfg(target_os = "linux")]
use super::proc_stat::{ProcStatError, ProcessCpuTicks};

const USEFUL_BYTES: u64 = 256 * 1024 * 1024;
const USEFUL_BLOCK_BYTES: u64 = 4 * 1024;
const GAP_BYTES: u64 = 4 * 1024;
const BLOCKS_PER_GROUP: u64 = 16;
const GROUP_READ_BYTES: u64 =
    USEFUL_BLOCK_BYTES * BLOCKS_PER_GROUP + GAP_BYTES * (BLOCKS_PER_GROUP - 1);
const MAX_IN_FLIGHT_READS: u64 = 16;
const BYTE_BUDGET_BYTES: u64 = GROUP_READ_BYTES * MAX_IN_FLIGHT_READS;
const WARMUPS_PER_ROW: u32 = 1;
const MEASURED_REPETITIONS: u32 = 8;
#[cfg(target_os = "linux")]
const HEX: &[u8; 16] = b"0123456789abcdef";

/// One of the two fixed physical layouts over the same useful blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Every useful 4 KiB block is one physical operation with no over-read.
    Separate,
    /// One physical operation spans 16 useful blocks and the 15 gaps between
    /// them.
    Grouped16,
}

impl Layout {
    const fn label(self) -> &'static str {
        match self {
            Self::Separate => "separate_4k",
            Self::Grouped16 => "grouped_16",
        }
    }

    const fn read_size_bytes(self) -> u64 {
        match self {
            Self::Separate => USEFUL_BLOCK_BYTES,
            Self::Grouped16 => GROUP_READ_BYTES,
        }
    }
}

/// One backend/layout row in the fixed experiment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExperimentRow {
    backend: BackendSpec,
    layout: Layout,
}

const ROWS: [ExperimentRow; 4] = [
    ExperimentRow {
        backend: BackendSpec::Pread,
        layout: Layout::Separate,
    },
    ExperimentRow {
        backend: BackendSpec::Pread,
        layout: Layout::Grouped16,
    },
    ExperimentRow {
        backend: BackendSpec::Uring { queue_depth: 1 },
        layout: Layout::Separate,
    },
    ExperimentRow {
        backend: BackendSpec::Uring { queue_depth: 1 },
        layout: Layout::Grouped16,
    },
];

/// Pure projection contract used by both production values and tiny fixtures.
#[derive(Clone, Copy, Debug)]
struct ProjectionSpec {
    useful_bytes: u64,
    block_bytes: u64,
    gap_bytes: u64,
    blocks_per_group: u64,
}

const PROJECTION: ProjectionSpec = ProjectionSpec {
    useful_bytes: USEFUL_BYTES,
    block_bytes: USEFUL_BLOCK_BYTES,
    gap_bytes: GAP_BYTES,
    blocks_per_group: BLOCKS_PER_GROUP,
};

/// Complete deterministic plans and metrics for both layouts.
#[derive(Debug)]
struct ExperimentPlans {
    separate: ExecutionPlan,
    grouped: ExecutionPlan,
    separate_metrics: PlanMetrics,
    grouped_metrics: PlanMetrics,
    required_data_bytes: u64,
}

impl ExperimentPlans {
    const fn execution(&self, layout: Layout) -> &ExecutionPlan {
        match layout {
            Layout::Separate => &self.separate,
            Layout::Grouped16 => &self.grouped,
        }
    }

    const fn metrics(&self, layout: Layout) -> PlanMetrics {
        match layout {
            Layout::Separate => self.separate_metrics,
            Layout::Grouped16 => self.grouped_metrics,
        }
    }
}

/// Reason the deterministic experiment plans or useful-byte projection failed.
#[derive(Debug, Error)]
pub(super) enum ExperimentError {
    #[error("coalescing experiment arithmetic overflowed while deriving {0}")]
    ArithmeticOverflow(&'static str),
    #[error("coalescing experiment count does not fit this machine")]
    UnrepresentableCount(#[source] std::num::TryFromIntError),
    #[error("cannot reserve the {layout} schedule")]
    ScheduleReservation {
        layout: &'static str,
        #[source]
        source: TryReserveError,
    },
    #[error("cannot construct {layout} range {index}")]
    Range {
        layout: &'static str,
        index: u64,
        #[source]
        source: RangeError,
    },
    #[error("cannot canonicalize the {layout} experiment plan")]
    Plan {
        layout: &'static str,
        #[source]
        source: PlanError,
    },
    #[error("invalid {layout} read size")]
    ReadSize {
        layout: &'static str,
        #[source]
        source: ReadSizeError,
    },
    #[error("invalid shared coalescing byte budget")]
    ByteBudget(#[source] BudgetError),
    #[error("invalid {layout} execution configuration")]
    Config {
        layout: &'static str,
        #[source]
        source: ExecutionConfigError,
    },
    #[error("cannot derive the {layout} execution plan")]
    ExecutionPlan {
        layout: &'static str,
        #[source]
        source: ExecutionPlanError,
    },
    #[error("cannot derive deterministic physical metrics")]
    PlanMetrics(#[source] super::matrix::MatrixError),
    #[error("physical bytes are smaller than useful bytes for {layout}")]
    PhysicalBytesUnderflow { layout: &'static str },
    #[error("cannot reserve the reconstructed useful payload")]
    UsefulReservation(#[source] TryReserveError),
    #[error("expected {expected} {layout} outputs but received {actual}")]
    UnexpectedOutputCount {
        layout: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("{layout} output {output_index} has {actual} bytes instead of {expected}")]
    UnexpectedOutputLength {
        layout: &'static str,
        output_index: usize,
        expected: usize,
        actual: usize,
    },
    #[error("grouped output {output_index} omits useful block {block_index}")]
    MissingUsefulSlice {
        output_index: usize,
        block_index: u64,
    },
    #[error("reconstructed payload has {actual} bytes instead of {expected}")]
    UsefulLengthMismatch { expected: usize, actual: usize },
}

/// Whether an observation warms its row or belongs to the measured sample.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
enum Phase {
    Warmup,
    Measured,
}

#[cfg(target_os = "linux")]
impl Phase {
    const fn label(self) -> &'static str {
        match self {
            Self::Warmup => "warmup",
            Self::Measured => "measured",
        }
    }
}

/// One raw coalescing observation.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct Observation {
    row: ExperimentRow,
    phase: Phase,
    repetition: u32,
    metrics: PlanMetrics,
    overread_bytes: u64,
    elapsed_ns: u128,
    user_cpu_ticks: u64,
    system_cpu_ticks: u64,
    cpu_ns: u128,
    useful_throughput_bytes_per_second: u128,
    physical_throughput_bytes_per_second: u128,
    useful_sha256: String,
}

/// One completed timed sample and its exact useful logical payload.
#[cfg(target_os = "linux")]
struct Sample {
    observation: Observation,
    useful_payload: Vec<u8>,
}

/// Reason the Linux experiment could not complete and render atomically.
#[cfg(target_os = "linux")]
#[derive(Debug, Error)]
pub(super) enum RunError {
    #[error("cannot open data file `{}`", path.display())]
    OpenData { path: PathBuf, source: io::Error },
    #[error("cannot inspect data file `{}`", path.display())]
    InspectData { path: PathBuf, source: io::Error },
    #[error("data path `{}` is not a regular file", path.display())]
    DataNotRegular { path: PathBuf },
    #[error(
        "data file `{}` has {actual} bytes but the coalescing experiment needs at least {required}",
        path.display()
    )]
    DataTooShort {
        path: PathBuf,
        actual: u64,
        required: u64,
    },
    #[error("cannot reserve coalescing observations")]
    ObservationReservation(#[source] TryReserveError),
    #[error("pread failed for {layout}")]
    Pread {
        layout: &'static str,
        #[source]
        source: PreadExecutionError,
    },
    #[error("invalid io_uring depth {queue_depth}")]
    QueueDepth {
        queue_depth: u32,
        #[source]
        source: UringQueueDepthError,
    },
    #[error("io_uring depth {queue_depth} failed for {layout}")]
    Uring {
        layout: &'static str,
        queue_depth: u32,
        #[source]
        source: UringExecutionError,
    },
    #[error(
        "{backend} {layout} useful payload differs from the separate pread reference at repetition {repetition}"
    )]
    OutputMismatch {
        backend: &'static str,
        layout: &'static str,
        repetition: u32,
    },
    #[error("elapsed time was zero for {layout}")]
    ZeroElapsed { layout: &'static str },
    #[error("throughput arithmetic overflowed for {layout}")]
    ThroughputOverflow { layout: &'static str },
    #[error("cannot write completed coalescing observations")]
    WriteOutput(#[source] io::Error),
    #[error(transparent)]
    Experiment(#[from] ExperimentError),
    #[error(transparent)]
    ProcStat(#[from] ProcStatError),
}

/// Runs the complete fixed coalescing experiment and emits TSV after success.
#[cfg(target_os = "linux")]
pub(super) fn run(data_path: &Path, clock_ticks_per_second: u64) -> Result<(), RunError> {
    let plans = build_experiment()?;
    let (file, data_bytes) = open_data(data_path, plans.required_data_bytes)?;
    let observations = execute_experiment(&file, &plans, clock_ticks_per_second)?;

    let mut stdout = io::BufWriter::new(io::stdout().lock());
    render_observations(
        &mut stdout,
        data_path,
        data_bytes,
        plans.required_data_bytes,
        clock_ticks_per_second,
        &observations,
    )?;
    stdout.flush().map_err(RunError::WriteOutput)
}

fn build_experiment() -> Result<ExperimentPlans, ExperimentError> {
    let block_count = exact_quotient(USEFUL_BYTES, USEFUL_BLOCK_BYTES, "useful block count")?;
    let group_count = exact_quotient(block_count, BLOCKS_PER_GROUP, "group count")?;
    let separate_schedule = build_separate_schedule(block_count)?;
    let grouped_schedule = build_grouped_schedule(group_count)?;
    let separate_plan = ReadPlan::try_from_schedule(&separate_schedule).map_err(|source| {
        ExperimentError::Plan {
            layout: Layout::Separate.label(),
            source,
        }
    })?;
    let grouped_plan =
        ReadPlan::try_from_schedule(&grouped_schedule).map_err(|source| ExperimentError::Plan {
            layout: Layout::Grouped16.label(),
            source,
        })?;
    let byte_budget = range_replay::ByteBudget::try_new(BYTE_BUDGET_BYTES)
        .map_err(ExperimentError::ByteBudget)?;
    let separate = execution_for(&separate_plan, Layout::Separate, byte_budget)?;
    let grouped = execution_for(&grouped_plan, Layout::Grouped16, byte_budget)?;
    let separate_metrics =
        PlanMetrics::try_from_execution(&separate).map_err(ExperimentError::PlanMetrics)?;
    let grouped_metrics =
        PlanMetrics::try_from_execution(&grouped).map_err(ExperimentError::PlanMetrics)?;
    let required_data_bytes = grouped_plan.ranges().last().map_or(0, ReadRange::end);

    Ok(ExperimentPlans {
        separate,
        grouped,
        separate_metrics,
        grouped_metrics,
        required_data_bytes,
    })
}

fn exact_quotient(dividend: u64, divisor: u64, name: &'static str) -> Result<u64, ExperimentError> {
    if divisor == 0 || !dividend.is_multiple_of(divisor) {
        return Err(ExperimentError::ArithmeticOverflow(name));
    }

    dividend
        .checked_div(divisor)
        .ok_or(ExperimentError::ArithmeticOverflow(name))
}

fn build_separate_schedule(block_count: u64) -> Result<Vec<ReadRange>, ExperimentError> {
    let capacity = usize::try_from(block_count).map_err(ExperimentError::UnrepresentableCount)?;
    let mut schedule = Vec::new();
    schedule.try_reserve_exact(capacity).map_err(|source| {
        ExperimentError::ScheduleReservation {
            layout: Layout::Separate.label(),
            source,
        }
    })?;
    let stride = USEFUL_BLOCK_BYTES
        .checked_add(GAP_BYTES)
        .ok_or(ExperimentError::ArithmeticOverflow("separate stride"))?;

    for index in 0..block_count {
        let offset = index
            .checked_mul(stride)
            .ok_or(ExperimentError::ArithmeticOverflow("separate offset"))?;
        let range = ReadRange::try_new(offset, USEFUL_BLOCK_BYTES).map_err(|source| {
            ExperimentError::Range {
                layout: Layout::Separate.label(),
                index,
                source,
            }
        })?;
        schedule.push(range);
    }

    Ok(schedule)
}

fn build_grouped_schedule(group_count: u64) -> Result<Vec<ReadRange>, ExperimentError> {
    let capacity = usize::try_from(group_count).map_err(ExperimentError::UnrepresentableCount)?;
    let mut schedule = Vec::new();
    schedule.try_reserve_exact(capacity).map_err(|source| {
        ExperimentError::ScheduleReservation {
            layout: Layout::Grouped16.label(),
            source,
        }
    })?;
    let useful_stride = USEFUL_BLOCK_BYTES
        .checked_add(GAP_BYTES)
        .ok_or(ExperimentError::ArithmeticOverflow("useful stride"))?;
    let group_stride = useful_stride
        .checked_mul(BLOCKS_PER_GROUP)
        .ok_or(ExperimentError::ArithmeticOverflow("group stride"))?;

    for index in 0..group_count {
        let offset = index
            .checked_mul(group_stride)
            .ok_or(ExperimentError::ArithmeticOverflow("group offset"))?;
        let range = ReadRange::try_new(offset, GROUP_READ_BYTES).map_err(|source| {
            ExperimentError::Range {
                layout: Layout::Grouped16.label(),
                index,
                source,
            }
        })?;
        schedule.push(range);
    }

    Ok(schedule)
}

fn execution_for(
    plan: &ReadPlan,
    layout: Layout,
    byte_budget: range_replay::ByteBudget,
) -> Result<ExecutionPlan, ExperimentError> {
    let read_size = ReadSize::try_new(layout.read_size_bytes()).map_err(|source| {
        ExperimentError::ReadSize {
            layout: layout.label(),
            source,
        }
    })?;
    let config = ExecutionConfig::try_new(read_size, byte_budget).map_err(|source| {
        ExperimentError::Config {
            layout: layout.label(),
            source,
        }
    })?;

    ExecutionPlan::try_from_read_plan(plan, config).map_err(|source| {
        ExperimentError::ExecutionPlan {
            layout: layout.label(),
            source,
        }
    })
}

fn project_useful(
    layout: Layout,
    outputs: &[RangeOutput],
    projection: ProjectionSpec,
) -> Result<Vec<u8>, ExperimentError> {
    let capacity =
        usize::try_from(projection.useful_bytes).map_err(ExperimentError::UnrepresentableCount)?;
    let expected_outputs = match layout {
        Layout::Separate => exact_quotient(
            projection.useful_bytes,
            projection.block_bytes,
            "separate output count",
        )?,
        Layout::Grouped16 => {
            let blocks = exact_quotient(
                projection.useful_bytes,
                projection.block_bytes,
                "projection block count",
            )?;
            exact_quotient(blocks, projection.blocks_per_group, "grouped output count")?
        }
    };
    let expected_outputs =
        usize::try_from(expected_outputs).map_err(ExperimentError::UnrepresentableCount)?;
    if outputs.len() != expected_outputs {
        return Err(ExperimentError::UnexpectedOutputCount {
            layout: layout.label(),
            expected: expected_outputs,
            actual: outputs.len(),
        });
    }

    let mut useful = Vec::new();
    useful
        .try_reserve_exact(capacity)
        .map_err(ExperimentError::UsefulReservation)?;
    let block_bytes =
        usize::try_from(projection.block_bytes).map_err(ExperimentError::UnrepresentableCount)?;

    for (output_index, output) in outputs.iter().enumerate() {
        match layout {
            Layout::Separate => {
                if output.bytes().len() != block_bytes {
                    return Err(ExperimentError::UnexpectedOutputLength {
                        layout: layout.label(),
                        output_index,
                        expected: block_bytes,
                        actual: output.bytes().len(),
                    });
                }
                useful.extend_from_slice(output.bytes());
            }
            Layout::Grouped16 => {
                project_grouped_output(&mut useful, output, output_index, projection)?;
            }
        }
    }

    if useful.len() != capacity {
        return Err(ExperimentError::UsefulLengthMismatch {
            expected: capacity,
            actual: useful.len(),
        });
    }

    Ok(useful)
}

fn project_grouped_output(
    useful: &mut Vec<u8>,
    output: &RangeOutput,
    output_index: usize,
    projection: ProjectionSpec,
) -> Result<(), ExperimentError> {
    let stride = projection
        .block_bytes
        .checked_add(projection.gap_bytes)
        .ok_or(ExperimentError::ArithmeticOverflow("projection stride"))?;
    let intervening_gap_count =
        projection
            .blocks_per_group
            .checked_sub(1)
            .ok_or(ExperimentError::ArithmeticOverflow(
                "grouped intervening gap count",
            ))?;
    let expected_length = projection
        .block_bytes
        .checked_mul(projection.blocks_per_group)
        .and_then(|useful_bytes| {
            projection
                .gap_bytes
                .checked_mul(intervening_gap_count)
                .and_then(|gap_bytes| useful_bytes.checked_add(gap_bytes))
        })
        .ok_or(ExperimentError::ArithmeticOverflow("grouped output length"))?;
    let expected_length =
        usize::try_from(expected_length).map_err(ExperimentError::UnrepresentableCount)?;
    if output.bytes().len() != expected_length {
        return Err(ExperimentError::UnexpectedOutputLength {
            layout: Layout::Grouped16.label(),
            output_index,
            expected: expected_length,
            actual: output.bytes().len(),
        });
    }

    for block_index in 0..projection.blocks_per_group {
        let start = block_index
            .checked_mul(stride)
            .ok_or(ExperimentError::ArithmeticOverflow("useful slice start"))?;
        let end = start
            .checked_add(projection.block_bytes)
            .ok_or(ExperimentError::ArithmeticOverflow("useful slice end"))?;
        let start = usize::try_from(start).map_err(ExperimentError::UnrepresentableCount)?;
        let end = usize::try_from(end).map_err(ExperimentError::UnrepresentableCount)?;
        let slice = output
            .bytes()
            .get(start..end)
            .ok_or(ExperimentError::MissingUsefulSlice {
                output_index,
                block_index,
            })?;
        useful.extend_from_slice(slice);
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn open_data(path: &Path, required: u64) -> Result<(File, u64), RunError> {
    let file = File::open(path).map_err(|source| RunError::OpenData {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| RunError::InspectData {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(RunError::DataNotRegular {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() < required {
        return Err(RunError::DataTooShort {
            path: path.to_path_buf(),
            actual: metadata.len(),
            required,
        });
    }

    Ok((file, metadata.len()))
}

#[cfg(target_os = "linux")]
fn execute_experiment(
    file: &File,
    plans: &ExperimentPlans,
    clock_ticks_per_second: u64,
) -> Result<Vec<Observation>, RunError> {
    let row_count = u64::try_from(ROWS.len()).map_err(ExperimentError::UnrepresentableCount)?;
    let observation_count = u64::from(WARMUPS_PER_ROW)
        .checked_add(u64::from(MEASURED_REPETITIONS))
        .and_then(|runs| runs.checked_mul(row_count))
        .ok_or(ExperimentError::ArithmeticOverflow("observation count"))?;
    let capacity =
        usize::try_from(observation_count).map_err(ExperimentError::UnrepresentableCount)?;
    let mut observations = Vec::new();
    observations
        .try_reserve_exact(capacity)
        .map_err(RunError::ObservationReservation)?;

    let mut reference = execute_sample(
        file,
        plans,
        ROWS[0],
        Phase::Warmup,
        0,
        clock_ticks_per_second,
        String::new(),
    )?;
    let reference_digest = useful_digest(&reference.useful_payload);
    reference
        .observation
        .useful_sha256
        .clone_from(&reference_digest);
    observations.push(reference.observation);

    for &row in &ROWS[1..] {
        let sample = execute_sample(
            file,
            plans,
            row,
            Phase::Warmup,
            0,
            clock_ticks_per_second,
            reference_digest.clone(),
        )?;
        ensure_equal(&reference.useful_payload, &sample)?;
        observations.push(sample.observation);
    }

    for repetition in 1..=MEASURED_REPETITIONS {
        for row in rows_for_repetition(repetition) {
            let sample = execute_sample(
                file,
                plans,
                row,
                Phase::Measured,
                repetition,
                clock_ticks_per_second,
                reference_digest.clone(),
            )?;
            ensure_equal(&reference.useful_payload, &sample)?;
            observations.push(sample.observation);
        }
    }

    Ok(observations)
}

#[cfg(target_os = "linux")]
fn execute_sample(
    file: &File,
    plans: &ExperimentPlans,
    row: ExperimentRow,
    phase: Phase,
    repetition: u32,
    clock_ticks_per_second: u64,
    useful_sha256: String,
) -> Result<Sample, RunError> {
    let execution = plans.execution(row.layout).clone();
    let metrics = plans.metrics(row.layout);
    let overread_bytes = metrics.physical_bytes.checked_sub(USEFUL_BYTES).ok_or(
        ExperimentError::PhysicalBytesUnderflow {
            layout: row.layout.label(),
        },
    )?;
    let cpu_start = ProcessCpuTicks::read()?;
    let wall_start = Instant::now();
    let outputs = match row.backend {
        BackendSpec::Pread => execute_pread(file, execution).map_err(|source| RunError::Pread {
            layout: row.layout.label(),
            source,
        })?,
        BackendSpec::Uring { queue_depth } => {
            let depth =
                UringQueueDepth::try_new(queue_depth).map_err(|source| RunError::QueueDepth {
                    queue_depth,
                    source,
                })?;
            execute_uring(file, execution, depth).map_err(|source| RunError::Uring {
                layout: row.layout.label(),
                queue_depth,
                source,
            })?
        }
    };
    let useful_payload = project_useful(row.layout, &outputs, PROJECTION)?;
    drop(outputs);
    let elapsed_ns = wall_start.elapsed().as_nanos();
    let cpu_end = ProcessCpuTicks::read()?;
    if elapsed_ns == 0 {
        return Err(RunError::ZeroElapsed {
            layout: row.layout.label(),
        });
    }
    let cpu = cpu_start.delta_to(cpu_end)?;
    let cpu_ns = cpu.nanoseconds(clock_ticks_per_second)?;
    let useful_throughput_bytes_per_second = throughput(USEFUL_BYTES, elapsed_ns, row.layout)?;
    let physical_throughput_bytes_per_second =
        throughput(metrics.physical_bytes, elapsed_ns, row.layout)?;

    Ok(Sample {
        observation: Observation {
            row,
            phase,
            repetition,
            metrics,
            overread_bytes,
            elapsed_ns,
            user_cpu_ticks: cpu.user,
            system_cpu_ticks: cpu.system,
            cpu_ns,
            useful_throughput_bytes_per_second,
            physical_throughput_bytes_per_second,
            useful_sha256,
        },
        useful_payload,
    })
}

#[cfg(target_os = "linux")]
fn throughput(bytes: u64, elapsed_ns: u128, layout: Layout) -> Result<u128, RunError> {
    u128::from(bytes)
        .checked_mul(1_000_000_000)
        .map(|scaled| scaled / elapsed_ns)
        .ok_or(RunError::ThroughputOverflow {
            layout: layout.label(),
        })
}

#[cfg(target_os = "linux")]
fn ensure_equal(reference: &[u8], sample: &Sample) -> Result<(), RunError> {
    if reference == sample.useful_payload {
        return Ok(());
    }

    Err(RunError::OutputMismatch {
        backend: sample.observation.row.backend.label(),
        layout: sample.observation.row.layout.label(),
        repetition: sample.observation.repetition,
    })
}

#[cfg(target_os = "linux")]
fn useful_digest(payload: &[u8]) -> String {
    let digest = Sha256::digest(payload);
    let mut rendered = String::with_capacity(digest.len() * 2);
    for byte in digest {
        rendered.push(char::from(HEX[usize::from(byte >> 4)]));
        rendered.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    rendered
}

const fn rows_for_repetition(repetition: u32) -> [ExperimentRow; 4] {
    match repetition.saturating_sub(1) % 4 {
        0 => [ROWS[0], ROWS[1], ROWS[2], ROWS[3]],
        1 => [ROWS[1], ROWS[2], ROWS[3], ROWS[0]],
        2 => [ROWS[2], ROWS[3], ROWS[0], ROWS[1]],
        _ => [ROWS[3], ROWS[0], ROWS[1], ROWS[2]],
    }
}

#[cfg(target_os = "linux")]
fn render_observations(
    writer: &mut impl Write,
    data_path: &Path,
    data_bytes: u64,
    required_data_bytes: u64,
    clock_ticks_per_second: u64,
    observations: &[Observation],
) -> Result<(), RunError> {
    writeln!(writer, "# schema=range-replay-coalescing/v1")
        .and_then(|()| writeln!(writer, "# complete=true"))
        .and_then(|()| writeln!(writer, "# cache_condition=warm_after_one_warmup_per_row"))
        .and_then(|()| writeln!(writer, "# timing=backend_execute_project_and_drop"))
        .and_then(|()| writeln!(writer, "# data_file={}", data_path.display()))
        .and_then(|()| writeln!(writer, "# data_file_bytes={data_bytes}"))
        .and_then(|()| writeln!(writer, "# required_data_bytes={required_data_bytes}"))
        .and_then(|()| writeln!(writer, "# useful_bytes={USEFUL_BYTES}"))
        .and_then(|()| writeln!(writer, "# useful_block_bytes={USEFUL_BLOCK_BYTES}"))
        .and_then(|()| writeln!(writer, "# gap_bytes={GAP_BYTES}"))
        .and_then(|()| writeln!(writer, "# grouped_blocks={BLOCKS_PER_GROUP}"))
        .and_then(|()| writeln!(writer, "# shared_byte_budget={BYTE_BUDGET_BYTES}"))
        .and_then(|()| writeln!(writer, "# warmups_per_row={WARMUPS_PER_ROW}"))
        .and_then(|()| writeln!(writer, "# measured_repetitions={MEASURED_REPETITIONS}"))
        .and_then(|()| writeln!(writer, "# clock_ticks_per_second={clock_ticks_per_second}"))
        .and_then(|()| {
            writeln!(
                writer,
                "backend\tsubmitted_depth\tlayout\tphase\trepetition\tuseful_block_bytes\tgap_bytes\tblocks_per_physical_read\tread_size_bytes\tbyte_budget_bytes\tuseful_bytes\tphysical_bytes\toverread_bytes\tphysical_operations\telapsed_ns\tuser_cpu_ticks\tsystem_cpu_ticks\tcpu_ns\tuseful_throughput_bytes_per_second\tphysical_throughput_bytes_per_second\tuseful_sha256\tbytes_equal_to_reference"
            )
        })
        .map_err(RunError::WriteOutput)?;

    for observation in observations {
        let blocks_per_read = match observation.row.layout {
            Layout::Separate => 1,
            Layout::Grouped16 => BLOCKS_PER_GROUP,
        };
        writeln!(
            writer,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\ttrue",
            observation.row.backend.label(),
            observation.row.backend.submitted_depth(),
            observation.row.layout.label(),
            observation.phase.label(),
            observation.repetition,
            USEFUL_BLOCK_BYTES,
            GAP_BYTES,
            blocks_per_read,
            observation.row.layout.read_size_bytes(),
            BYTE_BUDGET_BYTES,
            USEFUL_BYTES,
            observation.metrics.physical_bytes,
            observation.overread_bytes,
            observation.metrics.operations,
            observation.elapsed_ns,
            observation.user_cpu_ticks,
            observation.system_cpu_ticks,
            observation.cpu_ns,
            observation.useful_throughput_bytes_per_second,
            observation.physical_throughput_bytes_per_second,
            observation.useful_sha256,
        )
        .map_err(RunError::WriteOutput)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write as _;

    use range_replay::{ReadPlan, ReadRange, read_plan};

    use super::{
        BYTE_BUDGET_BYTES, ExperimentRow, GAP_BYTES, GROUP_READ_BYTES, Layout,
        MEASURED_REPETITIONS, PlanMetrics, ProjectionSpec, ROWS, USEFUL_BLOCK_BYTES, USEFUL_BYTES,
        build_experiment, project_useful, rows_for_repetition,
    };

    #[test]
    fn fixed_plans_express_the_hand_calculated_tradeoff() {
        let plans = build_experiment().expect("fixed plans are valid");

        assert_eq!(USEFUL_BYTES, 268_435_456);
        assert_eq!(USEFUL_BLOCK_BYTES, 4096);
        assert_eq!(GAP_BYTES, 4096);
        assert_eq!(GROUP_READ_BYTES, 126_976);
        assert_eq!(BYTE_BUDGET_BYTES, 2_031_616);
        assert_eq!(MEASURED_REPETITIONS, 8);
        assert_eq!(
            plans.separate_metrics,
            PlanMetrics {
                logical_bytes: 268_435_456,
                physical_bytes: 268_435_456,
                operations: 65_536,
            }
        );
        assert_eq!(
            plans.grouped_metrics,
            PlanMetrics {
                logical_bytes: 520_093_696,
                physical_bytes: 520_093_696,
                operations: 4096,
            }
        );
        assert_eq!(
            plans.grouped_metrics.physical_bytes - USEFUL_BYTES,
            251_658_240
        );
        assert_eq!(plans.required_data_bytes, 536_866_816);
    }

    #[test]
    fn tiny_projection_returns_equal_useful_bytes_for_both_layouts() {
        let path = std::env::temp_dir().join(format!(
            "range-replay-coalescing-projection-{}",
            std::process::id()
        ));
        File::create_new(&path)
            .and_then(|mut file| file.write_all(b"abXXcd"))
            .expect("the unique fixture file is writable");
        let file = File::open(&path).expect("the fixture file reopens");
        let separate_plan = ReadPlan::try_from_schedule(&[
            ReadRange::try_new(0, 2).expect("fixture range is valid"),
            ReadRange::try_new(4, 2).expect("fixture range is valid"),
        ])
        .expect("separate fixture plan is valid");
        let grouped_plan = ReadPlan::try_from_schedule(&[
            ReadRange::try_new(0, 6).expect("fixture range is valid")
        ])
        .expect("grouped fixture plan is valid");
        let projection = ProjectionSpec {
            useful_bytes: 4,
            block_bytes: 2,
            gap_bytes: 2,
            blocks_per_group: 2,
        };

        let separate = read_plan(&file, &separate_plan).expect("separate fixture reads");
        let grouped = read_plan(&file, &grouped_plan).expect("grouped fixture reads");

        assert_eq!(
            project_useful(Layout::Separate, &separate, projection)
                .expect("separate projection succeeds"),
            b"abcd"
        );
        assert_eq!(
            project_useful(Layout::Grouped16, &grouped, projection)
                .expect("grouped projection succeeds"),
            b"abcd"
        );

        std::fs::remove_file(path).expect("fixture cleanup succeeds");
    }

    #[test]
    fn row_order_balances_every_position_over_four_repetitions() {
        for position in 0..4 {
            let observed = [1, 2, 3, 4].map(|repetition| rows_for_repetition(repetition)[position]);

            assert!(ROWS.into_iter().all(|row| observed.contains(&row)));
        }
    }

    #[test]
    fn rows_keep_backend_depth_one_while_only_layout_changes() {
        assert_eq!(
            ROWS,
            [
                ExperimentRow {
                    backend: super::BackendSpec::Pread,
                    layout: Layout::Separate,
                },
                ExperimentRow {
                    backend: super::BackendSpec::Pread,
                    layout: Layout::Grouped16,
                },
                ExperimentRow {
                    backend: super::BackendSpec::Uring { queue_depth: 1 },
                    layout: Layout::Separate,
                },
                ExperimentRow {
                    backend: super::BackendSpec::Uring { queue_depth: 1 },
                    layout: Layout::Grouped16,
                },
            ]
        );
    }
}
