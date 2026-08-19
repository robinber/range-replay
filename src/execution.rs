//! Read-size-derived physical execution planning over a validated logical
//! plan.
//!
//! A [`ReadPlan`] is the canonical *logical* request: which bytes must be
//! read, independent of any backend, machine, or configuration. An
//! [`ExecutionPlan`] is the *physical* plan derived from one logical plan and
//! one [`ExecutionConfig`]: the same bytes, split into the concrete read
//! operations whose sizes never exceed the configured [`ReadSize`]. The two
//! stay separate types because the same logical range needs different
//! physical reads under different read sizes, and a tuning input must never
//! change the established meaning of the logical plan.
//!
//! The configuration separates two policies that are both measured in bytes
//! but mean different things. [`ReadSize`] bounds the length of *one*
//! physical read and is the only value that shapes the plan. The
//! [`ByteBudget`] inside the configuration bounds the *total* bytes a
//! [`BudgetLimiter`](crate::BudgetLimiter) keeps in flight at runtime and
//! never affects splitting. Because [`ExecutionConfig`] validates
//! `read_size <= byte_budget` at construction, every planned physical read
//! fits under an empty limiter built from the same configuration, and
//! several reads can be admitted together whenever the budget can hold
//! their combined lengths.
//!
//! Splitting is deterministic and greedy: each logical range is covered from
//! its start offset by the largest read that exceeds neither the bytes
//! remaining in the range nor the read size. Every physical read except a
//! possible final tail therefore has exactly the read size's length. For
//! every planned range the physical reads are non-empty, ordered by
//! ascending offset, exactly adjacent with neither gap nor overlap, and
//! cover every logical byte exactly once. For a fixed logical plan and read
//! size, changing only the budget changes neither the operation counts nor
//! any physical read.
//!
//! The representation is compact: a plan stores one [`PlannedRange`] per
//! logical range — the range, the read size, and the exact operation count —
//! and never materializes a collection of physical reads. Construction is
//! proportional to the number of logical ranges, while each physical read is
//! computed on demand through [`PlannedRange::physical_read`], so the
//! [`Scheduler`](crate::Scheduler) can request reads incrementally without
//! reconstructing or re-validating the plan.
//!
//! This module is planning only. Nothing here reads a file or schedules
//! work; tracking and releasing the bytes actually in flight belongs to
//! [`BudgetLimiter`](crate::BudgetLimiter), a
//! [`Scheduler`](crate::Scheduler) selects and admits planned reads under
//! the budget, and the synchronous executor
//! [`execute_pread`](crate::execute_pread) submits them.

use std::collections::TryReserveError;

use thiserror::Error;

use crate::budget::ByteBudget;
use crate::plan::ReadPlan;
use crate::range::ReadRange;

/// Reason a [`ReadSize`] could not be constructed.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ReadSizeError {
    /// The requested read size was `0`, so no physical read could ever make
    /// progress.
    #[error("read size must be greater than zero")]
    ZeroReadSize,
    /// The requested read size exceeds [`ReadSize::MAX_BYTES`], so one
    /// physical read of that length could be capped short by the operating
    /// system instead of completing exactly.
    #[error(
        "read size of {requested} bytes exceeds the maximum of {maximum} bytes for one physical \
         read"
    )]
    ReadSizeExceedsMaximum {
        /// Exact requested read size in bytes.
        requested: u64,
        /// Exact maximum read size in bytes, always [`ReadSize::MAX_BYTES`].
        maximum: u64,
    },
}

/// Reason an [`ExecutionConfig`] could not be constructed.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ExecutionConfigError {
    /// The read size exceeds the byte budget, so not even one full physical
    /// read could ever be admitted by a limiter enforcing that budget.
    #[error(
        "read size of {read_size} bytes exceeds the byte budget of {byte_budget} bytes, so no \
         full physical read could ever be admitted"
    )]
    ReadSizeExceedsBudget {
        /// Exact read size in bytes that could never be admitted whole.
        read_size: u64,
        /// Exact byte budget that the read size exceeds.
        byte_budget: u64,
    },
}

