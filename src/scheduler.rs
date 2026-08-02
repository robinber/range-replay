//! Budget-aware greedy scheduling of planned physical reads.
//!
//! A [`Scheduler`] owns one [`ExecutionPlan`] and one internal
//! [`BudgetLimiter`] built from the plan's own configuration. Each call to
//! [`Scheduler::schedule_next`] makes one local decision: among the
//! still-pending physical reads whose length fits in the currently available
//! bytes, it selects the one with the greatest length, breaking equal
//! lengths by plan order — the lexicographically smallest [`OperationId`].
//! The selected read is reserved through the internal limiter and removed
//! from the pending set before a [`ScheduleDecision::Ready`] becomes
//! observable, so every returned [`ScheduledRead`] proves its bytes are
//! already accounted for.
//!
//! The policy is deliberately greedy, not globally optimal. With 6 bytes
//! available and pending lengths 4, 3, and 3 it admits the single 4-byte
//! read, even though the two 3-byte reads together would have used all 6
//! bytes; no subset-sum or bin-packing search happens. Because every
//! physical read except at most one shorter final tail per logical range
//! has exactly the configured read size, equal-length full reads retain
//! plan order while a fitting tail may pass blocked full reads. Submission
//! order may therefore differ from physical-offset order, even within one
//! logical range; stable identity, not submission order, ties each read
//! back to the plan.
//!
//! Temporary backpressure and exhaustion stay distinct.
//! [`ScheduleDecision::WaitingForBudget`] means pending work remains but
//! nothing fits right now; it mutates nothing, and dropping a live
//! [`ScheduledRead`] releases its exact bytes so a retry can succeed.
//! [`ScheduleDecision::Exhausted`] means the scheduler has nothing left to
//! distribute — live handles may still exist, so exhaustion is *not*
//! global completion. A future executor owns backend invocation,
//! completion supervision, and the final finished state; nothing here
//! reads a file, allocates a buffer, or sees an I/O result.
//!
//! Scheduler state stays compact: construction and retained metadata are
//! proportional to the number of logical [`PlannedRange`] entries, never to
//! their summed operation counts. Physical reads are generated on demand
//! through the plan's indexed lookup, and the per-range progress is one
//! cursor over the equal-length full reads plus one optional pending tail
//! length.

use std::collections::TryReserveError;
use std::fmt;

use thiserror::Error;

use crate::budget::{BudgetLimiter, Reservation, ReservationError};
use crate::execution::{ExecutionPlan, ExecutionPlanError, PlannedRange};
use crate::range::ReadRange;

/// Reason a [`Scheduler`] could not be constructed or a scheduling decision
/// failed permanently.
///
/// Temporary budget pressure is never an error; it is the non-mutating
/// [`ScheduleDecision::WaitingForBudget`] decision. Backend and I/O
/// failures have no variant here because the scheduler never sees bytes;
/// those belong to the future executor and backend boundary.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SchedulerError {
    /// The compact progress allocation for the logical ranges could not be
    /// reserved.
    ///
    /// The only allocation in a scheduler is one progress entry per logical
    /// range, bounded by the already-materialized plan. It is reserved
    /// fallibly before any entry is produced, so a scheduler is either
    /// built completely or fails with this variant instead of aborting
    /// mid-way.
    #[error("cannot reserve scheduler progress state for {capacity} logical ranges")]
    StateReservationFailed {
        /// Number of logical ranges whose progress reservation failed.
        capacity: usize,
        /// Allocator failure reported by the reservation.
        #[source]
        source: TryReserveError,
    },
    /// Generating the physical read of one operation failed.
    ///
    /// This cannot occur for a plan built from validated inputs; it
    /// preserves the guard of the plan's generation arithmetic instead of
    /// describing a reachable caller mistake.
    #[error("generating the physical read of {id} failed")]
    PhysicalReadFailed {
        /// Identity of the operation whose physical read failed.
        id: OperationId,
        /// Generation failure reported by the plan.
        #[source]
        source: ExecutionPlanError,
    },
    /// The plan has no physical read for an operation the scheduler still
    /// tracks as pending.
    ///
    /// This cannot occur for a scheduler built from a valid plan; it
    /// reports an internal inconsistency instead of admitting bytes for an
    /// operation the plan cannot produce.
    #[error("the plan has no physical read for {id}")]
    PhysicalReadMissing {
        /// Identity of the operation without a physical read.
        id: OperationId,
    },
    /// The internal limiter permanently rejected the physical read of one
    /// operation.
    ///
    /// This cannot occur for a plan built from a validated
    /// [`ExecutionConfig`](crate::ExecutionConfig), which guarantees every
    /// physical read fits under the budget; it preserves the limiter's
    /// permanent-rejection guard instead of retrying or waiting.
    #[error("the budget limiter permanently rejected the physical read of {id}")]
    AdmissionRejected {
        /// Identity of the operation whose admission was rejected.
        id: OperationId,
        /// Permanent rejection reported by the limiter.
        #[source]
        source: ReservationError,
    },
}

