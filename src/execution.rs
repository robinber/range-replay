//! Budget-derived physical execution planning over a validated logical plan.
//!
//! A [`ReadPlan`] is the canonical *logical* request: which bytes must be
//! read, independent of any backend, machine, or budget. An [`ExecutionPlan`]
//! is the *physical* plan derived from one logical plan and one
//! [`ByteBudget`]: the same bytes, split into the concrete read operations
//! whose sizes never exceed the budget. The two stay separate types because
//! the same logical range needs different physical reads under different
//! budgets, and a runtime tuning input must never change the established
//! meaning of the logical plan.
//!
//! Splitting is deterministic and greedy: each logical range is covered from
//! its start offset by the largest read that exceeds neither the bytes
//! remaining in the range nor the budget. Every physical read except a
//! possible final tail therefore has exactly the budget's length. For every
//! planned range the physical reads are non-empty, ordered by ascending
//! offset, exactly adjacent with neither gap nor overlap, and cover every
//! logical byte exactly once.
//!
//! This module is planning only. Nothing here reads a file, tracks bytes
//! actually in flight, releases budget on completion, or schedules work;
//! those remain later slices.

use std::collections::TryReserveError;

use thiserror::Error;

use crate::plan::ReadPlan;
use crate::range::ReadRange;

/// Reason a [`ByteBudget`] could not be constructed.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum BudgetError {
    /// The requested budget was `0`, so no non-empty read could ever fit.
    #[error("byte budget must be greater than zero")]
    ZeroBudget,
}

/// Reason an [`ExecutionPlan`] could not be derived.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ExecutionPlanError {
    /// The number of physical reads covering one logical range is not
    /// representable as a collection capacity on this platform.
    #[error(
        "range [{offset}, {end}) at budget {budget} needs more physical reads than a plan can hold"
    )]
    UnrepresentableOperationCount {
        /// Inclusive start offset of the logical range being split.
        offset: u64,
        /// Exclusive end offset of the logical range being split.
        end: u64,
        /// Byte budget the split had to respect.
        budget: u64,
    },
    /// The metadata allocation for planned entries could not be reserved.
    ///
    /// Physical reads are materialized eagerly, so their metadata capacity is
    /// reserved fallibly before any entry is produced; a plan is either built
    /// completely or fails with this variant instead of aborting mid-way.
    #[error("cannot reserve plan capacity for {capacity} entries")]
    ReservationFailed {
        /// Number of entries whose reservation failed.
        capacity: usize,
        /// Allocator failure reported by the reservation.
        #[source]
        source: TryReserveError,
    },
    /// Splitting produced bounds that no [`ReadRange`] can represent.
    ///
    /// Greedy splitting of a validated range under a non-zero budget always
    /// yields valid reads, so this variant guards the splitting arithmetic
    /// instead of describing a reachable input. Reporting it keeps the split
    /// free of a panic path and free of any silent correction that would
    /// return a plan with wrong coverage.
    #[error("splitting range [{offset}, {end}) at budget {budget} produced an invalid read")]
    UnrepresentableSplit {
        /// Offset at which the invalid physical read would have started.
        offset: u64,
        /// Exclusive end offset of the logical range being split.
        end: u64,
        /// Byte budget the split had to respect.
        budget: u64,
    },
}

/// A validated, non-zero limit on the size of one physical read.
///
/// A budget of `0` is rejected at construction rather than treated as
/// temporary backpressure: no non-empty read could ever fit under it, so a
/// zero budget can never admit any work and is an invalid configuration
/// instead of a momentarily full one.
///
/// `ByteBudget` is [`Copy`] because it is immutable configuration: copying a
/// limit cannot multiply any capacity. A future runtime *reservation* guard
/// is the opposite — it will represent exclusive ownership of admitted
/// in-flight bytes and must stay uniquely owned rather than copyable.
///
/// This slice uses the budget for static planning only; enforcing the sum of
/// bytes actually in flight is a later slice.
///
/// # Examples
///
/// ```
/// use range_replay::{BudgetError, ByteBudget};
///
/// let budget = ByteBudget::try_new(8)?;
/// assert_eq!(budget.bytes(), 8);
///
/// assert_eq!(ByteBudget::try_new(0), Err(BudgetError::ZeroBudget));
/// # Ok::<(), BudgetError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteBudget {
    bytes: u64,
}

