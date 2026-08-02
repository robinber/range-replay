//! Backend-neutral logical range outputs and their assembly from physical
//! completions.
//!
//! A [`RangeOutput`] is one complete canonical *logical* range of a
//! [`ReadPlan`](crate::ReadPlan) together with the owned bytes covering it
//! exactly. It is the logical counterpart of the physical [`CompletedRead`]:
//! a completion covers one admitted physical operation of an
//! [`ExecutionPlan`] and keeps its budget reservation live, while a range
//! output holds no reservation and becomes observable only once every
//! logical byte is present.
//!
//! An [`OutputAssembler`] connects the two. It is prepared fallibly from one
//! execution plan *before* any backend work runs: every final logical buffer
//! and all per-range state are allocated up front, so no output allocation
//! can fail mid-execution. Those final buffers are deliberately outside the
//! in-flight [`ByteBudget`](crate::ByteBudget), which bounds physical
//! buffers in flight only; no total-process-memory claim is made, and a
//! compact plan over a huge logical range may fail output preparation even
//! though planning succeeded.
//!
//! Physical completions may arrive in any order: the scheduler may reorder
//! fitting tails past blocked full reads, and a later backend may complete
//! submitted operations in any order. Each recorded completion is validated
//! against the retained plan metadata — identity, expected physical range,
//! and checked destination bounds — before any byte or counter changes, then
//! copied directly to its relative destination
//! `physical_offset - logical_offset`. Progress per logical range is one
//! compact `remaining_bytes` counter; no per-operation status, bitmap,
//! queue, or map entry is ever materialized, so retained state stays
//! proportional to the number of logical ranges.
//!
//! Completions must come from the scheduler run paired with the plan the
//! assembler was prepared from. Completions from a *different* plan are
//! rejected by the expected/actual range comparison, but two independent
//! runs built from byte-for-byte identical plans are indistinguishable
//! here; cross-run provenance is future executor/session work and is
//! deliberately not claimed.
//!
//! Finalization is fail-closed: no [`RangeOutput`] is observable until every
//! logical range is complete, and a successful [`OutputAssembler::finish`]
//! moves each buffer into its output in plan order without recopying payload
//! bytes or allocating. [`OutputAssembler::is_complete`] means only that
//! every logical byte was integrated — it is not global execution success,
//! which only a future executor loop over scheduler and backend could
//! decide. No such executor exists yet, and nothing here schedules work,
//! reads a file, or chooses a backend.

use std::collections::TryReserveError;
use std::num::TryFromIntError;
use std::ops::Range;

use thiserror::Error;

use crate::completion::CompletedRead;
use crate::execution::{ExecutionPlan, ExecutionPlanError, PlannedRange};
use crate::range::ReadRange;
use crate::scheduler::OperationId;