/// Stable identity of one physical read inside one [`ExecutionPlan`].
///
/// The identity pairs the index of the logical range — in the order
/// retained by [`ExecutionPlan::ranges`] — with the index of the physical
/// operation inside that range. The operation index stays a `u64` because a
/// compact plan never narrows its physical operation count to `usize`.
///
/// An identity is local to the plan it was scheduled from; two schedulers
/// over different plans may return equal identities for unrelated reads.
/// Only scheduling produces values — the type has no public constructor, so
/// an identity cannot be forged for an operation that was never admitted.
/// Because the scheduler may admit operations out of physical-offset order,
/// this identity — not submission order — is what preserves each read's
/// relationship to the plan.
///
/// The derived ordering compares the logical-range index first and the
/// operation index second, which is exactly the documented lexicographic
/// plan order used to break equal-length ties.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId {
    logical_range_index: usize,
    operation_index: u64,
}

impl OperationId {
    const fn new(logical_range_index: usize, operation_index: u64) -> Self {
        Self {
            logical_range_index,
            operation_index,
        }
    }

    /// Returns the index of the logical range in [`ExecutionPlan::ranges`]
    /// order.
    #[must_use]
    pub const fn logical_range_index(&self) -> usize {
        self.logical_range_index
    }

    /// Returns the index of the physical operation inside its logical
    /// range.
    #[must_use]
    pub const fn operation_index(&self) -> u64 {
        self.operation_index
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "operation {} of logical range {}",
            self.operation_index, self.logical_range_index
        )
    }
}

/// Proof that one physical read was admitted under the budget.
///
/// A value pairs the stable [`OperationId`] of the read with the owned
/// [`Reservation`] that already accounts for its exact length. Only a
/// successful [`Scheduler::schedule_next`] constructs one, and the type is
/// deliberately neither [`Clone`] nor [`Copy`]: one admission creates
/// exactly one handle and therefore exactly one release.
///
/// The handle is an admission capability, not proof of successful I/O: no
/// backend has read any byte when it is returned. Dropping it releases
/// exactly the bytes of [`Self::range`] back to the scheduler's budget,
/// whether the guarded work succeeded, failed, or never ran; the scheduler
/// does not requeue the operation.
#[derive(Debug)]
#[must_use = "dropping a scheduled read immediately releases its admitted budget bytes"]
pub struct ScheduledRead {
    id: OperationId,
    reservation: Reservation,
}

impl ScheduledRead {
    /// Returns the stable identity of the admitted physical read.
    #[must_use]
    pub const fn id(&self) -> OperationId {
        self.id
    }

    /// Returns the exact physical range whose length the owned reservation
    /// admitted.
    #[must_use]
    pub const fn range(&self) -> ReadRange {
        self.reservation.range()
    }
}

/// Outcome of one [`Scheduler::schedule_next`] decision.
///
/// The three variants keep temporary backpressure, permanent distribution
/// of the plan, and admitted work distinct; permanent failures are the
/// separate [`SchedulerError`].
#[derive(Debug)]
#[must_use = "a ready decision owns admitted budget bytes; ignoring a decision may drop them without any work having run"]
pub enum ScheduleDecision {
    /// One pending operation was greedily selected, reserved, removed from
    /// the pending set, and returned.
    Ready(ScheduledRead),
    /// Pending work remains, but no pending operation fits in the currently
    /// available bytes. Nothing changed; dropping a live [`ScheduledRead`]
    /// releases capacity so a retry can succeed.
    WaitingForBudget,
    /// No pending physical operation remains to distribute. Live
    /// [`ScheduledRead`] handles may still exist, so exhaustion is not
    /// execution completion; only a future executor can declare global
    /// success after every admitted read finished.
    Exhausted,
}

