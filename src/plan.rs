//! Pure planning transformations over collections of read ranges.
//!
//! Planning turns a caller's logical schedule into a canonical plan. It stays
//! deterministic and synchronous, and it never touches a file: file sizes,
//! EOF, buffers, and backend execution belong to the read backends
//! ([`read_plan`](crate::read_plan), [`execute_pread`](crate::execute_pread)).
//!
//! The validated boundary type is [`ReadPlan`]; [`coalesce`] is the pure merge
//! used to build it.

use thiserror::Error;

use crate::range::ReadRange;

/// Reason a plan could not be produced from a collection of ranges.
///
/// [`RangeError`](crate::RangeError) protects the construction invariants of a
/// single [`ReadRange`]. `PlanError` describes failures that only exist for a
/// whole schedule.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum PlanError {
    /// The schedule held no range, so there is no plan to produce.
    #[error("cannot plan an empty schedule")]
    EmptySchedule,
    /// A merge produced bounds that no [`ReadRange`] can represent.
    ///
    /// Merging two validated ranges always yields a valid range, so this
    /// variant guards the coalescing arithmetic instead of describing a
    /// reachable input. Reporting it keeps the merge free of a panic path and
    /// free of any silent correction that would return a wrong plan.
    #[error("coalescing produced bounds that no read range can represent")]
    UnrepresentableMerge,
}

/// Reduces a schedule to the minimal set of ranges covering exactly the same
/// bytes.
///
/// The input is borrowed so the caller keeps its original schedule for
/// provenance and reporting, and the returned plan is newly owned. Overlapping
/// and adjacent ranges merge, gaps stay intact, and the output is sorted by
/// ascending offset with neither overlap nor adjacency left between neighbours.
///
/// The returned `Vec<ReadRange>` is canonical, but its type proves nothing: an
/// arbitrary vector looks the same. [`ReadPlan::try_from_schedule`] wraps this
/// same transformation in the validated boundary type the backends accept
/// ([`read_plan`](crate::read_plan) borrows one directly); prefer it whenever
/// the invariants must travel with the value.
///
/// Two ranges merge when `current.offset() <= last.end()`, which covers overlap
/// and exact adjacency in one comparison because the bounds are half-open. A
/// merge keeps `max(end)` rather than summing lengths, so overlapping bytes are
/// never counted twice, and a range contained in its predecessor never shortens
/// it.
///
/// Coalescing is pure: it performs no I/O, consults no file size, leaves the
/// input untouched, and returns the same plan for equal inputs.
///
/// # Errors
///
/// Returns [`PlanError::EmptySchedule`] when `ranges` is empty, and
/// [`PlanError::UnrepresentableMerge`] if a merge produced bounds outside the
/// [`ReadRange`] contract. The second case cannot occur for validated input; it
/// guards a coalescing logic error rather than a caller mistake.
///
/// # Examples
///
/// ```
/// use range_replay::{PlanError, ReadRange, coalesce};
///
/// let schedule = [
///     ReadRange::try_new(10, 2)?,
///     ReadRange::try_new(4, 3)?,
///     ReadRange::try_new(0, 4)?,
/// ];
///
/// let plan = coalesce(&schedule)?;
/// let bounds: Vec<(u64, u64)> = plan.iter().map(|range| (range.offset(), range.end())).collect();
///
/// assert_eq!(bounds, vec![(0, 7), (10, 12)]);
/// assert_eq!(schedule.len(), 3);
///
/// assert_eq!(coalesce(&[]), Err(PlanError::EmptySchedule));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn coalesce(ranges: &[ReadRange]) -> Result<Vec<ReadRange>, PlanError> {
    let mut sorted = ranges.to_vec();
    sorted.sort_unstable_by_key(|range| (range.offset(), range.length()));

    let Some((&first, rest)) = sorted.split_first() else {
        return Err(PlanError::EmptySchedule);
    };

    let mut coalesced = Vec::with_capacity(sorted.len());
    let mut pending = first;

    for &current in rest {
        if current.offset() <= pending.end() {
            if current.end() > pending.end() {
                pending = merged_range(pending.offset(), current.end())?;
            }
        } else {
            coalesced.push(pending);
            pending = current;
        }
    }

    coalesced.push(pending);

    Ok(coalesced)
}

