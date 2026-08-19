//! Fixed logical workloads and backend rows for the terminal comparison.

use std::collections::TryReserveError;

use range_replay::{ExecutionPlan, ExecutionPlanError, PlanError, RangeError, ReadPlan, ReadRange};
use thiserror::Error;

pub(super) const LOGICAL_BYTES: u64 = 256 * 1024 * 1024;
pub(super) const WARMUPS_PER_ROW: u32 = 1;
pub(super) const MEASURED_REPETITIONS: u32 = 8;
pub(super) const MAX_BUDGET_DEPTH: u64 = 16;

const SCATTER_SLOT_MULTIPLIER: u64 = 4;
const RANGE_GAP_BYTES: u64 = 1;

/// One predeclared logical access pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AccessPattern {
    /// Ascending ranges separated by the smallest gap that prevents coalescing.
    MostlySequential,
    /// Deterministic ranges distributed across four times the logical span.
    Scattered,
}

impl AccessPattern {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::MostlySequential => "mostly_sequential",
            Self::Scattered => "scattered",
        }
    }
}

/// One backend row in the fixed comparison matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BackendSpec {
    /// Synchronous positioned reads; only one kernel read executes at a time.
    Pread,
    /// Linux `io_uring` with the stated submission queue depth.
    Uring { queue_depth: u32 },
}

impl BackendSpec {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Pread => "pread",
            Self::Uring { .. } => "io_uring",
        }
    }

    pub(super) const fn submitted_depth(self) -> u32 {
        match self {
            Self::Pread => 1,
            Self::Uring { queue_depth } => queue_depth,
        }
    }
}

pub(super) const BACKENDS: [BackendSpec; 4] = [
    BackendSpec::Pread,
    BackendSpec::Uring { queue_depth: 1 },
    BackendSpec::Uring { queue_depth: 4 },
    BackendSpec::Uring { queue_depth: 16 },
];

/// Immutable declaration of one logical workload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WorkloadSpec {
    id: &'static str,
    pattern: AccessPattern,
    range_bytes: u64,
}

impl WorkloadSpec {
    pub(super) const fn id(self) -> &'static str {
        self.id
    }

    pub(super) const fn pattern(self) -> AccessPattern {
        self.pattern
    }

    pub(super) const fn range_bytes(self) -> u64 {
        self.range_bytes
    }
}

/// One generated, canonical logical workload.
#[derive(Debug)]
pub(super) struct Workload {
    spec: WorkloadSpec,
    plan: ReadPlan,
}

impl Workload {
    pub(super) const fn spec(&self) -> WorkloadSpec {
        self.spec
    }

    pub(super) const fn plan(&self) -> &ReadPlan {
        &self.plan
    }

    pub(super) fn required_data_bytes(&self) -> u64 {
        self.plan.ranges().last().map_or(0, ReadRange::end)
    }
}

/// Deterministic physical work derived from one execution plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PlanMetrics {
    pub(super) logical_bytes: u64,
    pub(super) physical_bytes: u64,
    pub(super) operations: u64,
}

impl PlanMetrics {
    pub(super) fn try_from_execution(plan: &ExecutionPlan) -> Result<Self, MatrixError> {
        let mut logical_bytes = 0_u64;
        let mut physical_bytes = 0_u64;
        let mut operations = 0_u64;

        for planned in plan.ranges() {
            logical_bytes = logical_bytes
                .checked_add(planned.logical_range().length())
                .ok_or(MatrixError::MetricOverflow("logical bytes"))?;
            operations = operations
                .checked_add(planned.operation_count())
                .ok_or(MatrixError::MetricOverflow("physical operation count"))?;

            for operation_index in 0..planned.operation_count() {
                let physical = planned
                    .physical_read(operation_index)?
                    .ok_or(MatrixError::MissingPhysicalRead { operation_index })?;
                physical_bytes = physical_bytes
                    .checked_add(physical.length())
                    .ok_or(MatrixError::MetricOverflow("physical bytes"))?;
            }
        }

        Ok(Self {
            logical_bytes,
            physical_bytes,
            operations,
        })
    }
}