/// Compact pending-work state of one logical range.
///
/// Every operation index below `next_full` was issued, every index in
/// `next_full..full_count` is a pending full-size read, and the shorter
/// tail at operation index `full_count` is pending exactly while
/// `pending_tail` holds its length. No per-operation entry ever exists, so
/// the state stays proportional to the logical ranges.
#[derive(Clone, Debug)]
struct RangeProgress {
    next_full: u64,
    full_count: u64,
    pending_tail: Option<u64>,
}

impl RangeProgress {
    fn try_from_planned(
        logical_range_index: usize,
        planned: &PlannedRange,
        read_size: u64,
    ) -> Result<Self, SchedulerError> {
        let operation_count = planned.operation_count();
        let Some(last_index) = operation_count.checked_sub(1) else {
            // Unreachable: every planned range covers at least one byte and
            // therefore plans at least one operation. Fail closed with
            // nothing pending instead of inventing work.
            return Ok(Self {
                next_full: 0,
                full_count: 0,
                pending_tail: None,
            });
        };

        // The plan's indexed lookup is the single source of truth for the
        // greedy split, so the presence and length of a shorter tail are
        // read off the last operation instead of re-deriving the splitting
        // arithmetic here.
        let id = OperationId::new(logical_range_index, last_index);
        let last = planned
            .physical_read(last_index)
            .map_err(|source| SchedulerError::PhysicalReadFailed { id, source })?
            .ok_or(SchedulerError::PhysicalReadMissing { id })?;

        if last.length() < read_size {
            Ok(Self {
                next_full: 0,
                full_count: last_index,
                pending_tail: Some(last.length()),
            })
        } else {
            Ok(Self {
                next_full: 0,
                full_count: operation_count,
                pending_tail: None,
            })
        }
    }
}

/// Fallibly reserves the compact progress allocation for `capacity` logical
/// ranges, so construction either completes or fails with a typed error
/// before any entry is produced.
fn try_reserve_progress(capacity: usize) -> Result<Vec<RangeProgress>, SchedulerError> {
    let mut progress = Vec::new();
    progress
        .try_reserve_exact(capacity)
        .map_err(|source| SchedulerError::StateReservationFailed { capacity, source })?;

    Ok(progress)
}

/// Incremental greedy distribution of one [`ExecutionPlan`] under its own
/// byte budget.
///
/// The scheduler owns the plan and an internal [`BudgetLimiter`] built from
/// the plan's configuration; the limiter is deliberately not exposed, so no
/// caller can make unrelated reservations that would perturb scheduling
/// decisions. Every [`Self::schedule_next`] call applies the same local
/// policy:
///
/// ```text
/// eligible = pending operations whose length <= available bytes
/// chosen   = greatest length in eligible
/// tie      = earliest OperationId in plan order
/// ```
///
/// The policy is not a global utilization optimum — it never searches for a
/// combination of operations — and it may admit reads out of
/// physical-offset order, including within one logical range, when a
/// shorter tail fits while full reads do not. Selection is deterministic
/// for equal plans and equal sequences of admissions and releases.
///
/// State stays proportional to the number of logical ranges: full-size
/// reads are addressed through one cursor per range and each physical range
/// is generated on demand, so even a plan with hundreds of millions of
/// physical operations schedules without any per-operation allocation.
///
/// # Examples
///
/// ```
/// use range_replay::{
///     ByteBudget, ExecutionConfig, ExecutionPlan, ReadPlan, ReadRange, ReadSize,
///     ScheduleDecision, Scheduler,
/// };
///
/// # fn ready(decision: ScheduleDecision) -> range_replay::ScheduledRead {
/// #     match decision {
/// #         ScheduleDecision::Ready(read) => read,
/// #         other => panic!("expected a ready decision, got {other:?}"),
/// #     }
/// # }
/// let plan = ReadPlan::try_from_schedule(&[ReadRange::try_new(0, 10)?])?;
/// let config = ExecutionConfig::try_new(ReadSize::try_new(4)?, ByteBudget::try_new(6)?)?;
/// let mut scheduler = Scheduler::try_new(ExecutionPlan::try_from_read_plan(&plan, config)?)?;
///
/// // Greedy largest-fitting: the first full read is admitted...
/// let first = ready(scheduler.schedule_next()?);
/// assert_eq!(first.range(), ReadRange::try_new(0, 4)?);
///
/// // ...then the 2-byte tail passes the blocked full read [4, 8).
/// let tail = ready(scheduler.schedule_next()?);
/// assert_eq!(tail.range(), ReadRange::try_new(8, 2)?);
/// assert_eq!(tail.id().operation_index(), 2);
/// assert_eq!(scheduler.available_bytes(), 0);
///
/// // Nothing fits: temporary backpressure, not an error and not exhaustion.
/// assert!(matches!(
///     scheduler.schedule_next()?,
///     ScheduleDecision::WaitingForBudget
/// ));
///
/// // Releasing admitted bytes lets the passed full read be admitted.
/// drop(first);
/// let second = ready(scheduler.schedule_next()?);
/// assert_eq!(second.range(), ReadRange::try_new(4, 4)?);
///
/// // The plan is exhausted while handles are still alive: distribution is
/// // done, execution is not.
/// assert!(matches!(
///     scheduler.schedule_next()?,
///     ScheduleDecision::Exhausted
/// ));
/// # drop(tail);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct Scheduler {
    plan: ExecutionPlan,
    limiter: BudgetLimiter,
    progress: Vec<RangeProgress>,
}