/// A validated maximum length in bytes for one physical read, always in
/// `1..=ReadSize::MAX_BYTES`.
///
/// The read size bounds a *single* operation: an [`ExecutionPlan`] splits
/// every logical range into greedy reads no longer than this value. It is an
/// explicit tuning input, not a measured optimum — nothing here chooses,
/// benchmarks, or adjusts it. The *total* bytes simultaneously in flight are
/// a separate policy owned by [`ByteBudget`]; [`ExecutionConfig`] pairs the
/// two and guarantees the read size fits under the budget.
///
/// Construction rejects both ends of the invalid domain. A read size of `0`
/// could never make progress, so it is an invalid configuration rather than
/// a degenerate plan. A read size above [`Self::MAX_BYTES`] could be capped
/// short by the operating system within one call, so it is rejected before
/// either backend can observe it; logical ranges larger than the maximum
/// stay valid and split into several physical reads. `ReadSize` is [`Copy`]
/// because it is immutable configuration.
///
/// # Examples
///
/// ```
/// use range_replay::{ReadSize, ReadSizeError};
///
/// let read_size = ReadSize::try_new(4)?;
/// assert_eq!(read_size.bytes(), 4);
///
/// assert_eq!(ReadSize::try_new(0), Err(ReadSizeError::ZeroReadSize));
/// assert_eq!(
///     ReadSize::try_new(ReadSize::MAX_BYTES + 1),
///     Err(ReadSizeError::ReadSizeExceedsMaximum {
///         requested: ReadSize::MAX_BYTES + 1,
///         maximum: ReadSize::MAX_BYTES,
///     })
/// );
/// # Ok::<(), ReadSizeError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadSize {
    bytes: u64,
}

impl ReadSize {
    /// Largest accepted length in bytes for one physical read: 1 GiB.
    ///
    /// The ceiling is a fixed, backend-neutral validity bound shared by the
    /// `pread` and `io_uring` backends, chosen deliberately below Linux's
    /// documented per-call transfer cap so no accepted physical read can be
    /// capped short by the kernel within one call. It is a hard constant
    /// rather than a host-probed value, so equal inputs produce the same
    /// physical plan on every machine, and it is a correctness policy, not a
    /// recommended read size. Logical ranges larger than the maximum remain
    /// valid: planning splits them into several physical reads.
    pub const MAX_BYTES: u64 = 1 << 30;

    /// Creates a read size allowing physical reads of up to `bytes` bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ReadSizeError::ZeroReadSize`] when `bytes` is `0`, and
    /// [`ReadSizeError::ReadSizeExceedsMaximum`] with the exact requested
    /// and maximum values when `bytes` exceeds [`Self::MAX_BYTES`].
    pub const fn try_new(bytes: u64) -> Result<Self, ReadSizeError> {
        if bytes == 0 {
            return Err(ReadSizeError::ZeroReadSize);
        }
        if bytes > Self::MAX_BYTES {
            return Err(ReadSizeError::ReadSizeExceedsMaximum {
                requested: bytes,
                maximum: Self::MAX_BYTES,
            });
        }

        Ok(Self { bytes })
    }