/// A canonical read plan whose construction invariants are established once.
///
/// `ReadPlan` is the validated input boundary for later read backends. A bare
/// `Vec<ReadRange>` cannot promise anything about its contents, so backends
/// accepting one would have to re-check ordering and overlap on every call.
/// A `ReadPlan` can only be built through [`ReadPlan::try_from_schedule`],
/// which guarantees that the owned ranges are sorted by ascending offset with
/// neither overlap nor adjacency between neighbours, and that the plan holds
/// at least one range.
///
/// Construction borrows the caller's schedule and owns the canonical output:
/// the original schedule stays untouched and usable, and the plan remains
/// valid after the schedule has gone out of scope. The ranges are only exposed
/// immutably through [`ReadPlan::ranges`], so no caller can break the
/// invariants after construction.
///
/// # Examples
///
/// ```
/// use range_replay::{PlanError, ReadPlan, ReadRange};
///
/// let schedule = [
///     ReadRange::try_new(10, 2)?,
///     ReadRange::try_new(0, 4)?,
///     ReadRange::try_new(4, 3)?,
/// ];
///
/// let plan = ReadPlan::try_from_schedule(&schedule)?;
/// let bounds: Vec<(u64, u64)> =
///     plan.ranges().iter().map(|range| (range.offset(), range.end())).collect();
///
/// assert_eq!(bounds, vec![(0, 7), (10, 12)]);
/// assert_eq!(schedule.len(), 3);
///
/// assert_eq!(
///     ReadPlan::try_from_schedule(&[]),
///     Err(PlanError::EmptySchedule)
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadPlan {
    ranges: Vec<ReadRange>,
}

impl ReadPlan {
    /// Builds a canonical plan from a borrowed schedule.
    ///
    /// The schedule is coalesced exactly like [`coalesce`]: overlapping and
    /// adjacent ranges merge, gaps stay intact, and equal inputs produce equal
    /// plans. Construction is pure and deterministic: it performs no I/O,
    /// consults no file size, and leaves the borrowed schedule untouched.
    ///
    /// # Errors
    ///
    /// Returns [`PlanError::EmptySchedule`] when `schedule` is empty, and
    /// [`PlanError::UnrepresentableMerge`] if a merge produced bounds outside
    /// the [`ReadRange`] contract. The second case cannot occur for validated
    /// input; it guards a coalescing logic error rather than a caller mistake.
    pub fn try_from_schedule(schedule: &[ReadRange]) -> Result<Self, PlanError> {
        coalesce(schedule).map(|ranges| Self { ranges })
    }

    /// Returns the canonical ranges, sorted by ascending offset with neither
    /// overlap nor adjacency between neighbours.
    ///
    /// The slice is never empty and borrows from the plan, so the invariants
    /// established at construction cannot be broken through it.
    #[must_use]
    pub fn ranges(&self) -> &[ReadRange] {
        &self.ranges
    }
}

fn merged_range(offset: u64, end: u64) -> Result<ReadRange, PlanError> {
    end.checked_sub(offset)
        .and_then(|length| ReadRange::try_new(offset, length).ok())
        .ok_or(PlanError::UnrepresentableMerge)
}

#[cfg(test)]
mod tests {
    use super::{PlanError, ReadPlan, coalesce};
    use crate::range::ReadRange;
    use crate::test_support::span;

    fn bounds(plan: &[ReadRange]) -> Vec<(u64, u64)> {
        plan.iter()
            .map(|range| (range.offset(), range.end()))
            .collect()
    }

    fn coalesced_bounds(schedule: &[ReadRange]) -> Vec<(u64, u64)> {
        let plan = coalesce(schedule).expect("schedule is not empty");

        bounds(&plan)
    }

    #[test]
    fn coalesce_rejects_an_empty_schedule() {
        assert_eq!(coalesce(&[]), Err(PlanError::EmptySchedule));
    }

    #[test]
    fn coalesce_returns_a_single_range_unchanged() {
        assert_eq!(coalesced_bounds(&[span(10, 15)]), vec![(10, 15)]);
    }