impl Scheduler {
    /// Builds a scheduler owning `plan` with every physical operation
    /// pending and nothing in flight.
    ///
    /// Construction allocates exactly one compact progress entry per
    /// logical range and performs no work proportional to the total
    /// physical operation count. The internal limiter is built from the
    /// plan's own [`ByteBudget`](crate::ByteBudget), which the plan's
    /// configuration already validated against the read size.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerError::StateReservationFailed`] when the
    /// per-logical-range progress allocation cannot be reserved. The
    /// [`SchedulerError::PhysicalReadFailed`] and
    /// [`SchedulerError::PhysicalReadMissing`] variants guard the probe of
    /// each range's final operation and cannot occur for a plan built from
    /// validated inputs. Every construction failure consumes the plan and
    /// never yields a partial scheduler.
    pub fn try_new(plan: ExecutionPlan) -> Result<Self, SchedulerError> {
        let read_size = plan.config().read_size().bytes();
        let planned = plan.ranges();
        let capacity = planned.len();

        let mut progress = try_reserve_progress(capacity)?;

        for (logical_range_index, planned_range) in planned.iter().enumerate() {
            progress.push(RangeProgress::try_from_planned(
                logical_range_index,
                planned_range,
                read_size,
            )?);
        }

        let limiter = BudgetLimiter::new(plan.config().byte_budget());

        Ok(Self {
            plan,
            limiter,
            progress,
        })
    }

    /// Returns the sum of bytes admitted for live [`ScheduledRead`] handles
    /// and not yet released, which never exceeds the plan's budget.
    #[must_use]
    pub fn in_flight_bytes(&self) -> u64 {
        self.limiter.in_flight_bytes()
    }

    /// Returns the bytes the next admission could still take right now.
    #[must_use]
    pub fn available_bytes(&self) -> u64 {
        self.limiter.available_bytes()
    }

