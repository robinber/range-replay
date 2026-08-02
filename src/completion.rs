//! Backend-neutral completion of one admitted physical read.
//!
//! A [`CompletedRead`] is the exact result of one physical read a backend
//! finished successfully: the owned bytes covering the admitted physical
//! range completely, paired with the [`ScheduledRead`] admission that
//! authorized the read. Holding the admission keeps its
//! [`Reservation`](crate::Reservation) live, so the bytes a completion
//! occupies stay accounted to the in-flight budget until the completion is
//! destroyed.
//!
//! The value is deliberately backend-neutral: nothing here reads a file,
//! chooses a backend, or assembles logical output. The synchronous adapter
//! [`read_scheduled`](crate::read_scheduled) is the only construction path
//! today. A completion is also distinct from the logical
//! [`RangeOutput`](crate::RangeOutput): a completion covers one *physical*
//! operation of an execution plan, while a range output covers one complete
//! canonical *logical* range. No slice assembles physical completions into
//! logical outputs yet.

use crate::range::ReadRange;
use crate::scheduler::{OperationId, ScheduledRead};

/// Reason a completion could not be constructed from a buffer and an
/// admission whose lengths disagree.
///
/// Crate-internal: the backend adapter that attempted the construction maps
/// this into its own typed error with the range context only it knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LengthMismatch {
    /// Byte count the admitted physical range requires.
    pub(crate) expected: u64,
    /// Byte count the rejected buffer actually holds.
    pub(crate) actual: usize,
}

/// One successfully and exactly completed physical read.
///
/// A value pairs the owned bytes of one physical read with the
/// [`ScheduledRead`] admission that authorized it. Only a successful exact
/// read constructs one, so three invariants always hold:
///
/// - [`Self::id`] is exactly the admitted operation's identity;
/// - [`Self::range`] is exactly the admitted physical range;
/// - [`Self::bytes`] covers that range completely: its length equals the range
///   length.
///
/// # Budget lifetime
///
/// The completion keeps the admission's reservation live for its whole
/// lifetime: while the bytes wait for a future consumer and while callers
/// borrow [`Self::bytes`], the range's length stays counted in the
/// scheduler's in-flight bytes. The physical buffer field is declared
/// before the scheduled handle, so normal field destruction order destroys
/// the buffer first and releases the reservation only afterwards; the
/// budget can never admit replacement work while an old physical buffer
/// still occupies the bytes accounted to its operation.
///
/// # Not global success
///
/// A completion proves only that its own physical range was read exactly.
/// Other admitted operations may still fail, so it is never proof that the
/// whole plan executed successfully; only a future executor could decide
/// that, and none exists yet.
///
/// Fields stay private and no public constructor exists, so a completion
/// cannot be forged, mutated into a length mismatch, or detached from its
/// reservation. The type is deliberately neither [`Clone`] nor [`Copy`]:
/// duplicating it would duplicate the apparent ownership of one unique
/// admission and physical completion.
///
/// ```compile_fail
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<range_replay::CompletedRead>();
/// ```
///
/// The complete struct literal below fails only because the fields are
/// private, so it would start compiling — and fail this test — if the
/// fields ever became public:
///
/// ```compile_fail
/// fn forge(
///     bytes: Vec<u8>,
///     scheduled: range_replay::ScheduledRead,
/// ) -> range_replay::CompletedRead {
///     range_replay::CompletedRead { bytes, scheduled }
/// }
/// ```
///
/// ```compile_fail
/// fn tamper(completed: &mut range_replay::CompletedRead) {
///     completed.bytes.pop();
/// }
/// ```
#[derive(Debug)]
#[must_use = "dropping a completed read destroys its bytes and releases its admitted budget bytes"]
pub struct CompletedRead {
    // Declared before `scheduled` so field destruction order destroys the
    // physical buffer before the reservation releases.
    bytes: Vec<u8>,
    scheduled: ScheduledRead,
}

impl CompletedRead {
    /// Builds a completion from an exactly filled buffer and the admission
    /// that authorized it.
    ///
    /// Crate-internal: only backend adapters construct completions. The
    /// buffer length is compared to the admitted range length through a
    /// checked conversion; on any disagreement both parts are destroyed —
    /// releasing the reservation — and a typed mismatch is returned instead
    /// of an invalid completion.
    pub(crate) fn try_new(
        bytes: Vec<u8>,
        scheduled: ScheduledRead,
    ) -> Result<Self, LengthMismatch> {
        let expected = scheduled.range().length();
        let actual = bytes.len();

        if u64::try_from(actual).is_ok_and(|converted| converted == expected) {
            Ok(Self { bytes, scheduled })
        } else {
            // Parameters would drop in reverse declaration order, releasing
            // the reservation while the rejected buffer still exists; drop
            // both explicitly to keep buffer-before-reservation destruction
            // on this path too.
            drop(bytes);
            drop(scheduled);
            Err(LengthMismatch { expected, actual })
        }
    }