    /// Returns the maximum physical read length, which is always in
    /// `1..=Self::MAX_BYTES`.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// A validated pairing of one [`ReadSize`] with one [`ByteBudget`].
///
/// Construction guarantees `0 < read_size <= byte_budget`, so the pairing is
/// the proof that every physical read planned under it fits under an empty
/// [`BudgetLimiter`](crate::BudgetLimiter) built from the same
/// configuration:
///
/// ```
/// use range_replay::{BudgetLimiter, ByteBudget, ExecutionConfig, ReadSize};
///
/// let config = ExecutionConfig::try_new(ReadSize::try_new(4)?, ByteBudget::try_new(8)?)?;
/// let limiter = BudgetLimiter::new(config.byte_budget());
/// # let _ = limiter;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// The two values are independent policies. The read size shapes the
/// physical plan; the budget only governs how many already-planned bytes a
/// limiter admits at once. A budget that can hold the combined lengths of
/// several planned reads lets them be in flight simultaneously without
/// requiring several threads — admission is accounting, not parallel
/// execution, and a merely valid configuration promises only that one full
/// read fits. A [`Scheduler`](crate::Scheduler) admits planned reads under
/// the budget, and [`execute_pread`](crate::execute_pread) executes them
/// synchronously.
///
/// An invalid pairing is unrepresentable: `try_new` rejects a read size
/// larger than the budget with the exact offending values instead of
/// clamping, swapping, or reinterpreting either one. `ExecutionConfig` is
/// [`Copy`] because it is immutable configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionConfig {
    read_size: ReadSize,
    byte_budget: ByteBudget,
}

impl ExecutionConfig {
    /// Pairs `read_size` with `byte_budget`, validating that one full read
    /// fits under the budget.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionConfigError::ReadSizeExceedsBudget`] with the
    /// exact byte values when `read_size` is larger than `byte_budget`.
    ///
    /// # Examples
    ///
    /// ```
    /// use range_replay::{ByteBudget, ExecutionConfig, ExecutionConfigError, ReadSize};
    ///
    /// let read_size = ReadSize::try_new(8)?;
    /// let equal = ExecutionConfig::try_new(read_size, ByteBudget::try_new(8)?)?;
    /// assert_eq!(equal.read_size().bytes(), 8);
    /// assert_eq!(equal.byte_budget().bytes(), 8);
    ///
    /// assert_eq!(
    ///     ExecutionConfig::try_new(read_size, ByteBudget::try_new(7)?),
    ///     Err(ExecutionConfigError::ReadSizeExceedsBudget {
    ///         read_size: 8,
    ///         byte_budget: 7,
    ///     })
    /// );
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub const fn try_new(
        read_size: ReadSize,
        byte_budget: ByteBudget,
    ) -> Result<Self, ExecutionConfigError> {
        if read_size.bytes() > byte_budget.bytes() {
            return Err(ExecutionConfigError::ReadSizeExceedsBudget {
                read_size: read_size.bytes(),
                byte_budget: byte_budget.bytes(),
            });
        }

        Ok(Self {
            read_size,
            byte_budget,
        })
    }

    /// Returns the maximum length of one physical read.
    #[must_use]
    pub const fn read_size(&self) -> ReadSize {
        self.read_size
    }

    /// Returns the total in-flight byte capacity for runtime admission.
    #[must_use]
    pub const fn byte_budget(&self) -> ByteBudget {
        self.byte_budget
    }
}

/// Reason an [`ExecutionPlan`] could not be derived or a physical read could
/// not be generated.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ExecutionPlanError {
    /// Counting the physical reads of one logical range overflowed `u64`.
    ///
    /// The count is the exact ceiling of `length / read_size`, which fits in
    /// a `u64` for every validated range and non-zero read size, so this
    /// variant guards the counting arithmetic instead of describing a
    /// reachable input. Reporting it keeps construction free of a panic path
    /// and free of any silent correction that would return a wrong count.
    #[error(
        "counting the physical reads of range [{offset}, {end}) at read size {read_size} \
         overflowed"
    )]
    UnrepresentableOperationCount {
        /// Inclusive start offset of the logical range being counted.
        offset: u64,
        /// Exclusive end offset of the logical range being counted.
        end: u64,
        /// Read size in bytes the count had to respect.
        read_size: u64,
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
        "operation {operation_index} of range [{offset}, {end}) at read size {read_size} \
         produced an invalid read"
    )]
    UnrepresentableRead {
        /// Index of the physical read that could not be generated.
        operation_index: u64,
        /// Inclusive start offset of the logical range being split.
        offset: u64,
        /// Exclusive end offset of the logical range being split.
        end: u64,
        /// Read size in bytes the read had to respect.
        read_size: u64,
    },
}