/// Reason an [`OutputAssembler`] could not be prepared, record a completion,
/// or finish.
///
/// Preparation failures occur before any backend work runs; recording and
/// finalization failures leave every previously assembled logical byte and
/// counter unchanged. An error never stands for scheduler backpressure,
/// backend I/O failure, global execution failure, or cancellation — those
/// belong to other boundaries.
#[derive(Debug, Error)]
pub enum AssemblyError {
    /// The per-logical-range state or the reserved output capacity could
    /// not be allocated.
    ///
    /// Both allocations are one entry per logical range, bounded by the
    /// already-materialized plan, and are reserved fallibly before any
    /// logical buffer exists, so an assembler is either built completely or
    /// fails with this variant instead of aborting mid-way.
    #[error("cannot reserve assembler state for {capacity} logical ranges")]
    StateReservationFailed {
        /// Number of logical ranges whose state reservation failed.
        capacity: usize,
        /// Allocator failure reported by the reservation.
        #[source]
        source: TryReserveError,
    },
    /// A logical range length does not fit in `usize`, so no final buffer of
    /// that size is representable on this platform.
    #[error(
        "range [{}, {}): length {} is not representable as a buffer size",
        .range.offset(),
        .range.end(),
        .range.length()
    )]
    UnrepresentableLength {
        /// The logical range whose length no buffer can represent.
        range: ReadRange,
        /// The underlying integer conversion failure.
        source: TryFromIntError,
    },
    /// A final logical buffer could not be reserved.
    #[error(
        "range [{}, {}): cannot reserve a {}-byte logical output buffer",
        .range.offset(),
        .range.end(),
        .range.length()
    )]
    BufferAllocation {
        /// The logical range whose buffer reservation failed.
        range: ReadRange,
        /// The underlying reservation failure reported by Rust.
        source: TryReserveError,
    },
    /// Generating the expected physical read of a recorded completion
    /// failed.
    ///
    /// This cannot occur for completions of a plan built from validated
    /// inputs; it preserves the guard of the plan's generation arithmetic
    /// instead of describing a reachable caller mistake.
    #[error("generating the expected physical read of {id} failed")]
    PhysicalReadFailed {
        /// Identity of the completion whose expected read failed.
        id: OperationId,
        /// Generation failure reported by the plan.
        #[source]
        source: ExecutionPlanError,
    },
    /// The prepared plan has no physical read for a recorded completion.
    ///
    /// Either the logical range index or the operation index of the
    /// completion lies outside the plan the assembler was prepared from,
    /// so the completion belongs to a different execution plan.
    #[error("the prepared plan has no physical read for {id}")]
    PhysicalReadMissing {
        /// Identity of the completion without an expected physical read.
        id: OperationId,
    },
    /// A recorded completion covers a different physical range than the
    /// prepared plan expects for its identity.
    ///
    /// The completion was produced for a different execution plan; the
    /// assembler rejects it without touching any logical byte or counter.
    #[error(
        "{id}: expected physical range [{}, {}), completed range is [{}, {})",
        .expected.offset(),
        .expected.end(),
        .actual.offset(),
        .actual.end()
    )]
    RangeMismatch {
        /// Identity of the rejected completion.
        id: OperationId,
        /// Physical range the prepared plan expects for that identity.
        expected: ReadRange,
        /// Physical range the completion actually covers.
        actual: ReadRange,
    },
    /// The checked destination arithmetic or bounds of a recorded
    /// completion left its logical buffer.
    ///
    /// This cannot occur for a completion that already matched its expected
    /// physical range; it guards the destination arithmetic and slice
    /// bounds instead of describing a reachable input, keeping recording
    /// free of any panic, wrap, or silent-narrowing path.
    #[error(
        "{id}: physical range [{}, {}) has no exact destination inside logical range [{}, {})",
        .physical.offset(),
        .physical.end(),
        .logical.offset(),
        .logical.end()
    )]
    InvalidDestination {
        /// Identity of the rejected completion.
        id: OperationId,
        /// Physical range that has no valid destination.
        physical: ReadRange,
        /// Logical range whose buffer was the copy target.
        logical: ReadRange,
    },
    /// Recording a completion would subtract more bytes than its logical
    /// range still has remaining.
    ///
    /// Within one correctly paired scheduling run this cannot occur —
    /// the scheduler returns each operation once and one completion
    /// consumes it — but completions from an independent run over a
    /// byte-for-byte identical plan are indistinguishable and can reach
    /// this guard. The counter is rejected instead of wrapped or
    /// saturated, and no logical byte changes.
    #[error(
        "{id}: recording {length} bytes would underflow the {remaining} bytes remaining for \
         logical range [{}, {})",
        .logical.offset(),
        .logical.end()
    )]
    RemainingBytesUnderflow {
        /// Identity of the rejected completion.
        id: OperationId,
        /// Logical range whose remaining bytes would underflow.
        logical: ReadRange,
        /// Bytes the logical range still has remaining.
        remaining: u64,
        /// Byte length the rejected completion would have subtracted.
        length: u64,
    },
    /// Finalization was requested while a logical range was still
    /// incomplete.
    ///
    /// No output is exposed; the consumed assembler drops every private
    /// buffer.
    #[error(
        "range [{}, {}): {remaining} of {} bytes were never recorded",
        .range.offset(),
        .range.end(),
        .range.length()
    )]
    Incomplete {
        /// The first logical range in plan order that is still incomplete.
        range: ReadRange,
        /// Exact byte count the range is still missing.
        remaining: u64,
    },
    /// A fully assembled buffer does not cover its logical range exactly.
    ///
    /// Construction sizes every buffer to its exact logical length and no
    /// recording path resizes one, so this variant guards output
    /// construction instead of describing a reachable input.
    #[error(
        "range [{}, {}): an assembled buffer holds {actual} bytes for a \
         {expected}-byte logical range",
        .range.offset(),
        .range.end()
    )]
    OutputLengthMismatch {
        /// The logical range the buffer was meant to cover.
        range: ReadRange,
        /// The byte count the range requires.
        expected: u64,
        /// The byte count the rejected buffer actually holds.
        actual: usize,
    },
}

/// Reason a [`RangeOutput`] could not be constructed from a range and a
/// buffer whose lengths disagree.
///
/// Crate-internal: the construction site maps this into its own typed error
/// with the context only it knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutputLengthMismatch {
    /// Byte count the logical range requires.
    pub(crate) expected: u64,
    /// Byte count the rejected buffer actually holds.
    pub(crate) actual: usize,
}

/// One fully read logical range and the owned bytes covering it exactly.
///
/// A value only exists after every byte of its range was read successfully,
/// so the bytes always cover the range completely: `bytes().len()` equals
/// the range length. Both fields stay private and construction is
/// crate-internal, so no caller can construct or mutate an output that
/// breaks this invariant.
///
/// A range output is the backend-neutral *logical* counterpart of the
/// physical [`CompletedRead`]: it covers one complete canonical logical
/// range of a [`ReadPlan`](crate::ReadPlan) and holds no budget
/// reservation, while a completion covers one admitted physical operation
/// and keeps its reservation live. [`read_plan`](crate::read_plan) produces
/// outputs directly through its exact-read loop, and an [`OutputAssembler`]
/// produces them by assembling out-of-order physical completions.
#[derive(Debug, PartialEq, Eq)]
pub struct RangeOutput {
    range: ReadRange,
    bytes: Vec<u8>,
}