impl ByteBudget {
    /// Creates a budget allowing physical reads of up to `bytes` bytes.
    ///
    /// # Errors
    ///
    /// Returns [`BudgetError::ZeroBudget`] when `bytes` is `0`.
    pub const fn try_new(bytes: u64) -> Result<Self, BudgetError> {
        if bytes == 0 {
            return Err(BudgetError::ZeroBudget);
        }

        Ok(Self { bytes })
    }

    /// Returns the byte limit, which is always at least `1`.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// One logical range grouped with the physical reads that cover it.
///
/// Values can only be constructed during [`ExecutionPlan`] derivation; no
/// public constructor accepts arbitrary reads, so the association between a
/// logical range and its covering physical reads can be neither forged nor
/// broken after construction.
///
/// The grouping is the reconstruction contract for later slices: the bytes of
/// the logical range are exactly the bytes of its physical reads, in order.
/// [`Self::physical_reads`] is never empty, is sorted by ascending offset,
/// has exactly adjacent neighbours, starts at the logical offset, and ends at
/// the logical end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedRange {
    logical_range: ReadRange,
    physical_reads: Vec<ReadRange>,
}

impl PlannedRange {
    /// Returns the logical range the physical reads reconstruct.
    #[must_use]
    pub const fn logical_range(&self) -> ReadRange {
        self.logical_range
    }

    /// Returns the physical reads covering the logical range exactly once.
    ///
    /// The slice is never empty; every read has a length between `1` and the
    /// byte budget of the parent [`ExecutionPlan`], and every read except a
    /// possible final tail has exactly that budget's length.
    #[must_use]
    pub fn physical_reads(&self) -> &[ReadRange] {
        &self.physical_reads
    }
}

/// An owned physical plan derived from one [`ReadPlan`] and one
/// [`ByteBudget`].
///
/// The logical plan stays the canonical description of *which* bytes to read;
/// the execution plan describes *how* those bytes are read without any single
/// operation exceeding the budget. Construction borrows the logical plan and
/// leaves it unchanged, and the returned value owns its budget, logical-range
/// copies, and physical reads, so it stays valid after the source plan is
/// dropped.
///
/// For equal logical plans and budgets, the derived plan, its physical-read
/// order, and its operation count are equal.
///
/// # Examples
///
/// ```
/// use range_replay::{ByteBudget, ExecutionPlan, ReadPlan, ReadRange};
///
/// let schedule = [ReadRange::try_new(0, 16)?, ReadRange::try_new(20, 5)?];
/// let plan = ReadPlan::try_from_schedule(&schedule)?;
/// let budget = ByteBudget::try_new(8)?;
///
/// let execution = ExecutionPlan::try_from_read_plan(&plan, budget)?;
///
/// assert_eq!(execution.budget(), budget);
/// assert_eq!(execution.ranges().len(), 2);
/// assert_eq!(
///     execution.ranges()[0].physical_reads(),
///     &[ReadRange::try_new(0, 8)?, ReadRange::try_new(8, 8)?]
/// );
/// assert_eq!(
///     execution.ranges()[1].physical_reads(),
///     &[ReadRange::try_new(20, 5)?]
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionPlan {
    budget: ByteBudget,
    ranges: Vec<PlannedRange>,
}

