//! Byte-budget configuration and runtime in-flight accounting.
//!
//! [`ByteBudget`] is the validated, immutable limit. A [`BudgetLimiter`]
//! enforces it at runtime: [`BudgetLimiter::try_reserve`] admits a physical
//! read only when its length still fits under the limit and returns a
//! uniquely owned [`Reservation`] guard whose destruction releases exactly
//! the admitted bytes. For every publicly observable state:
//!
//! ```text
//! 0 <= in_flight_bytes <= limit
//! available_bytes = limit - in_flight_bytes
//! ```
//!
//! No successful call can make the sum of live reservations exceed the
//! limit.
//!
//! The limiter is deliberately single-threaded and lock-free. Multiple
//! reservations alive at once describe admitted work, not parallel
//! execution, so private shared ownership (`Rc`) with interior mutability
//! (`Cell`) is the whole runtime state; the compiler keeps the types on one
//! thread. A threaded or async runtime is an explicit later decision, and
//! the shared representation stays private so that decision cannot leak into
//! this API.
//!
//! This module is accounting only. Nothing here reads a file, allocates
//! buffers, schedules work, waits, or queues; those remain later slices.

use std::cell::Cell;
use std::rc::Rc;

use thiserror::Error;

use crate::range::ReadRange;

/// Reason a [`ByteBudget`] could not be constructed.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum BudgetError {
    /// The requested budget was `0`, so no non-empty read could ever fit.
    #[error("byte budget must be greater than zero")]
    ZeroBudget,
}

/// Reason a [`BudgetLimiter`] can never admit a requested range.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ReservationError {
    /// The requested length exceeds the total limit, so the range could not
    /// be admitted even with nothing in flight.
    #[error("requested {requested} bytes exceed the total byte budget of {limit} bytes")]
    ExceedsBudget {
        /// Length in bytes of the range that could never be admitted.
        requested: u64,
        /// Total byte limit of the rejecting limiter.
        limit: u64,
    },
}

/// A validated, non-zero limit on budgeted bytes.
///
/// The same limit bounds two distinct things: the size of one physical read
/// planned by an [`ExecutionPlan`](crate::ExecutionPlan), and the total
/// bytes a [`BudgetLimiter`] keeps in flight at runtime. A budget of `0` is
/// rejected at construction rather than treated as temporary backpressure:
/// no non-empty read could ever fit under it, so a zero budget can never
/// admit any work and is an invalid configuration instead of a momentarily
/// full one.
///
/// `ByteBudget` is [`Copy`] because it is immutable configuration: copying a
/// limit cannot multiply any capacity. A [`Reservation`] is the opposite —
/// it represents exclusive ownership of admitted in-flight bytes and stays
/// uniquely owned rather than copyable.
///
/// Runtime enforcement exists as a primitive: a [`BudgetLimiter`] tracks the
/// sum of bytes actually in flight and never admits beyond the limit. No
/// scheduler or backend acquires reservations yet; that remains a later
/// slice.
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

/// Single-threaded runtime enforcement of one [`ByteBudget`].
///
/// The limiter tracks the sum of bytes admitted and not yet released, and
/// [`Self::try_reserve`] is the only admission path. It never waits or
/// queues, and it has three semantically distinct outcomes:
///
/// - `Ok(Some(reservation))`: the range is admitted now and its length has been
///   added exactly once to [`Self::in_flight_bytes`].
/// - `Ok(None)`: the range is valid under the total limit but temporarily does
///   not fit in [`Self::available_bytes`]. The state is unchanged.
/// - `Err(`[`ReservationError::ExceedsBudget`]`)`: the range is longer than the
///   total limit and can never be admitted by this limiter. The state is
///   unchanged.
///
/// A future backend must obtain the reservation *before* allocating a
/// per-read buffer or submitting the read: if allocation or submission then
/// fails, dropping the already-created guard restores the capacity. That
/// ordering is documentation for now — no buffer or backend integration
/// exists yet.
///
/// The limiter stays usable while any number of reservations are alive, and
/// it deliberately uses no locking: the runtime state is single-threaded by
/// construction, so a lock could only guard against threads that cannot
/// exist here.
///
/// # Examples
///
/// ```
/// use range_replay::{BudgetLimiter, ByteBudget, ReadRange};
///
/// let limiter = BudgetLimiter::new(ByteBudget::try_new(8)?);
///
/// let first = limiter.try_reserve(ReadRange::try_new(0, 5)?)?;
/// assert!(first.is_some());
/// assert_eq!(limiter.in_flight_bytes(), 5);
/// assert_eq!(limiter.available_bytes(), 3);
///
/// // Four more bytes do not fit right now: temporary, non-mutating.
/// assert!(limiter.try_reserve(ReadRange::try_new(10, 4)?)?.is_none());
/// assert_eq!(limiter.in_flight_bytes(), 5);
///
/// // Dropping the guard releases exactly the admitted bytes.
/// drop(first);
/// assert_eq!(limiter.in_flight_bytes(), 0);
/// assert_eq!(limiter.available_bytes(), 8);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct BudgetLimiter {
    limit: ByteBudget,
    in_flight: Rc<Cell<u64>>,
}