impl RangeOutput {
    /// Builds an output from a logical range and the buffer covering it.
    ///
    /// Crate-internal: only the exact-read loop of `pread` and the
    /// assembler construct outputs. The buffer length is compared to the
    /// range length through a checked conversion; on any disagreement a
    /// typed mismatch is returned instead of an invalid output.
    pub(crate) fn try_new(range: ReadRange, bytes: Vec<u8>) -> Result<Self, OutputLengthMismatch> {
        let expected = range.length();
        let actual = bytes.len();

        if u64::try_from(actual).is_ok_and(|converted| converted == expected) {
            Ok(Self { range, bytes })
        } else {
            Err(OutputLengthMismatch { expected, actual })
        }
    }

    /// Returns the range the bytes cover.
    #[must_use]
    pub const fn range(&self) -> ReadRange {
        self.range
    }

    /// Returns the bytes covering the range, whose length always equals the
    /// range length.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Compact assembly state of one logical range.
///
/// Exactly the planned range metadata, the final logical buffer, and the
/// remaining byte counter are retained — never any per-operation entry, so
/// state stays proportional to the logical ranges. The cloned
/// [`PlannedRange`] is metadata-only and is the single source of truth for
/// each expected physical read through its indexed lookup.
#[derive(Debug)]
struct RangeEntry {
    planned: PlannedRange,
    buffer: Vec<u8>,
    remaining_bytes: u64,
}

/// Fallibly reserves the compact entry allocation for `capacity` logical
/// ranges, so construction either completes or fails with a typed error
/// before any logical buffer is allocated.
fn try_reserve_state(capacity: usize) -> Result<Vec<RangeEntry>, AssemblyError> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(capacity)
        .map_err(|source| AssemblyError::StateReservationFailed { capacity, source })?;

    Ok(entries)
}

/// Computes the destination byte span of one expected physical range inside
/// its logical buffer.
///
/// The start is the relative offset `physical_offset - logical_offset` and
/// the end adds the physical length, all through checked arithmetic and
/// checked `usize` conversions. `None` reports any inconsistency — a
/// physical range starting before or ending after the logical buffer, or
/// unrepresentable bounds — instead of wrapping, truncating, or panicking.
fn destination_span(
    logical: ReadRange,
    physical: ReadRange,
    buffer_length: usize,
) -> Option<Range<usize>> {
    let start = physical.offset().checked_sub(logical.offset())?;
    let end = start.checked_add(physical.length())?;

    let start = usize::try_from(start).ok()?;
    let end = usize::try_from(end).ok()?;

    (end <= buffer_length).then_some(start..end)
}

/// Fail-fast assembly of out-of-order physical completions into logical
/// [`RangeOutput`] values.
///
/// [`Self::try_new`] borrows an [`ExecutionPlan`] only during construction
/// and prepares every logical destination before any backend operation is
/// submitted: all per-range state, the output capacity for finalization,
/// and one exact final buffer per logical range are allocated fallibly up
/// front. A successfully built assembler owns everything it needs and stays
/// valid after the source plan is dropped or moved into a
/// [`Scheduler`](crate::Scheduler).
///
/// [`Self::record`] consumes one [`CompletedRead`] at a time, in whatever
/// order completions arrive. Every fallible condition — identity, expected
/// physical range, destination arithmetic, bounds, and the remaining-byte
/// counter — is validated before any byte is copied or any counter changes,
/// so a rejected completion leaves the assembled state byte-for-byte
/// unchanged. The completion keeps its physical buffer and budget
/// reservation alive through validation and copying and is destroyed only
/// afterwards, releasing the reservation; on error it is destroyed and
/// released the same way.
///
/// Completions must come from the scheduler run paired with the plan the
/// assembler was prepared from. Within one such run duplicates are
/// structurally excluded — the scheduler returns each operation once and
/// one completion consumes it — while completions from an independent run
/// over a byte-for-byte identical plan cannot be distinguished; cross-run
/// provenance is future executor work and is not claimed here.
///
/// [`Self::is_complete`] and a successful [`Self::finish`] mean only that
/// every logical byte was integrated. Neither implies the scheduler is
/// exhausted, no backend work remains, or the execution globally
/// succeeded; a future executor decides those separately.
///
/// # Examples
///
/// The hand-calculated fixture: one logical range `[0, 14)` split at read
/// size 4 into `A = [0, 4)`, `B = [4, 8)`, `C = [8, 12)`, and `D = [12, 14)`
/// under a 10-byte budget, completed in the order B, D, A, C.
///
/// ```
/// use std::fs::File;
/// use std::io::Write;
///
/// use range_replay::{
///     ByteBudget, ExecutionConfig, ExecutionPlan, OutputAssembler, ReadPlan, ReadRange,
///     ReadSize, ScheduleDecision, Scheduler, read_scheduled,
/// };
///
/// # fn ready(decision: ScheduleDecision) -> range_replay::ScheduledRead {
/// #     match decision {
/// #         ScheduleDecision::Ready(read) => read,
/// #         other => panic!("expected a ready decision, got {other:?}"),
/// #     }
/// # }
/// let path = std::env::temp_dir()
///     .join(format!("range-replay-doc-output-assembler-{}", std::process::id()));
/// File::create_new(&path)?.write_all(b"abcdefghijklmn")?;
/// let file = File::open(&path)?;
///
/// let plan = ReadPlan::try_from_schedule(&[ReadRange::try_new(0, 14)?])?;
/// let config = ExecutionConfig::try_new(ReadSize::try_new(4)?, ByteBudget::try_new(10)?)?;
/// let execution = ExecutionPlan::try_from_read_plan(&plan, config)?;
///
/// // Prepare every logical destination before the plan moves into the
/// // scheduler and before any backend work runs.
/// let mut assembler = OutputAssembler::try_new(&execution)?;
/// let mut scheduler = Scheduler::try_new(execution)?;
///
/// // The greedy policy admits A, B, and the fitting tail D.
/// let a = read_scheduled(&file, ready(scheduler.schedule_next()?))?;
/// let b = read_scheduled(&file, ready(scheduler.schedule_next()?))?;
/// let d = read_scheduled(&file, ready(scheduler.schedule_next()?))?;
///
/// // Recording B releases its four bytes, which lets C be admitted.
/// assembler.record(b)?;
/// let c = read_scheduled(&file, ready(scheduler.schedule_next()?))?;
///
/// assembler.record(d)?;
/// assembler.record(a)?;
/// assert!(!assembler.is_complete());
///
/// assembler.record(c)?;
/// assert!(assembler.is_complete());
/// assert_eq!(scheduler.in_flight_bytes(), 0);
///
/// let outputs = assembler.finish()?;
/// assert_eq!(outputs.len(), 1);
/// assert_eq!(outputs[0].range(), ReadRange::try_new(0, 14)?);
/// assert_eq!(outputs[0].bytes(), b"abcdefghijklmn");
///
/// std::fs::remove_file(&path)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct OutputAssembler {
    entries: Vec<RangeEntry>,
    outputs: Vec<RangeOutput>,
}

