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
//! The representation is compact: a plan stores one [`PlannedRange`] per
//! logical range — the range, the budget, and the exact operation count —
//! and never materializes a collection of physical reads. Construction is
//! proportional to the number of logical ranges, while each physical read is
//! computed on demand through [`PlannedRange::physical_read`], so a later
//! scheduler can request reads incrementally without reconstructing or
//! re-validating the plan.
//!
//! This module is planning only. Nothing here reads a file or schedules
//! work; tracking and releasing the bytes actually in flight belongs to
//! [`BudgetLimiter`](crate::BudgetLimiter), and scheduling remains a later
//! slice.

use std::collections::TryReserveError;

use thiserror::Error;

use crate::budget::ByteBudget;
use crate::plan::ReadPlan;
use crate::range::ReadRange;

/// Reason an [`ExecutionPlan`] could not be derived or a physical read could
/// not be generated.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ExecutionPlanError {
    /// Counting the physical reads of one logical range overflowed `u64`.
    ///
    /// The count is the exact ceiling of `length / budget`, which fits in a
    /// `u64` for every validated range and non-zero budget, so this variant
    /// guards the counting arithmetic instead of describing a reachable
    /// input. Reporting it keeps construction free of a panic path and free
    /// of any silent correction that would return a wrong count.
    #[error("counting the physical reads of range [{offset}, {end}) at budget {budget} overflowed")]
    UnrepresentableOperationCount {
        /// Inclusive start offset of the logical range being counted.
        offset: u64,
        /// Exclusive end offset of the logical range being counted.
        end: u64,
        /// Byte budget the count had to respect.
        budget: u64,
    },
    /// The metadata allocation for the planned ranges could not be reserved.
    ///
    /// The only allocation in a plan is one entry per logical range, bounded
    /// by the already-materialized [`ReadPlan`]. It is reserved fallibly
    /// before any entry is produced, so a plan is either built completely or
    /// fails with this variant instead of aborting mid-way.
    #[error("cannot reserve plan capacity for {capacity} planned ranges")]
    ReservationFailed {
        /// Number of planned ranges whose reservation failed.
        capacity: usize,
        /// Allocator failure reported by the reservation.
        #[source]
        source: TryReserveError,
    },
    /// Generating one physical read produced bounds that no [`ReadRange`]
    /// can represent.
    ///
    /// Every index below the operation count maps to a valid read for
    /// validated inputs, so this variant guards the generation arithmetic
    /// instead of describing a reachable input. Reporting it keeps lookup
    /// free of a panic path and free of any silent correction that would
    /// return a read with wrong coverage.
    #[error(
        "operation {operation_index} of range [{offset}, {end}) at budget {budget} produced an \
         invalid read"
    )]
    UnrepresentableRead {
        /// Index of the physical read that could not be generated.
        operation_index: u64,
        /// Inclusive start offset of the logical range being split.
        offset: u64,
        /// Exclusive end offset of the logical range being split.
        end: u64,
        /// Byte budget the read had to respect.
        budget: u64,
    },
}

/// One logical range grouped with the physical reads that cover it.
///
/// Values can only be constructed during [`ExecutionPlan`] derivation; no
/// public constructor accepts arbitrary data, so the association between a
/// logical range and its covering physical reads can be neither forged nor
/// broken after construction.
///
/// The representation is compact: only the logical range, the budget, and
/// the exact operation count are stored, never a collection of reads. Each
/// physical read is computed on demand by [`Self::physical_read`], which
/// deterministically reproduces the greedy sequence: reads are non-empty, no
/// longer than the budget, ordered by ascending offset, exactly adjacent,
/// starting at the logical offset and ending at the logical end, with every
/// read except a possible final tail exactly the budget's length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedRange {
    logical_range: ReadRange,
    budget: ByteBudget,
    operation_count: u64,
}

impl PlannedRange {
    fn try_new(logical_range: ReadRange, budget: ByteBudget) -> Result<Self, ExecutionPlanError> {
        let full_reads = logical_range.length().checked_div(budget.bytes());
        let tail_read = logical_range
            .length()
            .checked_rem(budget.bytes())
            .map(|tail| u64::from(tail != 0));

        full_reads
            .zip(tail_read)
            .and_then(|(full, tail)| full.checked_add(tail))
            .map(|operation_count| Self {
                logical_range,
                budget,
                operation_count,
            })
            .ok_or(ExecutionPlanError::UnrepresentableOperationCount {
                offset: logical_range.offset(),
                end: logical_range.end(),
                budget: budget.bytes(),
            })
    }

    /// Returns the logical range the physical reads reconstruct.
    #[must_use]
    pub const fn logical_range(&self) -> ReadRange {
        self.logical_range
    }

    /// Returns the exact number of physical reads covering the logical
    /// range.
    ///
    /// The count is the ceiling of the logical length divided by the budget
    /// and is always at least `1`. It stays a `u64` because a compact plan
    /// never needs a collection of that size; narrowing to `usize` belongs
    /// to whoever later builds a concrete bounded collection.
    #[must_use]
    pub const fn operation_count(&self) -> u64 {
        self.operation_count
    }