impl BudgetLimiter {
    /// Creates a limiter with no bytes in flight and the full limit
    /// available.
    #[must_use]
    pub fn new(limit: ByteBudget) -> Self {
        Self {
            limit,
            in_flight: Rc::new(Cell::new(0)),
        }
    }

    /// Returns the budget being enforced.
    #[must_use]
    pub const fn limit(&self) -> ByteBudget {
        self.limit
    }

    /// Returns the sum of bytes admitted and not yet released, which never
    /// exceeds the limit.
    #[must_use]
    pub fn in_flight_bytes(&self) -> u64 {
        self.in_flight.get()
    }

    /// Returns the bytes an admission could still take right now.
    #[must_use]
    pub fn available_bytes(&self) -> u64 {
        // `try_reserve` never admits beyond the limit and every release
        // subtracts checked, so the in-flight bytes never exceed the limit
        // and the subtraction cannot fail. A hypothetically corrupt state
        // fails closed through the explicit branch: it reports zero
        // available instead of silently saturating or wrapping.
        let Some(available) = self.limit.bytes().checked_sub(self.in_flight.get()) else {
            return 0;
        };

        available
    }

    /// Attempts to admit `range` under the budget without waiting.
    ///
    /// Returns `Ok(Some(reservation))` when the range fits in the currently
    /// available bytes, adding its length exactly once to the in-flight
    /// bytes, and `Ok(None)` when the range is admissible in principle but
    /// temporarily does not fit; the state is then unchanged and the caller
    /// decides when to retry.
    ///
    /// # Errors
    ///
    /// Returns [`ReservationError::ExceedsBudget`] when the range is longer
    /// than the total limit and could never be admitted by this limiter,
    /// even with nothing in flight. The state is unchanged. A physical read
    /// produced by an [`ExecutionPlan`](crate::ExecutionPlan) derived with
    /// the same budget never exceeds it, so such reads cannot hit this
    /// error.
    pub fn try_reserve(&self, range: ReadRange) -> Result<Option<Reservation>, ReservationError> {
        let requested = range.length();
        let limit = self.limit.bytes();

        if requested > limit {
            return Err(ReservationError::ExceedsBudget { requested, limit });
        }

        let in_flight = self.in_flight.get();
        let Some(available) = limit.checked_sub(in_flight) else {
            // Unreachable: no admission path lets the in-flight bytes
            // exceed the limit. Fail closed by admitting nothing and
            // mutating nothing.
            return Ok(None);
        };

        if requested > available {
            return Ok(None);
        }

        let Some(updated) = in_flight.checked_add(requested) else {
            // Unreachable: `requested` fits in `limit - in_flight`, so the
            // sum stays at or under the limit. Fail closed by admitting
            // nothing instead of wrapping the accounting.
            return Ok(None);
        };

        self.in_flight.set(updated);

        Ok(Some(Reservation {
            range,
            in_flight: Rc::clone(&self.in_flight),
        }))
    }
}