    /// Greedily selects, admits, and returns the next pending physical
    /// read.
    ///
    /// Among the pending operations whose length fits in
    /// [`Self::available_bytes`], the one with the greatest length wins;
    /// equal lengths use the earliest [`OperationId`] in plan order. The
    /// decision is local — no combination of operations is ever searched —
    /// so the selection is deterministic but not a global utilization
    /// optimum.
    ///
    /// Admission is fail-closed: the chosen range is reserved through the
    /// internal limiter and removed from the pending set before the
    /// returned [`ScheduleDecision::Ready`] is observable. When pending
    /// work remains but nothing fits, the non-mutating
    /// [`ScheduleDecision::WaitingForBudget`] is returned and dropping a
    /// live [`ScheduledRead`] is what makes a retry able to succeed. Once
    /// nothing is pending, every further call returns
    /// [`ScheduleDecision::Exhausted`], which only ends distribution: live
    /// handles may still be executing under a future executor.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerError::PhysicalReadFailed`],
    /// [`SchedulerError::PhysicalReadMissing`], or
    /// [`SchedulerError::AdmissionRejected`] with the exact
    /// [`OperationId`] when the plan cannot produce the selected read or
    /// the limiter rejects it permanently. Neither can occur for a
    /// scheduler built from validated inputs; both guard internal
    /// inconsistencies by admitting nothing instead of panicking. Temporary
    /// budget pressure is never an error.
    pub fn schedule_next(&mut self) -> Result<ScheduleDecision, SchedulerError> {
        let available = self.limiter.available_bytes();
        let read_size = self.plan.config().read_size().bytes();

        let mut work_remains = false;
        let mut best: Option<(u64, OperationId)> = None;

        for (logical_range_index, progress) in self.progress.iter().enumerate() {
            if progress.next_full < progress.full_count {
                work_remains = true;
                if read_size <= available
                    && best.is_none_or(|(best_length, _)| read_size > best_length)
                {
                    best = Some((
                        read_size,
                        OperationId::new(logical_range_index, progress.next_full),
                    ));
                }
            }

            if let Some(tail_length) = progress.pending_tail {
                work_remains = true;
                if tail_length <= available
                    && best.is_none_or(|(best_length, _)| tail_length > best_length)
                {
                    best = Some((
                        tail_length,
                        OperationId::new(logical_range_index, progress.full_count),
                    ));
                }
            }
        }

        let Some((_, id)) = best else {
            if work_remains {
                return Ok(ScheduleDecision::WaitingForBudget);
            }

            return Ok(ScheduleDecision::Exhausted);
        };

        let range = self.physical_read(id)?;

        let reservation = match self.limiter.try_reserve(range) {
            Ok(Some(reservation)) => reservation,
            Ok(None) => return Ok(ScheduleDecision::WaitingForBudget),
            Err(source) => return Err(SchedulerError::AdmissionRejected { id, source }),
        };

        let Some(progress) = self.progress.get_mut(id.logical_range_index()) else {
            // Unreachable: the scan produced `id` from an existing progress
            // entry. Fail closed by releasing the reservation and reporting
            // the inconsistency instead of exposing an admission the
            // pending state does not reflect.
            drop(reservation);
            return Err(SchedulerError::PhysicalReadMissing { id });
        };

        if id.operation_index() < progress.full_count {
            // The selected operation is the next full read, so the
            // increment stays at or below `full_count` and cannot overflow.
            // A hypothetically corrupt state fails closed by marking every
            // full read issued instead of wrapping.
            progress.next_full = match id.operation_index().checked_add(1) {
                Some(next_full) => next_full,
                None => progress.full_count,
            };
        } else {
            progress.pending_tail = None;
        }

        Ok(ScheduleDecision::Ready(ScheduledRead { id, reservation }))
    }

    fn physical_read(&self, id: OperationId) -> Result<ReadRange, SchedulerError> {
        let planned = self
            .plan
            .ranges()
            .get(id.logical_range_index())
            .ok_or(SchedulerError::PhysicalReadMissing { id })?;

        planned
            .physical_read(id.operation_index())
            .map_err(|source| SchedulerError::PhysicalReadFailed { id, source })?
            .ok_or(SchedulerError::PhysicalReadMissing { id })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OperationId, ScheduleDecision, ScheduledRead, Scheduler, SchedulerError,
        try_reserve_progress,
    };
    use crate::budget::ByteBudget;
    use crate::execution::{ExecutionConfig, ExecutionPlan, ReadSize};
    use crate::plan::ReadPlan;
    use crate::range::ReadRange;

    const TEBIBYTE: u64 = 1 << 40;

    fn span(start: u64, end: u64) -> ReadRange {
        ReadRange::try_new(start, end - start).expect("test spans are valid ranges")
    }

    fn execution(schedule: &[ReadRange], read_size_bytes: u64, budget_bytes: u64) -> ExecutionPlan {
        let plan = ReadPlan::try_from_schedule(schedule).expect("test schedules are not empty");
        let read_size = ReadSize::try_new(read_size_bytes).expect("test read sizes are non-zero");
        let budget = ByteBudget::try_new(budget_bytes).expect("test budgets are non-zero");
        let config = ExecutionConfig::try_new(read_size, budget)
            .expect("test configurations pair a read size with a large enough budget");

        ExecutionPlan::try_from_read_plan(&plan, config).expect("test plans derive without failure")
    }

    fn scheduler(schedule: &[ReadRange], read_size_bytes: u64, budget_bytes: u64) -> Scheduler {
        Scheduler::try_new(execution(schedule, read_size_bytes, budget_bytes))
            .expect("test schedulers construct without failure")
    }