    /// Returns the physical read at `operation_index`, or `None` past the
    /// end.
    ///
    /// Lookup is `O(1)`: the read is computed directly from the logical
    /// range and the budget without materializing or traversing earlier
    /// operations. Every index in `0..operation_count()` maps to exactly one
    /// read of the deterministic greedy sequence; every index at or above
    /// the count returns `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionPlanError::UnrepresentableRead`] if the generation
    /// arithmetic produced bounds outside the [`ReadRange`] contract. This
    /// cannot occur for a plan built from validated inputs; it guards the
    /// arithmetic rather than describing a reachable caller mistake.
    pub fn physical_read(
        &self,
        operation_index: u64,
    ) -> Result<Option<ReadRange>, ExecutionPlanError> {
        if operation_index >= self.operation_count {
            return Ok(None);
        }

        operation_index
            .checked_mul(self.budget.bytes())
            .and_then(|skipped| self.logical_range.offset().checked_add(skipped))
            .and_then(|offset| {
                let remaining = self.logical_range.end().checked_sub(offset)?;

                ReadRange::try_new(offset, remaining.min(self.budget.bytes())).ok()
            })
            .map(Some)
            .ok_or(ExecutionPlanError::UnrepresentableRead {
                operation_index,
                offset: self.logical_range.offset(),
                end: self.logical_range.end(),
                budget: self.budget.bytes(),
            })
    }
}