    /// Returns the stable identity of the completed physical read.
    #[must_use]
    pub const fn id(&self) -> OperationId {
        self.scheduled.id()
    }

    /// Returns the exact physical range the bytes cover.
    #[must_use]
    pub const fn range(&self) -> ReadRange {
        self.scheduled.range()
    }

    /// Returns the bytes covering the physical range, whose length always
    /// equals the range length.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::{CompletedRead, LengthMismatch};
    use crate::budget::ByteBudget;
    use crate::execution::{ExecutionConfig, ExecutionPlan, ReadSize};
    use crate::plan::ReadPlan;
    use crate::range::ReadRange;
    use crate::scheduler::{ScheduleDecision, ScheduledRead, Scheduler};

    fn admitted_single(offset: u64, length: u64, budget_bytes: u64) -> (Scheduler, ScheduledRead) {
        let range = ReadRange::try_new(offset, length).expect("test ranges are valid");
        let plan = ReadPlan::try_from_schedule(&[range]).expect("test schedules are not empty");
        let read_size = ReadSize::try_new(length).expect("test read sizes are non-zero");
        let budget = ByteBudget::try_new(budget_bytes).expect("test budgets are non-zero");
        let config = ExecutionConfig::try_new(read_size, budget)
            .expect("test configurations pair a read size with a large enough budget");
        let execution = ExecutionPlan::try_from_read_plan(&plan, config)
            .expect("test plans derive without failure");
        let mut scheduler =
            Scheduler::try_new(execution).expect("test schedulers construct without failure");

        match scheduler
            .schedule_next()
            .expect("test scheduling decisions succeed")
        {
            ScheduleDecision::Ready(read) => (scheduler, read),
            decision => panic!("expected a ready decision, got {decision:?}"),
        }
    }

    #[test]
    fn try_new_preserves_identity_range_and_bytes() {
        let (scheduler, admission) = admitted_single(2, 3, 3);
        let id = admission.id();

        let completed = CompletedRead::try_new(b"234".to_vec(), admission)
            .expect("three bytes match the three-byte range");

        assert_eq!(completed.id(), id);
        assert_eq!(
            completed.range(),
            ReadRange::try_new(2, 3).expect("test ranges are valid")
        );
        assert_eq!(completed.bytes(), b"234");
        assert_eq!(scheduler.in_flight_bytes(), 3);
    }

    #[test]
    fn the_reservation_stays_counted_until_the_completion_is_destroyed() {
        let (scheduler, admission) = admitted_single(2, 3, 3);

        let completed = CompletedRead::try_new(b"234".to_vec(), admission)
            .expect("three bytes match the three-byte range");

        assert_eq!(scheduler.in_flight_bytes(), 3);
        assert_eq!(scheduler.available_bytes(), 0);

        drop(completed);
        assert_eq!(scheduler.in_flight_bytes(), 0);
        assert_eq!(scheduler.available_bytes(), 3);
    }

    #[test]
    fn a_short_buffer_is_rejected_and_releases_the_reservation() {
        let (scheduler, admission) = admitted_single(2, 3, 3);

        let error = CompletedRead::try_new(b"23".to_vec(), admission)
            .expect_err("two bytes cannot complete a three-byte range");

        assert_eq!(
            error,
            LengthMismatch {
                expected: 3,
                actual: 2,
            }
        );
        assert_eq!(scheduler.in_flight_bytes(), 0);
        assert_eq!(scheduler.available_bytes(), 3);
    }

    #[test]
    fn an_overlong_buffer_is_rejected_and_releases_the_reservation() {
        let (scheduler, admission) = admitted_single(2, 3, 3);

        let error = CompletedRead::try_new(b"2345".to_vec(), admission)
            .expect_err("four bytes cannot complete a three-byte range");

        assert_eq!(
            error,
            LengthMismatch {
                expected: 3,
                actual: 4,
            }
        );
        assert_eq!(scheduler.in_flight_bytes(), 0);
        assert_eq!(scheduler.available_bytes(), 3);
    }
}