    fn op(logical_range_index: usize, operation_index: u64) -> OperationId {
        OperationId::new(logical_range_index, operation_index)
    }

    fn ready(scheduler: &mut Scheduler) -> ScheduledRead {
        match scheduler
            .schedule_next()
            .expect("test scheduling decisions succeed")
        {
            ScheduleDecision::Ready(read) => read,
            decision => panic!("expected a ready decision, got {decision:?}"),
        }
    }

    fn assert_waiting(scheduler: &mut Scheduler) {
        assert!(matches!(
            scheduler
                .schedule_next()
                .expect("test scheduling decisions succeed"),
            ScheduleDecision::WaitingForBudget
        ));
    }

    fn assert_exhausted(scheduler: &mut Scheduler) {
        assert!(matches!(
            scheduler
                .schedule_next()
                .expect("test scheduling decisions succeed"),
            ScheduleDecision::Exhausted
        ));
    }

    fn counters(scheduler: &Scheduler) -> (u64, u64) {
        (scheduler.in_flight_bytes(), scheduler.available_bytes())
    }

    #[test]
    fn a_new_scheduler_starts_with_nothing_in_flight() {
        let scheduler = scheduler(&[span(0, 10)], 4, 10);

        assert_eq!(counters(&scheduler), (0, 10));
    }

    #[test]
    fn operation_ids_preserve_their_exact_indices() {
        let mut scheduler = scheduler(&[span(0, 10), span(20, 22)], 4, 16);

        let first = ready(&mut scheduler);
        assert_eq!(first.id(), op(0, 0));
        assert_eq!(first.id().logical_range_index(), 0);
        assert_eq!(first.id().operation_index(), 0);
        assert_eq!(first.range(), span(0, 4));

        let second = ready(&mut scheduler);
        assert_eq!(second.id(), op(0, 1));
        assert_eq!(second.range(), span(4, 8));

        let first_tail = ready(&mut scheduler);
        assert_eq!(first_tail.id(), op(0, 2));
        assert_eq!(first_tail.range(), span(8, 10));

        let second_tail = ready(&mut scheduler);
        assert_eq!(second_tail.id(), op(1, 0));
        assert_eq!(second_tail.id().logical_range_index(), 1);
        assert_eq!(second_tail.id().operation_index(), 0);
        assert_eq!(second_tail.range(), span(20, 22));
    }

    #[test]
    fn equal_full_reads_are_returned_in_plan_order_across_logical_ranges() {
        let mut scheduler = scheduler(&[span(0, 8), span(10, 18)], 4, 16);

        assert_eq!(ready(&mut scheduler).id(), op(0, 0));
        assert_eq!(ready(&mut scheduler).id(), op(0, 1));
        assert_eq!(ready(&mut scheduler).id(), op(1, 0));
        assert_eq!(ready(&mut scheduler).id(), op(1, 1));
        assert_exhausted(&mut scheduler);
    }

    #[test]
    fn equal_tails_use_plan_order() {
        let mut scheduler = scheduler(&[span(0, 2), span(10, 12)], 4, 8);

        assert_eq!(ready(&mut scheduler).id(), op(0, 0));
        assert_eq!(ready(&mut scheduler).id(), op(1, 0));
        assert_exhausted(&mut scheduler);
    }

    #[test]
    fn the_hand_calculated_fixture_reorders_the_tail_and_waits_before_exhaustion() {
        let mut scheduler = scheduler(&[span(0, 14)], 4, 10);

        let a = ready(&mut scheduler);
        assert_eq!(a.id(), op(0, 0));
        assert_eq!(a.range(), span(0, 4));
        assert_eq!(counters(&scheduler), (4, 6));

        let b = ready(&mut scheduler);
        assert_eq!(b.id(), op(0, 1));
        assert_eq!(b.range(), span(4, 8));
        assert_eq!(counters(&scheduler), (8, 2));

        let d = ready(&mut scheduler);
        assert_eq!(d.id(), op(0, 3));
        assert_eq!(d.range(), span(12, 14));
        assert_eq!(counters(&scheduler), (10, 0));

        assert_waiting(&mut scheduler);
        assert_eq!(counters(&scheduler), (10, 0));

        drop(a);
        assert_eq!(counters(&scheduler), (6, 4));

        let c = ready(&mut scheduler);
        assert_eq!(c.id(), op(0, 2));
        assert_eq!(c.range(), span(8, 12));
        assert_eq!(counters(&scheduler), (10, 0));

        assert_exhausted(&mut scheduler);

        drop(b);
        drop(c);
        drop(d);
        assert_eq!(counters(&scheduler), (0, 10));
        assert_exhausted(&mut scheduler);
    }