impl OutputAssembler {
    /// Prepares one exact logical destination per planned range of `plan`.
    ///
    /// The plan is borrowed only during construction. All fallible work
    /// happens here, before any backend operation is submitted: the
    /// per-range state and the output capacity used by [`Self::finish`] are
    /// reserved first, then every logical length is converted through a
    /// checked conversion and its final buffer is reserved at that exact
    /// length. Retained state is proportional to the number of logical
    /// ranges; no per-operation entry exists.
    ///
    /// The final logical buffers are deliberately outside the in-flight
    /// [`ByteBudget`](crate::ByteBudget), which limits physical buffers
    /// only. A compact plan over a huge logical range may therefore fail
    /// here even though planning succeeded; no total-process-memory claim
    /// is made.
    ///
    /// # Errors
    ///
    /// Returns [`AssemblyError::StateReservationFailed`] when the per-range
    /// state or reserved output capacity cannot be allocated,
    /// [`AssemblyError::UnrepresentableLength`] when a logical length does
    /// not fit in `usize`, and [`AssemblyError::BufferAllocation`] when a
    /// final logical buffer cannot be reserved. Every failure drops all
    /// previously prepared buffers and returns no assembler.
    pub fn try_new(plan: &ExecutionPlan) -> Result<Self, AssemblyError> {
        let planned_ranges = plan.ranges();
        let capacity = planned_ranges.len();

        let mut entries = try_reserve_state(capacity)?;

        let mut outputs = Vec::new();
        outputs
            .try_reserve_exact(capacity)
            .map_err(|source| AssemblyError::StateReservationFailed { capacity, source })?;

        for planned in planned_ranges {
            let logical = planned.logical_range();
            let length = usize::try_from(logical.length()).map_err(|source| {
                AssemblyError::UnrepresentableLength {
                    range: logical,
                    source,
                }
            })?;

            let mut buffer = Vec::new();
            buffer
                .try_reserve_exact(length)
                .map_err(|source| AssemblyError::BufferAllocation {
                    range: logical,
                    source,
                })?;
            buffer.resize(length, 0);

            entries.push(RangeEntry {
                planned: planned.clone(),
                buffer,
                remaining_bytes: logical.length(),
            });
        }

        Ok(Self { entries, outputs })
    }