impl ExecutionPlan {
    /// Derives the physical plan covering `plan` under `budget`.
    ///
    /// Each logical range is split greedily: starting at its offset, every
    /// physical read takes the largest length that exceeds neither the bytes
    /// remaining in the range nor the budget. Derivation is pure and
    /// deterministic: it performs no I/O, leaves the borrowed plan untouched,
    /// and produces equal values for equal inputs. The planned ranges
    /// preserve the order of [`ReadPlan::ranges`].
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionPlanError::UnrepresentableOperationCount`] when the
    /// physical reads covering one logical range cannot be counted as a
    /// collection capacity,
    /// [`ExecutionPlanError::ReservationFailed`] when the metadata allocation
    /// for the planned entries cannot be reserved, and
    /// [`ExecutionPlanError::UnrepresentableSplit`] if splitting produced
    /// bounds outside the [`ReadRange`] contract. The last case cannot occur
    /// for validated inputs; it guards the splitting arithmetic rather than
    /// describing a caller mistake.
    pub fn try_from_read_plan(
        plan: &ReadPlan,
        budget: ByteBudget,
    ) -> Result<Self, ExecutionPlanError> {
        let logical = plan.ranges();

        let mut ranges = Vec::new();
        ranges.try_reserve_exact(logical.len()).map_err(|source| {
            ExecutionPlanError::ReservationFailed {
                capacity: logical.len(),
                source,
            }
        })?;

        for &logical_range in logical {
            ranges.push(PlannedRange {
                logical_range,
                physical_reads: split_range(logical_range, budget)?,
            });
        }

        Ok(Self { budget, ranges })
    }

    /// Returns the budget the plan was derived under.
    #[must_use]
    pub const fn budget(&self) -> ByteBudget {
        self.budget
    }

    /// Returns the planned ranges in [`ReadPlan::ranges`] order.
    ///
    /// The slice is never empty and borrows from the plan, so the coverage
    /// invariants established at construction cannot be broken through it.
    #[must_use]
    pub fn ranges(&self) -> &[PlannedRange] {
        &self.ranges
    }
}

fn split_range(
    logical: ReadRange,
    budget: ByteBudget,
) -> Result<Vec<ReadRange>, ExecutionPlanError> {
    let operations = operation_count(logical, budget)?;

    let mut reads = Vec::new();
    reads.try_reserve_exact(operations).map_err(|source| {
        ExecutionPlanError::ReservationFailed {
            capacity: operations,
            source,
        }
    })?;

    let mut offset = logical.offset();

    while offset < logical.end() {
        let read = next_read(offset, logical, budget)?;
        offset = read.end();
        reads.push(read);
    }

    Ok(reads)
}

fn operation_count(logical: ReadRange, budget: ByteBudget) -> Result<usize, ExecutionPlanError> {
    let full_reads = logical.length().checked_div(budget.bytes());
    let tail_read = logical
        .length()
        .checked_rem(budget.bytes())
        .map(|tail| u64::from(tail != 0));

    full_reads
        .zip(tail_read)
        .and_then(|(full, tail)| full.checked_add(tail))
        .and_then(|operations| usize::try_from(operations).ok())
        .ok_or(ExecutionPlanError::UnrepresentableOperationCount {
            offset: logical.offset(),
            end: logical.end(),
            budget: budget.bytes(),
        })
}

fn next_read(
    offset: u64,
    logical: ReadRange,
    budget: ByteBudget,
) -> Result<ReadRange, ExecutionPlanError> {
    logical
        .end()
        .checked_sub(offset)
        .and_then(|remaining| ReadRange::try_new(offset, remaining.min(budget.bytes())).ok())
        .ok_or(ExecutionPlanError::UnrepresentableSplit {
            offset,
            end: logical.end(),
            budget: budget.bytes(),
        })
}

#[cfg(test)]
mod tests {
    use super::{BudgetError, ByteBudget, ExecutionPlan, ExecutionPlanError};
    use crate::plan::ReadPlan;
    use crate::range::ReadRange;

    fn span(start: u64, end: u64) -> ReadRange {
        ReadRange::try_new(start, end - start).expect("test spans are valid ranges")
    }

    fn budget(bytes: u64) -> ByteBudget {
        ByteBudget::try_new(bytes).expect("test budgets are non-zero")
    }

    fn plan(schedule: &[ReadRange]) -> ReadPlan {
        ReadPlan::try_from_schedule(schedule).expect("test schedules are not empty")
    }

    fn execution(schedule: &[ReadRange], bytes: u64) -> ExecutionPlan {
        ExecutionPlan::try_from_read_plan(&plan(schedule), budget(bytes))
            .expect("test plans split without failure")
    }

    fn physical_bounds(execution: &ExecutionPlan) -> Vec<Vec<(u64, u64)>> {
        execution
            .ranges()
            .iter()
            .map(|planned| {
                planned
                    .physical_reads()
                    .iter()
                    .map(|read| (read.offset(), read.end()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn byte_budget_rejects_zero() {
        assert_eq!(ByteBudget::try_new(0), Err(BudgetError::ZeroBudget));
    }

    #[test]
    fn byte_budget_preserves_its_exact_value() {
        assert_eq!(budget(8).bytes(), 8);
        assert_eq!(budget(u64::MAX).bytes(), u64::MAX);
    }

    #[test]
    fn execution_plan_matches_the_hand_calculated_fixture() {
        let execution = execution(&[span(0, 16), span(20, 25)], 8);

        assert_eq!(execution.budget(), budget(8));

        let logical: Vec<ReadRange> = execution
            .ranges()
            .iter()
            .map(super::PlannedRange::logical_range)
            .collect();
        assert_eq!(logical, vec![span(0, 16), span(20, 25)]);

        assert_eq!(
            physical_bounds(&execution),
            vec![vec![(0, 8), (8, 16)], vec![(20, 25)]]
        );

        let operations: usize = execution
            .ranges()
            .iter()
            .map(|planned| planned.physical_reads().len())
            .sum();
        let physical_bytes: u64 = execution
            .ranges()
            .iter()
            .flat_map(super::PlannedRange::physical_reads)
            .map(ReadRange::length)
            .sum();
        assert_eq!(operations, 3);
        assert_eq!(physical_bytes, 21);
    }

    #[test]
    fn a_range_shorter_than_the_budget_is_one_identical_read() {
        assert_eq!(
            physical_bounds(&execution(&[span(10, 13)], 8)),
            vec![vec![(10, 13)]]
        );
    }

    #[test]
    fn a_range_equal_to_the_budget_is_one_identical_read() {
        assert_eq!(
            physical_bounds(&execution(&[span(10, 18)], 8)),
            vec![vec![(10, 18)]]
        );
    }

    #[test]
    fn a_non_multiple_length_ends_with_one_exact_tail() {
        assert_eq!(
            physical_bounds(&execution(&[span(0, 10)], 4)),
            vec![vec![(0, 4), (4, 8), (8, 10)]]
        );
    }

    #[test]
    fn equal_inputs_produce_equal_execution_plans() {
        let plan = plan(&[span(0, 16), span(20, 25)]);

        let first = ExecutionPlan::try_from_read_plan(&plan, budget(8))
            .expect("test plans split without failure");
        let second = ExecutionPlan::try_from_read_plan(&plan, budget(8))
            .expect("test plans split without failure");

        assert_eq!(first, second);
        assert_eq!(physical_bounds(&first), physical_bounds(&second));
    }

    #[test]
    fn construction_leaves_the_read_plan_unchanged() {
        let plan = plan(&[span(10, 12), span(0, 4)]);
        let original = plan.clone();

        let _ = ExecutionPlan::try_from_read_plan(&plan, budget(3))
            .expect("test plans split without failure");

        assert_eq!(plan, original);
    }

    #[test]
    fn execution_plan_outlives_the_read_plan() {
        let execution = {
            let plan = plan(&[span(0, 16), span(20, 25)]);

            ExecutionPlan::try_from_read_plan(&plan, budget(8))
                .expect("test plans split without failure")
        };

        assert_eq!(
            physical_bounds(&execution),
            vec![vec![(0, 8), (8, 16)], vec![(20, 25)]]
        );
    }

    #[test]
    fn a_range_ending_at_the_last_representable_offset_splits_without_overflow() {
        assert_eq!(
            physical_bounds(&execution(&[span(u64::MAX - 10, u64::MAX)], 4)),
            vec![vec![
                (u64::MAX - 10, u64::MAX - 6),
                (u64::MAX - 6, u64::MAX - 2),
                (u64::MAX - 2, u64::MAX),
            ]]
        );
    }

    #[test]
    fn an_unallocatable_operation_collection_is_a_typed_error() {
        let plan = plan(&[span(0, u64::MAX)]);

        let result = ExecutionPlan::try_from_read_plan(&plan, budget(1));

        assert!(matches!(
            result,
            Err(ExecutionPlanError::ReservationFailed { .. }
                | ExecutionPlanError::UnrepresentableOperationCount { .. })
        ));
    }
}