/// An owned physical plan derived from one [`ReadPlan`] and one
/// [`ByteBudget`].
///
/// The logical plan stays the canonical description of *which* bytes to
/// read; the execution plan describes *how* those bytes are read without any
/// single operation exceeding the budget. Construction borrows the logical
/// plan and leaves it unchanged, and the returned value owns its budget and
/// planned ranges, so it stays valid after the source plan is dropped.
///
/// Construction stores one compact [`PlannedRange`] per logical range and
/// performs no work proportional to the total physical operation count;
/// physical reads are generated only when requested.
///
/// For equal logical plans and budgets, the derived plan, its operation
/// counts, and the physical read at every valid index are equal.
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
///
/// let first = &execution.ranges()[0];
/// assert_eq!(first.operation_count(), 2);
/// assert_eq!(first.physical_read(0)?, Some(ReadRange::try_new(0, 8)?));
/// assert_eq!(first.physical_read(1)?, Some(ReadRange::try_new(8, 8)?));
/// assert_eq!(first.physical_read(2)?, None);
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
    /// Each logical range is described by a compact [`PlannedRange`] whose
    /// greedy physical reads are generated on demand: starting at the
    /// logical offset, every read takes the largest length that exceeds
    /// neither the bytes remaining in the range nor the budget. Derivation
    /// is pure and deterministic: it performs no I/O, leaves the borrowed
    /// plan untouched, allocates only one entry per logical range, and
    /// produces equal values for equal inputs. The planned ranges preserve
    /// the order of [`ReadPlan::ranges`].
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionPlanError::ReservationFailed`] when the
    /// per-logical-range metadata allocation cannot be reserved, and
    /// [`ExecutionPlanError::UnrepresentableOperationCount`] if counting the
    /// physical reads of one logical range overflowed. The second case
    /// cannot occur for validated inputs; it guards the counting arithmetic
    /// rather than describing a caller mistake.
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
            ranges.push(PlannedRange::try_new(logical_range, budget)?);
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

#[cfg(test)]
mod tests {
    use super::{ByteBudget, ExecutionPlan, PlannedRange};
    use crate::plan::ReadPlan;
    use crate::range::ReadRange;

    const TEBIBYTE: u64 = 1 << 40;

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
            .expect("test plans derive without failure")
    }

    fn read_at(planned: &PlannedRange, operation_index: u64) -> Option<ReadRange> {
        planned
            .physical_read(operation_index)
            .expect("test lookups stay within the generation contract")
    }

    #[test]
    fn execution_plan_matches_the_hand_calculated_fixture() {
        let execution = execution(&[span(0, 16), span(20, 25)], 8);

        assert_eq!(execution.budget(), budget(8));
        assert_eq!(execution.ranges().len(), 2);

        let first = &execution.ranges()[0];
        assert_eq!(first.logical_range(), span(0, 16));
        assert_eq!(first.operation_count(), 2);
        assert_eq!(read_at(first, 0), Some(span(0, 8)));
        assert_eq!(read_at(first, 1), Some(span(8, 16)));
        assert_eq!(read_at(first, 2), None);

        let second = &execution.ranges()[1];
        assert_eq!(second.logical_range(), span(20, 25));
        assert_eq!(second.operation_count(), 1);
        assert_eq!(read_at(second, 0), Some(span(20, 25)));
        assert_eq!(read_at(second, 1), None);
    }

    #[test]
    fn a_range_shorter_than_the_budget_is_one_identical_read() {
        let execution = execution(&[span(10, 13)], 8);
        let planned = &execution.ranges()[0];

        assert_eq!(planned.operation_count(), 1);
        assert_eq!(read_at(planned, 0), Some(span(10, 13)));
        assert_eq!(read_at(planned, 1), None);
    }

    #[test]
    fn a_range_equal_to_the_budget_is_one_identical_read() {
        let execution = execution(&[span(10, 18)], 8);
        let planned = &execution.ranges()[0];

        assert_eq!(planned.operation_count(), 1);
        assert_eq!(read_at(planned, 0), Some(span(10, 18)));
        assert_eq!(read_at(planned, 1), None);
    }

    #[test]
    fn a_non_multiple_length_ends_with_one_exact_tail() {
        let execution = execution(&[span(0, 10)], 4);
        let planned = &execution.ranges()[0];

        assert_eq!(planned.operation_count(), 3);
        assert_eq!(read_at(planned, 0), Some(span(0, 4)));
        assert_eq!(read_at(planned, 1), Some(span(4, 8)));
        assert_eq!(read_at(planned, 2), Some(span(8, 10)));
        assert_eq!(read_at(planned, 3), None);
    }

    #[test]
    fn an_out_of_range_index_returns_none_without_error() {
        let execution = execution(&[span(0, 10)], 4);
        let planned = &execution.ranges()[0];

        assert_eq!(read_at(planned, 3), None);
        assert_eq!(read_at(planned, u64::MAX), None);
    }

    #[test]
    fn equal_inputs_produce_equal_execution_plans() {
        let plan = plan(&[span(0, 16), span(20, 25)]);

        let first = ExecutionPlan::try_from_read_plan(&plan, budget(8))
            .expect("test plans derive without failure");
        let second = ExecutionPlan::try_from_read_plan(&plan, budget(8))
            .expect("test plans derive without failure");

        assert_eq!(first, second);
        for (left, right) in first.ranges().iter().zip(second.ranges()) {
            assert_eq!(left.operation_count(), right.operation_count());
            for operation_index in 0..left.operation_count() {
                assert_eq!(
                    read_at(left, operation_index),
                    read_at(right, operation_index)
                );
            }
        }
    }

    #[test]
    fn construction_leaves_the_read_plan_unchanged() {
        let plan = plan(&[span(10, 12), span(0, 4)]);
        let original = plan.clone();

        let _ = ExecutionPlan::try_from_read_plan(&plan, budget(3))
            .expect("test plans derive without failure");

        assert_eq!(plan, original);
    }

    #[test]
    fn execution_plan_outlives_the_read_plan() {
        let execution = {
            let plan = plan(&[span(0, 16), span(20, 25)]);

            ExecutionPlan::try_from_read_plan(&plan, budget(8))
                .expect("test plans derive without failure")
        };

        assert_eq!(execution.ranges().len(), 2);
        assert_eq!(read_at(&execution.ranges()[0], 0), Some(span(0, 8)));
        assert_eq!(read_at(&execution.ranges()[1], 0), Some(span(20, 25)));
    }

    #[test]
    fn a_range_ending_at_the_last_representable_offset_splits_without_overflow() {
        let execution = execution(&[span(u64::MAX - 10, u64::MAX)], 4);
        let planned = &execution.ranges()[0];

        assert_eq!(planned.operation_count(), 3);
        assert_eq!(read_at(planned, 0), Some(span(u64::MAX - 10, u64::MAX - 6)));
        assert_eq!(read_at(planned, 1), Some(span(u64::MAX - 6, u64::MAX - 2)));
        assert_eq!(read_at(planned, 2), Some(span(u64::MAX - 2, u64::MAX)));
        assert_eq!(read_at(planned, 3), None);
    }

    #[test]
    fn the_widest_possible_range_plans_compactly_at_budget_one() {
        let execution = execution(&[span(0, u64::MAX)], 1);
        let planned = &execution.ranges()[0];

        assert_eq!(planned.operation_count(), u64::MAX);
        assert_eq!(read_at(planned, 0), Some(span(0, 1)));
        assert_eq!(
            read_at(planned, u64::MAX - 1),
            Some(span(u64::MAX - 1, u64::MAX))
        );
        assert_eq!(read_at(planned, u64::MAX), None);
    }

    #[test]
    fn a_tebibyte_range_at_a_small_budget_plans_without_materialization() {
        let execution = execution(&[span(0, TEBIBYTE)], 4096);
        let planned = &execution.ranges()[0];

        assert_eq!(planned.operation_count(), 268_435_456);
        assert_eq!(read_at(planned, 0), Some(span(0, 4096)));
        assert_eq!(read_at(planned, 1), Some(span(4096, 8192)));
        assert_eq!(
            read_at(planned, 268_435_455),
            Some(span(TEBIBYTE - 4096, TEBIBYTE))
        );
        assert_eq!(read_at(planned, 268_435_456), None);
    }
}