    #[test]
    fn a_smaller_tail_passes_blocked_full_reads_in_its_own_logical_range() {
        let mut scheduler = scheduler(&[span(0, 10)], 4, 6);

        let first = ready(&mut scheduler);
        assert_eq!(first.id(), op(0, 0));

        let tail = ready(&mut scheduler);
        assert_eq!(tail.id(), op(0, 2));
        assert_eq!(tail.range(), span(8, 10));
        assert_eq!(counters(&scheduler), (6, 0));

        assert_waiting(&mut scheduler);

        drop(first);
        let second = ready(&mut scheduler);
        assert_eq!(second.id(), op(0, 1));
        assert_eq!(second.range(), span(4, 8));
        assert_exhausted(&mut scheduler);
    }

    #[test]
    fn a_fitting_tail_from_a_later_range_passes_a_blocked_full_read() {
        let mut scheduler = scheduler(&[span(0, 8), span(10, 12)], 4, 6);

        let first = ready(&mut scheduler);
        assert_eq!(first.id(), op(0, 0));

        let tail = ready(&mut scheduler);
        assert_eq!(tail.id(), op(1, 0));
        assert_eq!(tail.range(), span(10, 12));
        assert_eq!(counters(&scheduler), (6, 0));

        assert_waiting(&mut scheduler);
    }

    #[test]
    fn the_greatest_fitting_tail_wins_over_an_earlier_smaller_tail() {
        let mut scheduler = scheduler(&[span(0, 2), span(10, 17)], 4, 7);

        let full = ready(&mut scheduler);
        assert_eq!(full.id(), op(1, 0));
        assert_eq!(full.range(), span(10, 14));
        assert_eq!(counters(&scheduler), (4, 3));

        let greatest = ready(&mut scheduler);
        assert_eq!(greatest.id(), op(1, 1));
        assert_eq!(greatest.range(), span(14, 17));
        assert_eq!(counters(&scheduler), (7, 0));

        assert_waiting(&mut scheduler);

        drop(full);
        let smaller = ready(&mut scheduler);
        assert_eq!(smaller.id(), op(0, 0));
        assert_eq!(smaller.range(), span(0, 2));
        assert_exhausted(&mut scheduler);
    }

    #[test]
    fn six_available_bytes_choose_the_four_byte_read_over_two_three_byte_tails() {
        let mut scheduler = scheduler(&[span(0, 4), span(10, 13), span(20, 23)], 4, 6);

        let widest = ready(&mut scheduler);
        assert_eq!(widest.id(), op(0, 0));
        assert_eq!(widest.range(), span(0, 4));
        assert_eq!(counters(&scheduler), (4, 2));

        assert_waiting(&mut scheduler);

        drop(widest);
        assert_eq!(ready(&mut scheduler).id(), op(1, 0));
        assert_eq!(ready(&mut scheduler).id(), op(2, 0));
        assert_exhausted(&mut scheduler);
    }

    #[test]
    fn waiting_for_budget_changes_neither_pending_state_nor_counters() {
        let mut scheduler = scheduler(&[span(0, 10)], 4, 4);

        let first = ready(&mut scheduler);
        assert_eq!(counters(&scheduler), (4, 0));

        assert_waiting(&mut scheduler);
        assert_eq!(counters(&scheduler), (4, 0));
        assert_waiting(&mut scheduler);
        assert_eq!(counters(&scheduler), (4, 0));

        drop(first);
        let second = ready(&mut scheduler);
        assert_eq!(second.id(), op(0, 1));
        assert_eq!(second.range(), span(4, 8));
    }

    #[test]
    fn dropping_a_scheduled_read_restores_exactly_its_length_and_unblocks_work() {
        let mut scheduler = scheduler(&[span(0, 8)], 4, 4);

        let first = ready(&mut scheduler);
        assert_eq!(counters(&scheduler), (4, 0));
        assert_waiting(&mut scheduler);

        drop(first);
        assert_eq!(counters(&scheduler), (0, 4));

        let second = ready(&mut scheduler);
        assert_eq!(second.range(), span(4, 8));
        assert_exhausted(&mut scheduler);
    }