    #[test]
    fn coalesce_merges_adjacent_ranges() {
        let schedule = [span(0, 4), span(4, 7), span(10, 12)];

        assert_eq!(coalesced_bounds(&schedule), vec![(0, 7), (10, 12)]);
    }

    #[test]
    fn coalesce_merges_overlapping_ranges() {
        let schedule = [span(10, 15), span(12, 20)];

        assert_eq!(coalesced_bounds(&schedule), vec![(10, 20)]);
    }

    #[test]
    fn coalesce_keeps_the_outer_bounds_of_a_contained_range() {
        let schedule = [span(10, 20), span(12, 15)];

        assert_eq!(coalesced_bounds(&schedule), vec![(10, 20)]);
    }

    #[test]
    fn coalesce_preserves_a_one_byte_gap() {
        let schedule = [span(0, 4), span(5, 7)];

        assert_eq!(coalesced_bounds(&schedule), vec![(0, 4), (5, 7)]);
    }

    #[test]
    fn coalesce_sorts_an_unsorted_schedule() {
        let schedule = [span(10, 12), span(4, 7), span(0, 4)];

        assert_eq!(coalesced_bounds(&schedule), vec![(0, 7), (10, 12)]);
    }

    #[test]
    fn coalesce_collapses_duplicate_ranges() {
        let schedule = [span(10, 15), span(10, 15), span(10, 15)];

        assert_eq!(coalesced_bounds(&schedule), vec![(10, 15)]);
    }

    #[test]
    fn coalesce_leaves_the_schedule_unchanged() {
        let schedule = [span(10, 12), span(4, 7), span(0, 4)];
        let original = schedule;

        let plan = coalesce(&schedule).expect("schedule is not empty");

        assert_eq!(schedule, original);
        assert_ne!(bounds(&plan), bounds(&original));
    }

    #[test]
    fn read_plan_rejects_an_empty_schedule() {
        assert_eq!(
            ReadPlan::try_from_schedule(&[]),
            Err(PlanError::EmptySchedule)
        );
    }

    #[test]
    fn read_plan_matches_the_hand_calculated_fixture() {
        let schedule = [span(10, 12), span(0, 4), span(4, 7)];

        let plan = ReadPlan::try_from_schedule(&schedule).expect("schedule is not empty");

        assert_eq!(bounds(plan.ranges()), vec![(0, 7), (10, 12)]);
    }

    #[test]
    fn read_plan_agrees_with_coalesce() {
        let schedule = [span(10, 15), span(12, 20), span(0, 4), span(5, 7)];

        let plan = ReadPlan::try_from_schedule(&schedule).expect("schedule is not empty");
        let coalesced = coalesce(&schedule).expect("schedule is not empty");

        assert_eq!(plan.ranges(), coalesced.as_slice());
    }

    #[test]
    fn read_plan_leaves_the_schedule_unchanged() {
        let schedule = [span(10, 12), span(4, 7), span(0, 4)];
        let original = schedule;

        let plan = ReadPlan::try_from_schedule(&schedule).expect("schedule is not empty");

        assert_eq!(schedule, original);
        assert_ne!(bounds(plan.ranges()), bounds(&original));
    }

    #[test]
    fn read_plan_outlives_the_schedule() {
        let plan = {
            let schedule = vec![span(10, 12), span(0, 4), span(4, 7)];

            ReadPlan::try_from_schedule(&schedule).expect("schedule is not empty")
        };

        assert_eq!(bounds(plan.ranges()), vec![(0, 7), (10, 12)]);
    }

    #[test]
    fn coalesce_handles_a_plan_ending_at_the_last_representable_offset() {
        let adjacent_at_the_end = [
            span(u64::MAX - 4, u64::MAX - 2),
            span(u64::MAX - 2, u64::MAX),
        ];
        let widest_possible_plan = [span(0, u64::MAX), span(u64::MAX - 1, u64::MAX)];

        assert_eq!(
            coalesced_bounds(&adjacent_at_the_end),
            vec![(u64::MAX - 4, u64::MAX)]
        );
        assert_eq!(coalesced_bounds(&widest_possible_plan), vec![(0, u64::MAX)]);
    }
}