/// Reason the fixed matrix could not be constructed exactly.
#[derive(Debug, Error)]
pub(super) enum MatrixError {
    #[error("matrix arithmetic overflowed while deriving {0}")]
    ArithmeticOverflow(&'static str),
    #[error("logical byte count is not divisible by range size {range_bytes}")]
    IndivisibleLogicalBytes { range_bytes: u64 },
    #[error("workload range count does not fit this machine")]
    UnrepresentableRangeCount(#[source] std::num::TryFromIntError),
    #[error("cannot reserve the workload schedule")]
    ScheduleReservation(#[source] TryReserveError),
    #[error("cannot construct workload range {index}")]
    Range {
        index: u64,
        #[source]
        source: RangeError,
    },
    #[error("cannot canonicalize workload `{workload}`")]
    Plan {
        workload: &'static str,
        #[source]
        source: PlanError,
    },
    #[error("execution plan metric overflowed while summing {0}")]
    MetricOverflow(&'static str),
    #[error("execution plan omitted physical operation {operation_index}")]
    MissingPhysicalRead { operation_index: u64 },
    #[error(transparent)]
    ExecutionPlan(#[from] ExecutionPlanError),
}

/// Returns the complete predeclared workload axis.
pub(super) const fn workload_specs() -> [WorkloadSpec; 6] {
    [
        WorkloadSpec {
            id: "sequential_4k",
            pattern: AccessPattern::MostlySequential,
            range_bytes: 4 * 1024,
        },
        WorkloadSpec {
            id: "scattered_4k",
            pattern: AccessPattern::Scattered,
            range_bytes: 4 * 1024,
        },
        WorkloadSpec {
            id: "sequential_64k",
            pattern: AccessPattern::MostlySequential,
            range_bytes: 64 * 1024,
        },
        WorkloadSpec {
            id: "scattered_64k",
            pattern: AccessPattern::Scattered,
            range_bytes: 64 * 1024,
        },
        WorkloadSpec {
            id: "sequential_1m",
            pattern: AccessPattern::MostlySequential,
            range_bytes: 1024 * 1024,
        },
        WorkloadSpec {
            id: "scattered_1m",
            pattern: AccessPattern::Scattered,
            range_bytes: 1024 * 1024,
        },
    ]
}

/// Builds the canonical plan for one workload declaration.
pub(super) fn build_workload(spec: WorkloadSpec) -> Result<Workload, MatrixError> {
    let range_count = LOGICAL_BYTES
        .checked_div(spec.range_bytes)
        .ok_or(MatrixError::ArithmeticOverflow("range count"))?;
    if !LOGICAL_BYTES.is_multiple_of(spec.range_bytes) {
        return Err(MatrixError::IndivisibleLogicalBytes {
            range_bytes: spec.range_bytes,
        });
    }

    let capacity = usize::try_from(range_count).map_err(MatrixError::UnrepresentableRangeCount)?;
    let mut schedule = Vec::new();
    schedule
        .try_reserve_exact(capacity)
        .map_err(MatrixError::ScheduleReservation)?;

    for index in 0..range_count {
        let offset = workload_offset(spec, index, range_count)?;
        let range = ReadRange::try_new(offset, spec.range_bytes)
            .map_err(|source| MatrixError::Range { index, source })?;
        schedule.push(range);
    }

    let plan = ReadPlan::try_from_schedule(&schedule).map_err(|source| MatrixError::Plan {
        workload: spec.id,
        source,
    })?;

    Ok(Workload { spec, plan })
}

/// Rotates execution order over a complete four-repetition cycle.
pub(super) const fn backends_for_repetition(repetition: u32) -> [BackendSpec; 4] {
    match repetition.saturating_sub(1) % 4 {
        0 => [BACKENDS[0], BACKENDS[1], BACKENDS[2], BACKENDS[3]],
        1 => [BACKENDS[1], BACKENDS[2], BACKENDS[3], BACKENDS[0]],
        2 => [BACKENDS[2], BACKENDS[3], BACKENDS[0], BACKENDS[1]],
        _ => [BACKENDS[3], BACKENDS[0], BACKENDS[1], BACKENDS[2]],
    }
}

fn workload_offset(spec: WorkloadSpec, index: u64, range_count: u64) -> Result<u64, MatrixError> {
    let slot_width = spec
        .range_bytes
        .checked_add(RANGE_GAP_BYTES)
        .ok_or(MatrixError::ArithmeticOverflow("slot width"))?;
    let slot_index = match spec.pattern {
        AccessPattern::MostlySequential => index,
        AccessPattern::Scattered => {
            let slot_count = range_count
                .checked_mul(SCATTER_SLOT_MULTIPLIER)
                .ok_or(MatrixError::ArithmeticOverflow("scatter slot count"))?;
            let stride = range_count
                .checked_mul(2)
                .and_then(|value| value.checked_add(1))
                .ok_or(MatrixError::ArithmeticOverflow("scatter stride"))?;

            index
                .checked_mul(stride)
                .map(|value| value % slot_count)
                .ok_or(MatrixError::ArithmeticOverflow("scatter slot index"))?
        }
    };

    slot_index
        .checked_mul(slot_width)
        .ok_or(MatrixError::ArithmeticOverflow("range offset"))
}

#[cfg(test)]
mod tests {
    use range_replay::{ByteBudget, ExecutionConfig, ExecutionPlan, ReadPlan, ReadRange, ReadSize};

    use super::{
        BACKENDS, LOGICAL_BYTES, MAX_BUDGET_DEPTH, MEASURED_REPETITIONS, PlanMetrics,
        WARMUPS_PER_ROW, backends_for_repetition, build_workload, workload_specs,
    };

    #[test]
    fn matrix_axes_are_fixed_and_bounded() {
        let specs = workload_specs();
        let ids: Vec<&str> = specs.into_iter().map(super::WorkloadSpec::id).collect();

        assert_eq!(
            ids,
            vec![
                "sequential_4k",
                "scattered_4k",
                "sequential_64k",
                "scattered_64k",
                "sequential_1m",
                "scattered_1m",
            ]
        );
        assert_eq!(LOGICAL_BYTES, 268_435_456);
        assert_eq!(MAX_BUDGET_DEPTH, 16);
        assert_eq!(WARMUPS_PER_ROW, 1);
        assert_eq!(MEASURED_REPETITIONS, 8);
        assert_eq!(
            BACKENDS.map(super::BackendSpec::submitted_depth),
            [1, 1, 4, 16]
        );
    }

    #[test]
    fn sequential_workload_keeps_one_byte_gaps_and_exact_logical_bytes() {
        let workload = build_workload(workload_specs()[0]).expect("fixed workload is valid");
        let ranges = workload.plan().ranges();

        assert_eq!(ranges.len(), 65_536);
        assert_eq!(
            ranges[0],
            ReadRange::try_new(0, 4096).expect("fixture is valid")
        );
        assert_eq!(ranges[1].offset(), 4097);
        assert_eq!(
            ranges.iter().map(ReadRange::length).sum::<u64>(),
            LOGICAL_BYTES
        );
        assert!(
            ranges
                .windows(2)
                .all(|pair| pair[0].end() < pair[1].offset())
        );
    }

    #[test]
    fn scattered_workload_is_deterministic_unique_and_wider_than_sequential() {
        let scattered = build_workload(workload_specs()[1]).expect("fixed workload is valid");
        let repeated = build_workload(workload_specs()[1]).expect("fixed workload is repeatable");
        let sequential = build_workload(workload_specs()[0]).expect("fixed workload is valid");

        assert_eq!(scattered.plan().ranges(), repeated.plan().ranges());
        assert_eq!(scattered.plan().ranges().len(), 65_536);
        assert!(scattered.required_data_bytes() > sequential.required_data_bytes());
        assert!(
            scattered
                .plan()
                .ranges()
                .windows(2)
                .all(|pair| pair[0].end() < pair[1].offset())
        );
    }

    #[test]
    fn plan_metrics_count_the_hand_calculated_physical_reads() {
        let plan = ReadPlan::try_from_schedule(&[
            ReadRange::try_new(0, 10).expect("fixture is valid"),
            ReadRange::try_new(20, 5).expect("fixture is valid"),
        ])
        .expect("fixture plan is valid");
        let config = ExecutionConfig::try_new(
            ReadSize::try_new(4).expect("fixture is valid"),
            ByteBudget::try_new(16).expect("fixture is valid"),
        )
        .expect("fixture configuration is valid");
        let execution =
            ExecutionPlan::try_from_read_plan(&plan, config).expect("fixture execution is valid");

        assert_eq!(
            PlanMetrics::try_from_execution(&execution).expect("fixture metrics are valid"),
            PlanMetrics {
                logical_bytes: 15,
                physical_bytes: 15,
                operations: 5,
            }
        );
    }

    #[test]
    fn backend_order_balances_every_position_over_four_repetitions() {
        for position in 0..4 {
            let observed =
                [1, 2, 3, 4].map(|repetition| backends_for_repetition(repetition)[position]);

            assert!(
                BACKENDS
                    .into_iter()
                    .all(|backend| observed.contains(&backend))
            );
        }
    }
}