    /// Records one completed physical read into its logical destination.
    ///
    /// The completion is consumed. Every fallible condition is validated
    /// before any mutation: the identity locates the retained logical
    /// entry, the entry's planned range regenerates the expected physical
    /// range for that identity, the completion's actual range must equal
    /// it, and the destination span plus the next remaining-byte value are
    /// computed through checked arithmetic and bounds. Only then are the
    /// physical bytes copied to their relative destination
    /// `physical_offset - logical_offset` and the counter committed.
    ///
    /// The completion keeps its physical buffer and budget reservation
    /// alive while validation and copying occur and is destroyed
    /// afterwards, which drops the physical buffer before the reservation
    /// releases. On error the completion is destroyed and released the same
    /// way, and no logical byte or counter has changed.
    ///
    /// # Errors
    ///
    /// Returns [`AssemblyError::PhysicalReadMissing`] when the identity
    /// lies outside the prepared plan,
    /// [`AssemblyError::PhysicalReadFailed`] when regenerating the expected
    /// read fails, [`AssemblyError::RangeMismatch`] when the completion
    /// covers a different physical range than expected,
    /// [`AssemblyError::InvalidDestination`] when the checked destination
    /// arithmetic or bounds report an inconsistency, and
    /// [`AssemblyError::RemainingBytesUnderflow`] when the completion would
    /// subtract more bytes than the logical range has remaining.
    pub fn record(&mut self, completed: CompletedRead) -> Result<(), AssemblyError> {
        let id = completed.id();

        let Some(entry) = self.entries.get_mut(id.logical_range_index()) else {
            return Err(AssemblyError::PhysicalReadMissing { id });
        };

        let expected = entry
            .planned
            .physical_read(id.operation_index())
            .map_err(|source| AssemblyError::PhysicalReadFailed { id, source })?
            .ok_or(AssemblyError::PhysicalReadMissing { id })?;

        let actual = completed.range();
        if expected != actual {
            return Err(AssemblyError::RangeMismatch {
                id,
                expected,
                actual,
            });
        }

        let logical = entry.planned.logical_range();
        let destination = destination_span(logical, expected, entry.buffer.len()).ok_or(
            AssemblyError::InvalidDestination {
                id,
                physical: expected,
                logical,
            },
        )?;

        let next_remaining = entry.remaining_bytes.checked_sub(expected.length()).ok_or(
            AssemblyError::RemainingBytesUnderflow {
                id,
                logical,
                remaining: entry.remaining_bytes,
                length: expected.length(),
            },
        )?;

        let source_bytes = completed.bytes();
        let destination_bytes = entry
            .buffer
            .get_mut(destination)
            .filter(|destination| destination.len() == source_bytes.len());
        let Some(destination_bytes) = destination_bytes else {
            return Err(AssemblyError::InvalidDestination {
                id,
                physical: expected,
                logical,
            });
        };

        destination_bytes.copy_from_slice(source_bytes);
        entry.remaining_bytes = next_remaining;

        // Destroying the completion now drops its physical buffer first and
        // releases its budget reservation only afterwards.
        drop(completed);

        Ok(())
    }