/// Exclusive ownership of the in-flight bytes admitted for one physical
/// read.
///
/// Only a successful [`BudgetLimiter::try_reserve`] constructs a value, and
/// the type is deliberately neither [`Clone`] nor [`Copy`]: one successful
/// admission creates exactly one guard and therefore exactly one release.
///
/// Dropping the guard releases exactly the length of [`Self::range`] back to
/// its limiter, independent of how the guarded work ended — successful
/// completion, an I/O error propagated with `?`, cancellation, or any other
/// early return. Guards may be dropped in any order, and a guard may outlive
/// the limiter handle because it owns the shared state needed for its
/// release.
///
/// As with standard RAII guards, deliberately leaking a reservation with
/// [`std::mem::forget`] can leak capacity because Rust does not guarantee
/// that destructors run; even then the accounting fails closed and the hard
/// limit cannot be exceeded.
///
/// The guard carries no logical-range identity, operation index, buffer, or
/// backend state; a future scheduler pairs those concerns with it. It
/// reserves the actual physical range length only.
#[derive(Debug)]
#[must_use = "dropping a reservation immediately releases its admitted bytes"]
pub struct Reservation {
    range: ReadRange,
    in_flight: Rc<Cell<u64>>,
}

impl Reservation {
    /// Returns the physical range whose length this reservation admitted.
    #[must_use]
    pub const fn range(&self) -> ReadRange {
        self.range
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // Private construction and unique ownership admit each guard's
        // length exactly once before the guard exists, so the subtraction
        // cannot underflow through the public API. A hypothetically corrupt
        // state fails closed: the release is skipped entirely rather than
        // clamped, so the hard limit still cannot be exceeded.
        if let Some(in_flight) = self.in_flight.get().checked_sub(self.range.length()) {
            self.in_flight.set(in_flight);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BudgetError, BudgetLimiter, ByteBudget, Reservation, ReservationError};
    use crate::execution::ExecutionPlan;
    use crate::plan::ReadPlan;
    use crate::range::ReadRange;

    fn span(start: u64, end: u64) -> ReadRange {
        ReadRange::try_new(start, end - start).expect("test spans are valid ranges")
    }

    fn budget(bytes: u64) -> ByteBudget {
        ByteBudget::try_new(bytes).expect("test budgets are non-zero")
    }

    fn admitted(limiter: &BudgetLimiter, range: ReadRange) -> Reservation {
        limiter
            .try_reserve(range)
            .expect("test range fits under the total limit")
            .expect("test range fits in the available bytes")
    }

    fn counters(limiter: &BudgetLimiter) -> (u64, u64) {
        (limiter.in_flight_bytes(), limiter.available_bytes())
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
    fn a_new_limiter_starts_empty_with_the_full_limit_available() {
        let limiter = BudgetLimiter::new(budget(8));

        assert_eq!(limiter.limit(), budget(8));
        assert_eq!(counters(&limiter), (0, 8));
    }

    #[test]
    fn one_reservation_preserves_its_range_and_updates_both_counters() {
        let limiter = BudgetLimiter::new(budget(8));

        let reservation = admitted(&limiter, span(0, 5));

        assert_eq!(reservation.range(), span(0, 5));
        assert_eq!(counters(&limiter), (5, 3));
    }

    #[test]
    fn the_hand_calculated_eight_byte_scenario_holds() {
        let limiter = BudgetLimiter::new(budget(8));

        let a = admitted(&limiter, span(0, 5));
        assert_eq!(counters(&limiter), (5, 3));

        let b = admitted(&limiter, span(10, 13));
        assert_eq!(counters(&limiter), (8, 0));

        assert!(
            limiter
                .try_reserve(span(20, 21))
                .expect("one byte is under the total limit")
                .is_none()
        );
        assert_eq!(counters(&limiter), (8, 0));

        drop(a);
        assert_eq!(counters(&limiter), (3, 5));

        let c = admitted(&limiter, span(20, 21));
        assert_eq!(counters(&limiter), (4, 4));

        drop(c);
        assert_eq!(counters(&limiter), (3, 5));

        drop(b);
        assert_eq!(counters(&limiter), (0, 8));
    }

    #[test]
    fn multiple_live_reservations_may_fill_the_limit_exactly() {
        let limiter = BudgetLimiter::new(budget(8));

        let _first = admitted(&limiter, span(0, 3));
        let _second = admitted(&limiter, span(10, 13));
        let _third = admitted(&limiter, span(20, 22));

        assert_eq!(counters(&limiter), (8, 0));
    }

    #[test]
    fn a_temporarily_unavailable_reservation_leaves_the_state_unchanged() {
        let limiter = BudgetLimiter::new(budget(8));
        let _held = admitted(&limiter, span(0, 6));

        assert!(
            limiter
                .try_reserve(span(10, 13))
                .expect("three bytes are under the total limit")
                .is_none()
        );
        assert_eq!(counters(&limiter), (6, 2));
    }

    #[test]
    fn dropping_one_of_several_reservations_restores_only_its_own_length() {
        let limiter = BudgetLimiter::new(budget(8));

        let _first = admitted(&limiter, span(0, 5));
        let second = admitted(&limiter, span(10, 12));

        drop(second);
        assert_eq!(counters(&limiter), (5, 3));
    }

    #[test]
    fn any_destruction_order_returns_the_limiter_exactly_to_zero() {
        let limiter = BudgetLimiter::new(budget(8));

        let first = admitted(&limiter, span(0, 3));
        let second = admitted(&limiter, span(10, 13));
        let third = admitted(&limiter, span(20, 22));

        drop(second);
        drop(third);
        drop(first);
        assert_eq!(counters(&limiter), (0, 8));

        let first = admitted(&limiter, span(0, 3));
        let second = admitted(&limiter, span(10, 13));
        let third = admitted(&limiter, span(20, 22));

        drop(first);
        drop(second);
        drop(third);
        assert_eq!(counters(&limiter), (0, 8));
    }

    #[test]
    fn a_request_longer_than_the_limit_is_a_permanent_typed_error() {
        let limiter = BudgetLimiter::new(budget(8));

        assert_eq!(
            limiter
                .try_reserve(span(0, 9))
                .map(|admitted| admitted.map(|reservation| reservation.range())),
            Err(ReservationError::ExceedsBudget {
                requested: 9,
                limit: 8,
            })
        );
        assert_eq!(counters(&limiter), (0, 8));
    }

    #[test]
    fn an_exact_limit_request_fills_the_limiter_until_dropped() {
        let limiter = BudgetLimiter::new(budget(8));

        let exact = admitted(&limiter, span(0, 8));
        assert_eq!(counters(&limiter), (8, 0));
        assert!(
            limiter
                .try_reserve(span(10, 11))
                .expect("one byte is under the total limit")
                .is_none()
        );

        drop(exact);
        assert_eq!(counters(&limiter), (0, 8));
        assert_eq!(admitted(&limiter, span(10, 11)).range(), span(10, 11));
    }

    #[test]
    fn an_error_propagated_past_the_guard_restores_capacity() {
        fn read_and_fail(limiter: &BudgetLimiter, range: ReadRange) -> Result<(), &'static str> {
            let _reservation = limiter
                .try_reserve(range)
                .map_err(|_oversized| "over the total limit")?
                .ok_or("temporarily unavailable")?;

            Err("simulated backend failure")
        }

        let limiter = BudgetLimiter::new(budget(8));

        assert_eq!(
            read_and_fail(&limiter, span(0, 5)),
            Err("simulated backend failure")
        );
        assert_eq!(counters(&limiter), (0, 8));
    }

    #[test]
    fn a_reservation_may_outlive_the_limiter_handle() {
        let reservation = {
            let limiter = BudgetLimiter::new(budget(8));

            admitted(&limiter, span(0, 5))
        };

        assert_eq!(reservation.range(), span(0, 5));
        drop(reservation);
    }

    #[test]
    fn the_maximum_limit_and_the_widest_range_do_not_overflow() {
        let limiter = BudgetLimiter::new(budget(u64::MAX));
        let widest = span(0, u64::MAX);

        let reservation = admitted(&limiter, widest);
        assert_eq!(reservation.range(), widest);
        assert_eq!(counters(&limiter), (u64::MAX, 0));

        drop(reservation);
        assert_eq!(counters(&limiter), (0, u64::MAX));
    }

    #[test]
    fn a_physical_read_from_a_compact_execution_plan_is_admitted_unchanged() {
        let schedule = [span(0, 16)];
        let plan = ReadPlan::try_from_schedule(&schedule).expect("test schedules are not empty");
        let execution = ExecutionPlan::try_from_read_plan(&plan, budget(8))
            .expect("test plans derive without failure");

        let planned = &execution.ranges()[0];
        let read = planned
            .physical_read(0)
            .expect("test lookups stay within the generation contract")
            .expect("index zero exists for a non-empty planned range");

        let limiter = BudgetLimiter::new(execution.budget());
        let reservation = admitted(&limiter, read);

        assert_eq!(reservation.range(), span(0, 8));
        assert_eq!(counters(&limiter), (8, 0));
        assert_eq!(planned.operation_count(), 2);
    }
}