    #[test]
    fn exhausted_is_returned_while_scheduled_reads_are_still_alive() {
        let mut scheduler = scheduler(&[span(0, 4)], 4, 8);

        let only = ready(&mut scheduler);
        assert_exhausted(&mut scheduler);
        assert_eq!(counters(&scheduler), (4, 4));

        drop(only);
        assert_exhausted(&mut scheduler);
        assert_eq!(counters(&scheduler), (0, 8));
    }

    #[test]
    fn every_successful_decision_keeps_in_flight_at_or_under_the_budget() {
        let mut scheduler = scheduler(&[span(0, 14), span(20, 27)], 4, 10);
        let mut live = Vec::new();
        let mut admitted = 0_u64;

        loop {
            match scheduler
                .schedule_next()
                .expect("test scheduling decisions succeed")
            {
                ScheduleDecision::Ready(read) => {
                    admitted += read.range().length();
                    live.push(read);
                    assert!(scheduler.in_flight_bytes() <= 10);
                    assert_eq!(
                        scheduler.in_flight_bytes() + scheduler.available_bytes(),
                        10
                    );
                }
                ScheduleDecision::WaitingForBudget => {
                    assert!(
                        !live.is_empty(),
                        "waiting requires admitted work to release"
                    );
                    drop(live.remove(0));
                }
                ScheduleDecision::Exhausted => break,
            }
        }

        assert_eq!(admitted, 14 + 7);
        drop(live);
        assert_eq!(counters(&scheduler), (0, 10));
        assert_exhausted(&mut scheduler);
    }

    #[test]
    fn a_range_ending_at_the_last_representable_offset_schedules_exactly() {
        let mut scheduler = scheduler(&[span(u64::MAX - 10, u64::MAX)], 4, 10);

        let first = ready(&mut scheduler);
        assert_eq!(first.id(), op(0, 0));
        assert_eq!(first.range(), span(u64::MAX - 10, u64::MAX - 6));

        let second = ready(&mut scheduler);
        assert_eq!(second.id(), op(0, 1));
        assert_eq!(second.range(), span(u64::MAX - 6, u64::MAX - 2));

        let tail = ready(&mut scheduler);
        assert_eq!(tail.id(), op(0, 2));
        assert_eq!(tail.range(), span(u64::MAX - 2, u64::MAX));

        assert_eq!(counters(&scheduler), (10, 0));
        assert_exhausted(&mut scheduler);
    }

    #[test]
    fn the_widest_single_read_is_admitted_without_overflow() {
        let mut scheduler = scheduler(&[span(0, u64::MAX)], u64::MAX, u64::MAX);

        let widest = ready(&mut scheduler);
        assert_eq!(widest.id(), op(0, 0));
        assert_eq!(widest.range(), span(0, u64::MAX));
        assert_eq!(counters(&scheduler), (u64::MAX, 0));

        assert_exhausted(&mut scheduler);
        drop(widest);
        assert_eq!(counters(&scheduler), (0, u64::MAX));
    }

    #[test]
    fn an_unreservable_progress_capacity_is_a_typed_error() {
        match try_reserve_progress(usize::MAX) {
            Err(SchedulerError::StateReservationFailed { capacity, .. }) => {
                assert_eq!(capacity, usize::MAX);
            }
            other => panic!("expected a typed reservation failure, got {other:?}"),
        }
    }

    #[test]
    fn a_tebibyte_plan_schedules_its_first_reads_compactly() {
        let execution = execution(&[span(0, TEBIBYTE)], 4096, 65536);
        assert_eq!(execution.ranges()[0].operation_count(), 268_435_456);

        let mut scheduler =
            Scheduler::try_new(execution).expect("test schedulers construct without failure");

        let mut live = Vec::new();
        for operation_index in 0..16 {
            let read = ready(&mut scheduler);
            assert_eq!(read.id(), op(0, operation_index));
            assert_eq!(
                read.range(),
                span(operation_index * 4096, (operation_index + 1) * 4096)
            );
            live.push(read);
        }

        assert_eq!(counters(&scheduler), (65536, 0));
        assert_waiting(&mut scheduler);
    }
}