/// One logical range grouped with the physical reads that cover it.
///
/// Values can only be constructed during [`ExecutionPlan`] derivation; no
/// public constructor accepts arbitrary data, so the association between a
/// logical range and its covering physical reads can be neither forged nor
/// broken after construction.
///
/// The representation is compact: only the logical range, the read size, and
/// the exact operation count are stored, never a collection of reads. Each
/// physical read is computed on demand by [`Self::physical_read`], which
/// deterministically reproduces the greedy sequence: reads are non-empty, no
/// longer than the read size, ordered by ascending offset, exactly adjacent,
/// starting at the logical offset and ending at the logical end, with every
/// read except a possible final tail exactly the read size's length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedRange {
    logical_range: ReadRange,
    read_size: ReadSize,
    operation_count: u64,
}

impl PlannedRange {
    fn try_new(logical_range: ReadRange, read_size: ReadSize) -> Result<Self, ExecutionPlanError> {
        let full_reads = logical_range.length().checked_div(read_size.bytes());
        let tail_read = logical_range
            .length()
            .checked_rem(read_size.bytes())
            .map(|tail| u64::from(tail != 0));

        full_reads
            .zip(tail_read)
            .and_then(|(full, tail)| full.checked_add(tail))
            .map(|operation_count| Self {
                logical_range,
                read_size,
                operation_count,
            })
            .ok_or(ExecutionPlanError::UnrepresentableOperationCount {
                offset: logical_range.offset(),
                end: logical_range.end(),
                read_size: read_size.bytes(),
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
    /// The count is the ceiling of the logical length divided by the read
    /// size and is always at least `1`. It stays a `u64` because a compact
    /// plan never needs a collection of that size; narrowing to `usize`
    /// belongs to whoever later builds a concrete bounded collection.
    #[must_use]
    pub const fn operation_count(&self) -> u64 {
        self.operation_count
    }

    /// Returns the physical read at `operation_index`, or `None` past the
    /// end.
    ///
    /// Lookup is `O(1)`: the read is computed directly from the logical
    /// range and the read size without materializing or traversing earlier
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
            .checked_mul(self.read_size.bytes())
            .and_then(|skipped| self.logical_range.offset().checked_add(skipped))
            .and_then(|offset| {
                let remaining = self.logical_range.end().checked_sub(offset)?;

                ReadRange::try_new(offset, remaining.min(self.read_size.bytes())).ok()
            })
            .map(Some)
            .ok_or(ExecutionPlanError::UnrepresentableRead {
                operation_index,
                offset: self.logical_range.offset(),
                end: self.logical_range.end(),
                read_size: self.read_size.bytes(),
            })
    }
}

/// An owned physical plan derived from one [`ReadPlan`] and one
/// [`ExecutionConfig`].
///
/// The logical plan stays the canonical description of *which* bytes to
/// read; the execution plan describes *how* those bytes are read without any
/// single operation exceeding the configured [`ReadSize`]. Construction
/// borrows the logical plan and leaves it unchanged, and the returned value
/// owns its complete configuration and planned ranges, so it stays valid
/// after the source plan is dropped.
///
/// Only the read size shapes the plan. The [`ByteBudget`] inside the
/// configuration is retained for provenance and for constructing a runtime
/// limiter, but changing only the budget changes neither the operation
/// counts nor any physical read. Two physical reads that together fit under
/// the budget remain two distinct operations; neither the plan nor a caller
/// may merge them back into one larger read.
///
/// Construction stores one compact [`PlannedRange`] per logical range and
/// performs no work proportional to the total physical operation count;
/// physical reads are generated only when requested.
///
/// For equal logical plans and configurations, the derived plan, its
/// operation counts, and the physical read at every valid index are equal.
///
/// # Examples
///
/// ```
/// use range_replay::{ByteBudget, ExecutionConfig, ExecutionPlan, ReadPlan, ReadRange, ReadSize};
///
/// let schedule = [ReadRange::try_new(0, 16)?, ReadRange::try_new(20, 5)?];
/// let plan = ReadPlan::try_from_schedule(&schedule)?;
/// let config = ExecutionConfig::try_new(ReadSize::try_new(8)?, ByteBudget::try_new(16)?)?;
///
/// let execution = ExecutionPlan::try_from_read_plan(&plan, config)?;
///
/// assert_eq!(execution.config(), config);
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
    config: ExecutionConfig,
    ranges: Vec<PlannedRange>,
}

impl ExecutionPlan {
    /// Derives the physical plan covering `plan` under `config`.
    ///
    /// Each logical range is described by a compact [`PlannedRange`] whose
    /// greedy physical reads are generated on demand: starting at the
    /// logical offset, every read takes the largest length that exceeds
    /// neither the bytes remaining in the range nor the configured read
    /// size. The budget inside `config` plays no part in splitting.
    /// Derivation is pure and deterministic: it performs no I/O, leaves the
    /// borrowed plan untouched, allocates only one entry per logical range,
    /// and produces equal values for equal inputs. The planned ranges
    /// preserve the order of [`ReadPlan::ranges`].
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
        config: ExecutionConfig,
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
            ranges.push(PlannedRange::try_new(logical_range, config.read_size())?);
        }