    /// Returns whether every logical byte of every range was integrated.
    ///
    /// This is assembly completeness only: it does not mean the scheduler
    /// is exhausted, no backend work remains, or the execution globally
    /// succeeded.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.entries.iter().all(|entry| entry.remaining_bytes == 0)
    }

    /// Finishes assembly, exposing one [`RangeOutput`] per logical range in
    /// original plan order.
    ///
    /// Finalization is fail-closed: if any logical range is incomplete, no
    /// output is exposed and consuming the assembler drops every private
    /// buffer. On success each final buffer is moved — never recopied —
    /// into its output, reusing the capacity reserved during construction
    /// so no new allocation happens here.
    ///
    /// # Errors
    ///
    /// Returns [`AssemblyError::Incomplete`] with the first incomplete
    /// logical range in plan order and its exact remaining byte count, and
    /// [`AssemblyError::OutputLengthMismatch`] as an output-construction
    /// guard that cannot occur for buffers this assembler prepared.
    pub fn finish(self) -> Result<Vec<RangeOutput>, AssemblyError> {
        let Self {
            entries,
            mut outputs,
        } = self;

        if let Some(entry) = entries.iter().find(|entry| entry.remaining_bytes != 0) {
            return Err(AssemblyError::Incomplete {
                range: entry.planned.logical_range(),
                remaining: entry.remaining_bytes,
            });
        }

        for entry in entries {
            let range = entry.planned.logical_range();
            let output = RangeOutput::try_new(range, entry.buffer).map_err(|mismatch| {
                AssemblyError::OutputLengthMismatch {
                    range,
                    expected: mismatch.expected,
                    actual: mismatch.actual,
                }
            })?;
            outputs.push(output);
        }

        Ok(outputs)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::PathBuf;
    use std::{env, fs, process};

    use super::{AssemblyError, OutputAssembler, destination_span, try_reserve_state};
    use crate::budget::ByteBudget;
    use crate::completion::CompletedRead;
    use crate::execution::{ExecutionConfig, ExecutionPlan, ReadSize};
    use crate::plan::ReadPlan;
    use crate::pread::read_scheduled;
    use crate::range::ReadRange;
    use crate::scheduler::{ScheduleDecision, ScheduledRead, Scheduler};

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

    fn assembler_for(execution: &ExecutionPlan) -> OutputAssembler {
        OutputAssembler::try_new(execution).expect("test assemblers construct without failure")
    }

    fn scheduler_for(execution: ExecutionPlan) -> Scheduler {
        Scheduler::try_new(execution).expect("test schedulers construct without failure")
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

    fn completed(scheduler: &mut Scheduler, bytes: &[u8]) -> CompletedRead {
        let admission = ready(scheduler);
        CompletedRead::try_new(bytes.to_vec(), admission)
            .expect("test bytes cover the admitted range exactly")
    }

    fn with_file_content<T>(test: &str, contents: &[u8], run: impl FnOnce(&File) -> T) -> T {
        let path: PathBuf =
            env::temp_dir().join(format!("range-replay-output-{test}-{}", process::id()));
        fs::write(&path, contents).expect("fixture file is writable");
        let file = File::open(&path).expect("fixture file opens");

        let result = run(&file);

        drop(file);
        fs::remove_file(&path).expect("fixture file is removable");

        result
    }

    #[test]
    fn construction_prepares_every_range_and_starts_incomplete() {
        let plan = execution(&[span(0, 4), span(10, 16)], 4, 8);
        let assembler = assembler_for(&plan);

        assert!(!assembler.is_complete());

        let error = assembler
            .finish()
            .expect_err("nothing was recorded, so finalization must fail closed");
        assert!(matches!(
            error,
            AssemblyError::Incomplete {
                range,
                remaining: 4,
            } if range == span(0, 4)
        ));
    }

    #[test]
    fn the_bdac_fixture_reconstructs_exact_bytes_despite_completion_order() {
        with_file_content("bdac", b"abcdefghijklmn", |file| {
            let plan = execution(&[span(0, 14)], 4, 10);
            let mut assembler = assembler_for(&plan);
            let mut scheduler = scheduler_for(plan);

            let a = read_scheduled(file, ready(&mut scheduler)).expect("A is inside the fixture");
            let b = read_scheduled(file, ready(&mut scheduler)).expect("B is inside the fixture");
            let d = read_scheduled(file, ready(&mut scheduler)).expect("D is inside the fixture");
            assert_eq!(a.id().operation_index(), 0);
            assert_eq!(b.id().operation_index(), 1);
            assert_eq!(d.id().operation_index(), 3);
            assert_eq!(scheduler.in_flight_bytes(), 10);
            assert!(matches!(
                scheduler
                    .schedule_next()
                    .expect("test scheduling decisions succeed"),
                ScheduleDecision::WaitingForBudget
            ));

            assembler.record(b).expect("B matches its expected range");
            assert_eq!(scheduler.in_flight_bytes(), 6);
            assert!(!assembler.is_complete());

            let c = read_scheduled(file, ready(&mut scheduler)).expect("C is inside the fixture");
            assert_eq!(c.id().operation_index(), 2);
            assert_eq!(scheduler.in_flight_bytes(), 10);

            assembler.record(d).expect("D matches its expected range");
            assert_eq!(scheduler.in_flight_bytes(), 8);

            assembler.record(a).expect("A matches its expected range");
            assert_eq!(scheduler.in_flight_bytes(), 4);
            assert!(!assembler.is_complete());

            assembler.record(c).expect("C matches its expected range");
            assert_eq!(scheduler.in_flight_bytes(), 0);
            assert!(assembler.is_complete());
            assert!(matches!(
                scheduler
                    .schedule_next()
                    .expect("test scheduling decisions succeed"),
                ScheduleDecision::Exhausted
            ));

            let outputs = assembler.finish().expect("every logical byte was recorded");
            assert_eq!(outputs.len(), 1);
            assert_eq!(outputs[0].range(), span(0, 14));
            assert_eq!(outputs[0].bytes(), b"abcdefghijklmn");
        });
    }

    #[test]
    fn scrambled_completions_land_on_their_relative_destinations_in_plan_order() {
        let plan = execution(&[span(10, 16), span(20, 26)], 4, 12);
        let mut assembler = assembler_for(&plan);
        let mut scheduler = scheduler_for(plan);

        // Greedy admission order: both full reads in plan order, then both
        // equal tails in plan order.
        let first_full = completed(&mut scheduler, b"ABCD");
        let second_full = completed(&mut scheduler, b"wxyz");
        let first_tail = completed(&mut scheduler, b"EF");
        let second_tail = completed(&mut scheduler, b"!?");
        assert_eq!(first_full.range(), span(10, 14));
        assert_eq!(second_full.range(), span(20, 24));
        assert_eq!(first_tail.range(), span(14, 16));
        assert_eq!(second_tail.range(), span(24, 26));

        for completion in [second_tail, first_tail, second_full, first_full] {
            assembler
                .record(completion)
                .expect("every completion matches its expected range");
        }
        assert!(assembler.is_complete());

        // Finalization moves each prepared buffer into its output — plan
        // order, no payload recopy, and no allocation beyond the capacity
        // reserved at construction.
        let outputs = assembler.finish().expect("every logical byte was recorded");
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].range(), span(10, 16));
        assert_eq!(outputs[0].bytes(), b"ABCDEF");
        assert_eq!(outputs[1].range(), span(20, 26));
        assert_eq!(outputs[1].bytes(), b"wxyz!?");
    }

    #[test]
    fn a_successful_record_consumes_the_completion_and_releases_exactly_its_bytes() {
        let plan = execution(&[span(0, 8)], 4, 8);
        let mut assembler = assembler_for(&plan);
        let mut scheduler = scheduler_for(plan);

        let first = completed(&mut scheduler, b"abcd");
        let second = completed(&mut scheduler, b"efgh");
        assert_eq!(scheduler.in_flight_bytes(), 8);

        assembler
            .record(first)
            .expect("the first completion matches");
        assert_eq!(scheduler.in_flight_bytes(), 4);
        assert_eq!(scheduler.available_bytes(), 4);

        assembler
            .record(second)
            .expect("the second completion matches");
        assert_eq!(scheduler.in_flight_bytes(), 0);
        assert!(assembler.is_complete());
    }

    #[test]
    fn an_unknown_logical_range_index_is_rejected_without_mutation() {
        let plan = execution(&[span(0, 4)], 4, 4);
        let mut assembler = assembler_for(&plan);
        let mut scheduler = scheduler_for(plan);

        let foreign_plan = execution(&[span(0, 4), span(10, 14)], 4, 8);
        let mut foreign = scheduler_for(foreign_plan);
        drop(ready(&mut foreign));
        let stray = completed(&mut foreign, b"wxyz");
        assert_eq!(stray.id().logical_range_index(), 1);
        assert_eq!(foreign.in_flight_bytes(), 4);

        let error = assembler
            .record(stray)
            .expect_err("the prepared plan has exactly one logical range");
        assert!(matches!(
            error,
            AssemblyError::PhysicalReadMissing { id }
                if id.logical_range_index() == 1 && id.operation_index() == 0
        ));
        assert_eq!(foreign.in_flight_bytes(), 0);

        let own = completed(&mut scheduler, b"abcd");
        assembler
            .record(own)
            .expect("the paired completion matches");
        let outputs = assembler
            .finish()
            .expect("the failed attempt was non-mutating");
        assert_eq!(outputs[0].bytes(), b"abcd");
    }

    #[test]
    fn an_unknown_operation_index_is_rejected_without_mutation() {
        let plan = execution(&[span(0, 14)], 4, 14);
        let mut assembler = assembler_for(&plan);
        let mut scheduler = scheduler_for(plan);

        let foreign_plan = execution(&[span(0, 20)], 4, 20);
        let mut foreign = scheduler_for(foreign_plan);
        for _admitted in 0..4 {
            drop(ready(&mut foreign));
        }
        let stray = completed(&mut foreign, b"qrst");
        assert_eq!(stray.id().operation_index(), 4);

        let error = assembler
            .record(stray)
            .expect_err("the prepared range has only four physical operations");
        assert!(matches!(
            error,
            AssemblyError::PhysicalReadMissing { id }
                if id.logical_range_index() == 0 && id.operation_index() == 4
        ));
        assert_eq!(foreign.in_flight_bytes(), 0);

        for bytes in [b"abcd".as_slice(), b"efgh", b"ijkl", b"mn"] {
            let own = completed(&mut scheduler, bytes);
            assembler
                .record(own)
                .expect("the paired completion matches");
        }
        let outputs = assembler
            .finish()
            .expect("the failed attempt was non-mutating");
        assert_eq!(outputs[0].bytes(), b"abcdefghijklmn");
    }

    #[test]
    fn a_different_plan_range_mismatch_is_typed_non_mutating_and_releases() {
        let plan = execution(&[span(0, 4)], 4, 4);
        let mut assembler = assembler_for(&plan);
        let mut scheduler = scheduler_for(plan);

        let foreign_plan = execution(&[span(20, 24)], 4, 4);
        let mut foreign = scheduler_for(foreign_plan);
        let stray = completed(&mut foreign, b"wxyz");
        assert_eq!(foreign.in_flight_bytes(), 4);

        let error = assembler
            .record(stray)
            .expect_err("a completion for [20, 24) cannot serve [0, 4)");
        match error {
            AssemblyError::RangeMismatch {
                id,
                expected,
                actual,
            } => {
                assert_eq!(id.logical_range_index(), 0);
                assert_eq!(id.operation_index(), 0);
                assert_eq!(expected, span(0, 4));
                assert_eq!(actual, span(20, 24));
            }
            other => panic!("expected a range mismatch, got {other:?}"),
        }
        assert_eq!(foreign.in_flight_bytes(), 0);
        assert_eq!(foreign.available_bytes(), 4);
        assert!(!assembler.is_complete());

        let own = completed(&mut scheduler, b"abcd");
        assembler
            .record(own)
            .expect("the paired completion matches");
        let outputs = assembler
            .finish()
            .expect("the failed attempt was non-mutating");
        assert_eq!(outputs[0].range(), span(0, 4));
        assert_eq!(outputs[0].bytes(), b"abcd");
    }

    #[test]
    fn destination_span_rejects_inconsistent_inputs_without_panic() {
        assert_eq!(destination_span(span(10, 20), span(12, 16), 10), Some(2..6));
        assert_eq!(
            destination_span(span(10, 20), span(10, 20), 10),
            Some(0..10)
        );

        // A physical range starting before the logical offset underflows the
        // relative start.
        assert_eq!(destination_span(span(10, 20), span(8, 12), 10), None);
        // A physical range ending past the logical end leaves the buffer.
        assert_eq!(destination_span(span(10, 20), span(16, 22), 10), None);
        // An inconsistently short buffer cannot hold the destination.
        assert_eq!(destination_span(span(10, 20), span(14, 20), 6), None);
    }

    #[test]
    fn a_duplicate_from_an_identical_plan_underflows_typed_and_non_mutating() {
        let plan = execution(&[span(0, 6)], 4, 6);
        let mut assembler = assembler_for(&plan);
        let mut scheduler = scheduler_for(plan);

        // An independent run over a byte-for-byte identical plan produces
        // completions this low-level assembler cannot distinguish; the
        // remaining-byte counter is the guard that still refuses to wrap.
        let twin_plan = execution(&[span(0, 6)], 4, 6);
        let mut twin = scheduler_for(twin_plan);
        let duplicate = completed(&mut twin, b"QRST");

        let full = completed(&mut scheduler, b"abcd");
        assembler
            .record(full)
            .expect("the paired completion matches");

        let error = assembler
            .record(duplicate)
            .expect_err("four more bytes cannot fit into the two remaining");
        match error {
            AssemblyError::RemainingBytesUnderflow {
                id,
                logical,
                remaining,
                length,
            } => {
                assert_eq!(id.operation_index(), 0);
                assert_eq!(logical, span(0, 6));
                assert_eq!(remaining, 2);
                assert_eq!(length, 4);
            }
            other => panic!("expected a remaining-byte underflow, got {other:?}"),
        }
        assert_eq!(twin.in_flight_bytes(), 0);

        let tail = completed(&mut scheduler, b"ef");
        assembler
            .record(tail)
            .expect("the paired completion matches");
        assert!(assembler.is_complete());

        // The rejected duplicate changed no byte: the output holds the
        // originally recorded payload, not "QRST".
        let outputs = assembler.finish().expect("every logical byte was recorded");
        assert_eq!(outputs[0].bytes(), b"abcdef");
    }

    #[test]
    fn is_complete_stays_false_until_every_logical_range_is_complete() {
        let plan = execution(&[span(0, 4), span(10, 14)], 4, 8);
        let mut assembler = assembler_for(&plan);
        let mut scheduler = scheduler_for(plan);

        let first = completed(&mut scheduler, b"abcd");
        let second = completed(&mut scheduler, b"wxyz");

        assert!(!assembler.is_complete());
        assembler.record(first).expect("the first range completes");
        assert!(!assembler.is_complete());
        assembler
            .record(second)
            .expect("the second range completes");
        assert!(assembler.is_complete());
    }

    #[test]
    fn finishing_with_an_incomplete_range_reports_its_exact_remaining_bytes() {
        let plan = execution(&[span(0, 14)], 4, 14);
        let mut assembler = assembler_for(&plan);
        let mut scheduler = scheduler_for(plan);

        let first = completed(&mut scheduler, b"abcd");
        assembler
            .record(first)
            .expect("the paired completion matches");

        let error = assembler
            .finish()
            .expect_err("ten bytes were never recorded");
        assert!(matches!(
            error,
            AssemblyError::Incomplete {
                range,
                remaining: 10,
            } if range == span(0, 14)
        ));
    }

    #[test]
    fn the_assembler_stays_valid_after_the_source_plan_is_dropped() {
        let source = execution(&[span(0, 4)], 4, 4);
        let twin = source.clone();

        let mut assembler = assembler_for(&source);
        drop(source);

        let mut scheduler = scheduler_for(twin);
        let only = completed(&mut scheduler, b"abcd");
        assembler
            .record(only)
            .expect("the equal-plan completion matches");

        let outputs = assembler.finish().expect("every logical byte was recorded");
        assert_eq!(outputs[0].bytes(), b"abcd");
    }

    #[test]
    fn per_range_state_stays_proportional_for_a_large_operation_count() {
        const LENGTH: u64 = 1 << 24;

        // 16,777,216 physical operations at read size 1. Preparation must
        // allocate only the one logical buffer plus constant metadata —
        // never any per-operation entry — so this constructs instantly.
        let plan = execution(&[span(0, LENGTH)], 1, 1);
        assert_eq!(plan.ranges()[0].operation_count(), LENGTH);

        let assembler = assembler_for(&plan);
        assert!(!assembler.is_complete());

        let error = assembler
            .finish()
            .expect_err("nothing was recorded, so finalization must fail closed");
        assert!(matches!(
            error,
            AssemblyError::Incomplete {
                remaining: LENGTH,
                ..
            }
        ));
    }

    #[test]
    fn an_unallocatable_logical_buffer_is_a_typed_error_before_any_io() {
        // On 64-bit targets `u64::MAX` converts to `usize`, so the fallible
        // buffer reservation is what must reject the request.
        let plan = execution(&[span(0, u64::MAX)], u64::MAX, u64::MAX);

        let error = OutputAssembler::try_new(&plan)
            .expect_err("no buffer of u64::MAX bytes can be reserved");
        assert!(matches!(
            error,
            AssemblyError::BufferAllocation { range, .. } if range == span(0, u64::MAX)
        ));
    }

    #[test]
    fn an_unreservable_state_capacity_is_a_typed_error() {
        match try_reserve_state(usize::MAX) {
            Err(AssemblyError::StateReservationFailed { capacity, .. }) => {
                assert_eq!(capacity, usize::MAX);
            }
            other => panic!("expected a typed reservation failure, got {other:?}"),
        }
    }
}