        Ok(Self { config, ranges })
    }

    /// Returns the complete configuration the plan was derived under.
    #[must_use]
    pub const fn config(&self) -> ExecutionConfig {
        self.config
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
    use super::{
        ByteBudget, ExecutionConfig, ExecutionConfigError, ExecutionPlan, PlannedRange, ReadSize,
        ReadSizeError,
    };
    use crate::range::ReadRange;
    use crate::test_support::{execution, plan, span};

    const TEBIBYTE: u64 = 1 << 40;

    fn read_size(bytes: u64) -> ReadSize {
        ReadSize::try_new(bytes).expect("test read sizes are within the valid domain")
    }

    fn budget(bytes: u64) -> ByteBudget {
        ByteBudget::try_new(bytes).expect("test budgets are non-zero")
    }

    fn config(read_size_bytes: u64, budget_bytes: u64) -> ExecutionConfig {
        ExecutionConfig::try_new(read_size(read_size_bytes), budget(budget_bytes))
            .expect("test configurations pair a read size with a large enough budget")
    }

    fn read_at(planned: &PlannedRange, operation_index: u64) -> Option<ReadRange> {
        planned
            .physical_read(operation_index)
            .expect("test lookups stay within the generation contract")
    }

    fn physical_reads(planned: &PlannedRange) -> Vec<ReadRange> {
        (0..planned.operation_count())
            .map(|operation_index| {
                read_at(planned, operation_index).expect("indexes below the count produce reads")
            })
            .collect()
    }

    #[test]
    fn read_size_rejects_zero() {
        assert_eq!(ReadSize::try_new(0), Err(ReadSizeError::ZeroReadSize));
    }

    #[test]
    fn the_maximum_read_size_is_exactly_one_gibibyte() {
        assert_eq!(ReadSize::MAX_BYTES, 1_073_741_824);
    }

    #[test]
    fn read_size_preserves_its_exact_value_across_the_valid_domain() {
        assert_eq!(read_size(1).bytes(), 1);
        assert_eq!(read_size(4096).bytes(), 4096);
        assert_eq!(read_size(ReadSize::MAX_BYTES).bytes(), 1_073_741_824);
    }

    #[test]
    fn read_size_rejects_values_above_the_maximum_with_exact_values() {
        assert_eq!(
            ReadSize::try_new(1_073_741_825),
            Err(ReadSizeError::ReadSizeExceedsMaximum {
                requested: 1_073_741_825,
                maximum: 1_073_741_824,
            })
        );
        assert_eq!(
            ReadSize::try_new(u64::MAX),
            Err(ReadSizeError::ReadSizeExceedsMaximum {
                requested: u64::MAX,
                maximum: 1_073_741_824,
            })
        );
    }

    #[test]
    fn execution_config_accepts_a_read_size_below_the_budget() {
        let config = config(4, 8);

        assert_eq!(config.read_size(), read_size(4));
        assert_eq!(config.byte_budget(), budget(8));
    }

    #[test]
    fn execution_config_accepts_a_read_size_equal_to_the_budget() {
        let config = config(8, 8);

        assert_eq!(config.read_size(), read_size(8));
        assert_eq!(config.byte_budget(), budget(8));
    }

    #[test]
    fn execution_config_rejects_a_read_size_above_the_budget_with_exact_values() {
        assert_eq!(
            ExecutionConfig::try_new(read_size(9), budget(8)),
            Err(ExecutionConfigError::ReadSizeExceedsBudget {
                read_size: 9,
                byte_budget: 8,
            })
        );
    }

    #[test]
    fn execution_config_accepts_the_maximum_read_size_with_an_equal_budget() {
        let config = config(ReadSize::MAX_BYTES, ReadSize::MAX_BYTES);

        assert_eq!(config.read_size().bytes(), ReadSize::MAX_BYTES);
        assert_eq!(config.byte_budget().bytes(), ReadSize::MAX_BYTES);
    }

    #[test]
    fn a_maximum_read_size_above_its_budget_is_a_budget_error_not_a_read_size_error() {
        assert_eq!(
            ExecutionConfig::try_new(
                read_size(ReadSize::MAX_BYTES),
                budget(ReadSize::MAX_BYTES - 1)
            ),
            Err(ExecutionConfigError::ReadSizeExceedsBudget {
                read_size: ReadSize::MAX_BYTES,
                byte_budget: ReadSize::MAX_BYTES - 1,
            })
        );
    }

    #[test]
    fn execution_plan_matches_the_hand_calculated_fixture() {
        let execution = execution(&[span(0, 16), span(20, 25)], 8, 16);

        assert_eq!(execution.config(), config(8, 16));
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
    fn the_ten_byte_fixture_splits_into_two_full_reads_and_one_tail() {
        let execution = execution(&[span(0, 10)], 4, 8);
        let planned = &execution.ranges()[0];

        assert_eq!(planned.operation_count(), 3);
        assert_eq!(read_at(planned, 0), Some(span(0, 4)));
        assert_eq!(read_at(planned, 1), Some(span(4, 8)));
        assert_eq!(read_at(planned, 2), Some(span(8, 10)));
        assert_eq!(read_at(planned, 3), None);
    }

    #[test]
    fn a_range_shorter_than_the_read_size_is_one_identical_read() {
        let execution = execution(&[span(10, 13)], 8, 8);
        let planned = &execution.ranges()[0];

        assert_eq!(planned.operation_count(), 1);
        assert_eq!(read_at(planned, 0), Some(span(10, 13)));
        assert_eq!(read_at(planned, 1), None);
    }

    #[test]
    fn a_range_equal_to_the_read_size_is_one_identical_read() {
        let execution = execution(&[span(10, 18)], 8, 8);
        let planned = &execution.ranges()[0];

        assert_eq!(planned.operation_count(), 1);
        assert_eq!(read_at(planned, 0), Some(span(10, 18)));
        assert_eq!(read_at(planned, 1), None);
    }

    #[test]
    fn an_out_of_range_index_returns_none_without_error() {
        let execution = execution(&[span(0, 10)], 4, 8);
        let planned = &execution.ranges()[0];

        assert_eq!(read_at(planned, 3), None);
        assert_eq!(read_at(planned, u64::MAX), None);
    }

    #[test]
    fn equal_inputs_produce_equal_execution_plans() {
        let plan = plan(&[span(0, 16), span(20, 25)]);

        let first = ExecutionPlan::try_from_read_plan(&plan, config(8, 16))
            .expect("test plans derive without failure");
        let second = ExecutionPlan::try_from_read_plan(&plan, config(8, 16))
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
    fn changing_only_the_budget_leaves_every_physical_read_unchanged() {
        let narrow = execution(&[span(0, 8)], 2, 4);
        let wide = execution(&[span(0, 8)], 2, 8);

        assert_ne!(narrow.config(), wide.config());

        let expected = [span(0, 2), span(2, 4), span(4, 6), span(6, 8)];
        for execution in [&narrow, &wide] {
            let planned = &execution.ranges()[0];
            assert_eq!(planned.operation_count(), 4);
            assert_eq!(physical_reads(planned), expected);
        }
    }

    #[test]
    fn changing_only_the_read_size_changes_the_physical_reads_deterministically() {
        let plan = plan(&[span(0, 8)]);

        let quarters = ExecutionPlan::try_from_read_plan(&plan, config(2, 8))
            .expect("test plans derive without failure");
        let halves = ExecutionPlan::try_from_read_plan(&plan, config(4, 8))
            .expect("test plans derive without failure");

        assert_eq!(
            physical_reads(&quarters.ranges()[0]),
            [span(0, 2), span(2, 4), span(4, 6), span(6, 8)]
        );
        assert_eq!(
            physical_reads(&halves.ranges()[0]),
            [span(0, 4), span(4, 8)]
        );
    }

    #[test]
    fn construction_leaves_the_read_plan_unchanged() {
        let plan = plan(&[span(10, 12), span(0, 4)]);
        let original = plan.clone();

        let execution = ExecutionPlan::try_from_read_plan(&plan, config(3, 6))
            .expect("test plans derive without failure");

        assert_eq!(plan, original);
        assert_eq!(execution.config(), config(3, 6));
    }

    #[test]
    fn execution_plan_outlives_the_read_plan() {
        let execution = {
            let plan = plan(&[span(0, 16), span(20, 25)]);

            ExecutionPlan::try_from_read_plan(&plan, config(8, 16))
                .expect("test plans derive without failure")
        };

        assert_eq!(execution.config(), config(8, 16));
        assert_eq!(execution.ranges().len(), 2);
        assert_eq!(read_at(&execution.ranges()[0], 0), Some(span(0, 8)));
        assert_eq!(read_at(&execution.ranges()[1], 0), Some(span(20, 25)));
    }

    #[test]
    fn a_range_ending_at_the_last_representable_offset_splits_without_overflow() {
        let execution = execution(&[span(u64::MAX - 10, u64::MAX)], 4, 8);
        let planned = &execution.ranges()[0];

        assert_eq!(planned.operation_count(), 3);
        assert_eq!(read_at(planned, 0), Some(span(u64::MAX - 10, u64::MAX - 6)));
        assert_eq!(read_at(planned, 1), Some(span(u64::MAX - 6, u64::MAX - 2)));
        assert_eq!(read_at(planned, 2), Some(span(u64::MAX - 2, u64::MAX)));
        assert_eq!(read_at(planned, 3), None);
    }

    #[test]
    fn the_widest_possible_range_plans_compactly_at_read_size_one() {
        let execution = execution(&[span(0, u64::MAX)], 1, 1);
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
    fn a_logical_range_above_the_maximum_read_size_splits_into_two_adjacent_reads() {
        const MAX: u64 = ReadSize::MAX_BYTES;

        let execution = execution(&[span(0, MAX + 4096)], MAX, MAX);
        let planned = &execution.ranges()[0];

        assert_eq!(planned.operation_count(), 2);
        assert_eq!(read_at(planned, 0), Some(span(0, MAX)));
        assert_eq!(read_at(planned, 1), Some(span(MAX, MAX + 4096)));
        assert_eq!(read_at(planned, 2), None);
    }

    #[test]
    fn a_tebibyte_range_at_a_small_read_size_plans_without_materialization() {
        let execution = execution(&[span(0, TEBIBYTE)], 4096, 65536);
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
