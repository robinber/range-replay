//! Synchronous positioned-read reference backend.
//!
//! This backend executes positioned exact reads against an already open
//! [`File`] through the safe Unix positioned-read API ([`FileExt::read_at`],
//! `pread` semantics), covering both an already validated [`ReadPlan`] and
//! one already admitted [`ScheduledRead`]. Planning stays pure; this module
//! owns the first real I/O boundary and is the correctness reference that
//! later backends must match byte for byte.
//!
//! Two entry points share one exact-read loop: [`read_plan`] executes a whole
//! logical plan into [`RangeOutput`] values, and [`read_scheduled`] executes
//! exactly one admitted [`ScheduledRead`] into a backend-neutral
//! [`CompletedRead`] whose reservation stays live until the completion is
//! destroyed. Neither entry point moves the file cursor.

use std::collections::TryReserveError;
use std::fs::File;
use std::io;
use std::num::TryFromIntError;
use std::os::unix::fs::FileExt;

use thiserror::Error;

use crate::completion::CompletedRead;
use crate::output::RangeOutput;
use crate::plan::ReadPlan;
use crate::range::ReadRange;
use crate::scheduler::ScheduledRead;

/// Reason a positioned exact read against a file failed.
///
/// The same error serves both entry points of this backend: [`read_plan`],
/// which executes a whole validated plan, and [`read_scheduled`], which
/// executes exactly one admitted physical read. Every variant carries the
/// range whose read failed, so the error points back into the plan or the
/// admitted operation.
///
/// Both entry points are fail-closed. A failing [`read_plan`] call aborts
/// at its first failing range and never exposes output for previously
/// completed ranges; a failing [`read_scheduled`] call exposes no
/// completion and releases its admitted budget bytes. The
/// [`Self::OffsetOverflow`], [`Self::OverreportedRead`],
/// [`Self::CompletionLengthMismatch`], and [`Self::OutputLengthMismatch`]
/// variants are loop and construction guards for states unreachable through
/// validated inputs rather than ordinary I/O outcomes.
#[derive(Debug, Error)]
pub enum ReadError {
    /// The range length does not fit in `usize`, so no buffer of that size is
    /// representable on this platform.
    #[error(
        "range [{}, {}): length {} is not representable as a buffer size",
        .range.offset(),
        .range.end(),
        .range.length()
    )]
    UnrepresentableLength {
        /// The range whose length no buffer can represent.
        range: ReadRange,
        /// The underlying integer conversion failure.
        source: TryFromIntError,
    },
    /// The range buffer could not be reserved.
    #[error(
        "range [{}, {}): cannot reserve a {}-byte buffer",
        .range.offset(),
        .range.end(),
        .range.length()
    )]
    BufferAllocation {
        /// The range whose buffer reservation failed.
        range: ReadRange,
        /// The underlying reservation failure reported by Rust.
        source: TryReserveError,
    },
    /// The file ended before the range was filled.
    #[error(
        "range [{}, {}): unexpected end of file after {actual} of {expected} bytes",
        .range.offset(),
        .range.end()
    )]
    UnexpectedEof {
        /// The range the file ended inside.
        range: ReadRange,
        /// The byte count the range requires.
        expected: u64,
        /// The byte count actually read before the end of the file.
        actual: u64,
    },
    /// The exact-read loop arithmetic left the representable range.
    ///
    /// Reads that follow the [`FileExt::read_at`] contract can never advance
    /// past a validated range end, so this variant guards the loop arithmetic
    /// instead of describing a reachable input. Reporting it keeps the loop
    /// free of any panic or silent-wrap path.
    #[error("range [{}, {}): offset arithmetic overflowed", .range.offset(), .range.end())]
    OffsetOverflow {
        /// The range being read when the arithmetic overflowed.
        range: ReadRange,
    },
    /// A positioned read reported more bytes than the unfilled remainder.
    ///
    /// [`FileExt::read_at`] can never report more bytes than the buffer it
    /// was given, so this variant guards the exact-read loop against a broken
    /// positioned-read implementation instead of describing a reachable
    /// production input. Rejecting the over-report keeps a contract violation
    /// from turning into silent success over unread bytes.
    #[error(
        "range [{}, {}): read at offset {offset} reported {reported} bytes for a \
         {remaining}-byte remainder",
        .range.offset(),
        .range.end()
    )]
    OverreportedRead {
        /// The range being read when the over-report happened.
        range: ReadRange,
        /// The absolute file offset of the over-reporting read.
        offset: u64,
        /// The byte count the read claimed to have filled.
        reported: usize,
        /// The unfilled byte count that was actually available.
        remaining: usize,
    },
    /// A completed physical buffer does not cover its admitted range
    /// exactly.
    ///
    /// The exact-read loop only returns a buffer whose length equals the
    /// requested range length, so this variant guards completion
    /// construction instead of describing a reachable production input.
    /// Rejecting the mismatch keeps a broken construction path from
    /// exposing a completion whose bytes and budget accounting disagree.
    #[error(
        "range [{}, {}): a completed buffer holds {actual} bytes for a \
         {expected}-byte physical read",
        .range.offset(),
        .range.end()
    )]
    CompletionLengthMismatch {
        /// The admitted physical range the buffer was meant to cover.
        range: ReadRange,
        /// The byte count the range requires.
        expected: u64,
        /// The byte count the rejected buffer actually holds.
        actual: usize,
    },
    /// A fully read logical buffer does not cover its range exactly.
    ///
    /// The exact-read loop only returns a buffer whose length equals the
    /// requested range length, so this variant guards logical output
    /// construction instead of describing a reachable production input.
    /// Rejecting the mismatch keeps a broken construction path from
    /// exposing an output whose bytes and range disagree.
    #[error(
        "range [{}, {}): a logical buffer holds {actual} bytes for a \
         {expected}-byte range",
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
    /// A read failed with an error that is not an interruption.
    #[error("range [{}, {}): read failed at offset {offset}", .range.offset(), .range.end())]
    Io {
        /// The range being read when the failure happened.
        range: ReadRange,
        /// The absolute file offset of the failing read.
        offset: u64,
        /// The underlying I/O failure.
        source: io::Error,
    },
}

/// Executes every range of a validated plan against an open file.
///
/// The file and the plan are only borrowed: the call consumes neither, and it
/// never moves the file cursor because every read goes through the positioned
/// [`FileExt::read_at`] API (`pread` semantics). The plan is trusted as
/// validated input; nothing here parses, sorts, coalesces, or revalidates it.
///
/// On success the result holds exactly one owned [`RangeOutput`] per
/// canonical range, in [`ReadPlan::ranges`] order, and every output buffer is
/// filled exactly. Short reads are completed by follow-up reads of the
/// unfilled remainder, and [`io::ErrorKind::Interrupted`] is retried without
/// losing progress.
///
/// The call is fail-closed: if any range fails, the whole result is an error
/// and no partial output is observable.
///
/// # Errors
///
/// Returns [`ReadError::UnrepresentableLength`] when a range length does not
/// fit in `usize`, [`ReadError::BufferAllocation`] when a range buffer cannot
/// be reserved, [`ReadError::UnexpectedEof`] when the file ends before a
/// range is filled, [`ReadError::OffsetOverflow`] if the exact-read loop
/// arithmetic would overflow, [`ReadError::OverreportedRead`] if a read
/// reports more bytes than the unfilled remainder,
/// [`ReadError::OutputLengthMismatch`] as a logical-output construction
/// guard, and [`ReadError::Io`] for every other I/O failure, preserving the
/// original [`io::Error`] as its source.
///
/// # Examples
///
/// ```
/// use std::fs::File;
/// use std::io::Write;
///
/// use range_replay::{ReadPlan, ReadRange, read_plan};
///
/// let path = std::env::temp_dir()
///     .join(format!("range-replay-doc-read-plan-{}", std::process::id()));
/// File::create_new(&path)?.write_all(b"0123456789abcdef")?;
///
/// let file = File::open(&path)?;
/// let plan = ReadPlan::try_from_schedule(&[
///     ReadRange::try_new(10, 4)?,
///     ReadRange::try_new(2, 3)?,
/// ])?;
///
/// let outputs = read_plan(&file, &plan)?;
///
/// assert_eq!(outputs[0].range().offset(), 2);
/// assert_eq!(outputs[0].bytes(), b"234");
/// assert_eq!(outputs[1].range().offset(), 10);
/// assert_eq!(outputs[1].bytes(), b"abcd");
///
/// std::fs::remove_file(&path)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn read_plan(file: &File, plan: &ReadPlan) -> Result<Vec<RangeOutput>, ReadError> {
    plan.ranges()
        .iter()
        .map(|&range| {
            let bytes = read_range_exact(|buffer, offset| file.read_at(buffer, offset), range)?;

            RangeOutput::try_new(range, bytes).map_err(|mismatch| ReadError::OutputLengthMismatch {
                range,
                expected: mismatch.expected,
                actual: mismatch.actual,
            })
        })
        .collect()
}

/// Executes one admitted physical read against an open file.
///
/// The file is only borrowed and its cursor never moves, because the read
/// goes through the positioned [`FileExt::read_at`] API (`pread`
/// semantics). The scheduled handle is consumed: its
/// [`ScheduledRead::range`] is the only source of the physical range, and
/// the per-read buffer is allocated only after the admission already
/// exists, so the buffer never occupies bytes the budget has not accounted.
///
/// On success the returned [`CompletedRead`] preserves the exact
/// [`OperationId`](crate::OperationId), range, and bytes of the admitted
/// operation and keeps its reservation live: the bytes stay counted in
/// flight until the completion is destroyed. Short reads are completed by
/// follow-up reads of the unfilled remainder, and
/// [`io::ErrorKind::Interrupted`] is retried without losing progress.
///
/// On any error no completion exists and no partial bytes are observable:
/// the consumed handle and the partial buffer are destroyed, which releases
/// exactly the admitted bytes back to the scheduler's budget.
///
/// The function performs exactly one physical read. It never asks the
/// scheduler for more work, waits for budget, assembles logical output,
/// computes a checksum, or chooses a backend.
///
/// # Errors
///
/// Returns [`ReadError::UnrepresentableLength`] when the range length does
/// not fit in `usize`, [`ReadError::BufferAllocation`] when the range
/// buffer cannot be reserved, [`ReadError::UnexpectedEof`] with exact
/// expected and actual counts when the file ends before the range is
/// filled, [`ReadError::OffsetOverflow`] and [`ReadError::OverreportedRead`]
/// as exact-read loop guards, [`ReadError::CompletionLengthMismatch`] as a
/// completion-construction guard, and [`ReadError::Io`] for every other I/O
/// failure, preserving the original [`io::Error`] as its source.
///
/// # Examples
///
/// ```
/// use std::fs::File;
/// use std::io::Write;
///
/// use range_replay::{
///     ByteBudget, ExecutionConfig, ExecutionPlan, ReadPlan, ReadRange, ReadSize,
///     ScheduleDecision, Scheduler, read_scheduled,
/// };
///
/// let path = std::env::temp_dir()
///     .join(format!("range-replay-doc-read-scheduled-{}", std::process::id()));
/// File::create_new(&path)?.write_all(b"0123456789abcdef")?;
/// let file = File::open(&path)?;
///
/// let plan = ReadPlan::try_from_schedule(&[ReadRange::try_new(2, 3)?])?;
/// let config = ExecutionConfig::try_new(ReadSize::try_new(3)?, ByteBudget::try_new(3)?)?;
/// let mut scheduler = Scheduler::try_new(ExecutionPlan::try_from_read_plan(&plan, config)?)?;
///
/// let ScheduleDecision::Ready(scheduled) = scheduler.schedule_next()? else {
///     unreachable!("one three-byte read fits the whole budget");
/// };
/// assert_eq!(scheduler.in_flight_bytes(), 3);
///
/// let completed = read_scheduled(&file, scheduled)?;
/// assert_eq!(completed.id().logical_range_index(), 0);
/// assert_eq!(completed.id().operation_index(), 0);
/// assert_eq!(completed.range(), ReadRange::try_new(2, 3)?);
/// assert_eq!(completed.bytes(), b"234");
///
/// // The admitted bytes stay in flight while the completion is alive...
/// assert_eq!(scheduler.in_flight_bytes(), 3);
///
/// // ...and are released only when the completion is destroyed.
/// drop(completed);
/// assert_eq!(scheduler.in_flight_bytes(), 0);
/// assert_eq!(scheduler.available_bytes(), 3);
///
/// std::fs::remove_file(&path)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn read_scheduled(file: &File, scheduled: ScheduledRead) -> Result<CompletedRead, ReadError> {
    read_scheduled_with(|buffer, offset| file.read_at(buffer, offset), scheduled)
}

/// Executes one admitted physical read through a positioned-read callback.
///
/// This is the deterministic seam under [`read_scheduled`]: production
/// passes a closure over a borrowed [`File`], tests pass scripted closures.
/// The callback contract is the one documented on [`read_range_exact`]. The
/// scheduled handle is consumed either into the returned completion or, on
/// any error, into an automatic release of its admitted bytes.
fn read_scheduled_with<F>(read_at: F, scheduled: ScheduledRead) -> Result<CompletedRead, ReadError>
where
    F: FnMut(&mut [u8], u64) -> io::Result<usize>,
{
    let range = scheduled.range();
    let bytes = read_range_exact(read_at, range)?;

    CompletedRead::try_new(bytes, scheduled).map_err(|mismatch| {
        ReadError::CompletionLengthMismatch {
            range,
            expected: mismatch.expected,
            actual: mismatch.actual,
        }
    })
}

/// Fills a whole range through a positioned-read callback.
///
/// `read_at` must follow [`FileExt::read_at`] semantics: it reads into the
/// given buffer at the given absolute file offset, returns the number of
/// bytes read with `0` meaning end of file, never reports more bytes than the
/// buffer holds, and moves no cursor. A callback that reports more bytes than
/// the unfilled remainder is rejected with
/// [`ReadError::OverreportedRead`] rather than trusted, so a broken reader can
/// never turn into silent success over unread bytes. Production passes a
/// closure over a borrowed [`File`]; tests pass scripted closures to exercise
/// short reads, interruption, EOF, and failures deterministically.
///
/// Every call receives only the unfilled remainder of the buffer and the
/// matching absolute offset, so a short read is completed without touching
/// the bytes already read, and an [`io::ErrorKind::Interrupted`] result
/// retries the exact same slice and offset.
fn read_range_exact<F>(mut read_at: F, range: ReadRange) -> Result<Vec<u8>, ReadError>
where
    F: FnMut(&mut [u8], u64) -> io::Result<usize>,
{
    let length = usize::try_from(range.length())
        .map_err(|source| ReadError::UnrepresentableLength { range, source })?;

    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|source| ReadError::BufferAllocation { range, source })?;
    bytes.resize(length, 0);

    let mut filled = 0;
    let mut offset = range.offset();

    while filled < length {
        let unfilled = &mut bytes[filled..];
        let remaining = unfilled.len();

        match read_at(unfilled, offset) {
            Ok(0) => {
                let actual =
                    u64::try_from(filled).map_err(|_source| ReadError::OffsetOverflow { range })?;

                return Err(ReadError::UnexpectedEof {
                    range,
                    expected: range.length(),
                    actual,
                });
            }
            Ok(count) => {
                if count > remaining {
                    return Err(ReadError::OverreportedRead {
                        range,
                        offset,
                        reported: count,
                        remaining,
                    });
                }

                offset = u64::try_from(count)
                    .ok()
                    .and_then(|advance| offset.checked_add(advance))
                    .ok_or(ReadError::OffsetOverflow { range })?;
                filled = filled
                    .checked_add(count)
                    .ok_or(ReadError::OffsetOverflow { range })?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(source) => {
                return Err(ReadError::Io {
                    range,
                    offset,
                    source,
                });
            }
        }
    }

    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io;
    use std::io::{ErrorKind, Seek, SeekFrom};

    use super::{ReadError, read_plan, read_range_exact, read_scheduled, read_scheduled_with};
    use crate::test_support::{HEX_FIXTURE, admitted_single, plan, range, with_file_content};

    fn with_fixture_file<T>(test: &str, run: impl FnOnce(&mut File) -> T) -> T {
        with_file_content(test, HEX_FIXTURE, run)
    }

    #[test]
    fn read_plan_returns_the_fixture_bytes_in_plan_order() {
        with_fixture_file("plan-order", |file| {
            let plan = plan(&[range(10, 4), range(2, 3)]);

            let outputs = read_plan(file, &plan).expect("both ranges are inside the fixture");

            assert_eq!(outputs.len(), 2);
            assert_eq!(outputs[0].range(), range(2, 3));
            assert_eq!(outputs[0].bytes(), b"234");
            assert_eq!(outputs[1].range(), range(10, 4));
            assert_eq!(outputs[1].bytes(), b"abcd");
        });
    }

    #[test]
    fn read_plan_leaves_the_file_cursor_unchanged() {
        with_fixture_file("cursor", |file| {
            file.seek(SeekFrom::Start(7)).expect("fixture file seeks");
            let plan = plan(&[range(2, 3), range(10, 4)]);

            read_plan(file, &plan).expect("both ranges are inside the fixture");

            let cursor = file
                .stream_position()
                .expect("fixture file reports its cursor");
            assert_eq!(cursor, 7);
        });
    }

    #[test]
    fn read_plan_fails_closed_when_a_later_range_passes_eof() {
        with_fixture_file("fail-closed", |file| {
            let plan = plan(&[range(2, 3), range(100, 4)]);

            let error = read_plan(file, &plan).expect_err("the second range starts past EOF");

            assert!(matches!(
                error,
                ReadError::UnexpectedEof {
                    range: failed,
                    expected: 4,
                    actual: 0,
                } if failed == range(100, 4)
            ));
        });
    }

    #[test]
    fn read_range_exact_completes_short_reads_without_overwriting_prior_bytes() {
        let mut calls = Vec::new();
        let mut step = 0;

        let bytes = read_range_exact(
            |buffer, offset| {
                calls.push((buffer.len(), offset));
                step += 1;
                match step {
                    1 => {
                        buffer[..2].copy_from_slice(b"ab");
                        Ok(2)
                    }
                    2 => {
                        buffer[..1].copy_from_slice(b"c");
                        Ok(1)
                    }
                    _ => panic!("no further read expected"),
                }
            },
            range(10, 3),
        )
        .expect("the scripted reads fill the range");

        assert_eq!(bytes, b"abc");
        assert_eq!(calls, vec![(3, 10), (1, 12)]);
    }

    #[test]
    fn read_range_exact_retries_the_same_slice_and_offset_after_interruption() {
        let mut calls = Vec::new();
        let mut step = 0;

        let bytes = read_range_exact(
            |buffer, offset| {
                calls.push((buffer.len(), offset));
                step += 1;
                match step {
                    1 => Err(io::Error::new(ErrorKind::Interrupted, "signal")),
                    2 => {
                        buffer.copy_from_slice(b"abc");
                        Ok(3)
                    }
                    _ => panic!("no further read expected"),
                }
            },
            range(10, 3),
        )
        .expect("the retried read fills the range");

        assert_eq!(bytes, b"abc");
        assert_eq!(calls, vec![(3, 10), (3, 10)]);
    }

    #[test]
    fn read_range_exact_reports_partial_eof_with_exact_counts() {
        let mut step = 0;

        let error = read_range_exact(
            |buffer, _offset| {
                step += 1;
                match step {
                    1 => {
                        buffer[..2].copy_from_slice(b"ab");
                        Ok(2)
                    }
                    _ => Ok(0),
                }
            },
            range(10, 4),
        )
        .expect_err("the file ends inside the range");

        assert!(matches!(
            error,
            ReadError::UnexpectedEof {
                range: failed,
                expected: 4,
                actual: 2,
            } if failed == range(10, 4)
        ));
    }

    #[test]
    fn read_range_exact_rejects_an_overreported_count() {
        let error = read_range_exact(|buffer, _offset| Ok(buffer.len() + 1), range(10, 4))
            .expect_err("an over-reported read must not become silent success");

        assert!(matches!(
            error,
            ReadError::OverreportedRead {
                offset: 10,
                reported: 5,
                remaining: 4,
                ..
            }
        ));
    }

    #[test]
    fn read_range_exact_reports_eof_before_the_first_byte() {
        let error = read_range_exact(|_buffer, _offset| Ok(0), range(10, 4))
            .expect_err("the file ends before the range");

        assert!(matches!(
            error,
            ReadError::UnexpectedEof {
                expected: 4,
                actual: 0,
                ..
            }
        ));
    }

    #[test]
    fn read_range_exact_preserves_the_io_error_and_its_context() {
        let mut step = 0;

        let error = read_range_exact(
            |buffer, _offset| {
                step += 1;
                match step {
                    1 => {
                        buffer[..2].copy_from_slice(b"ab");
                        Ok(2)
                    }
                    _ => Err(io::Error::new(ErrorKind::PermissionDenied, "denied")),
                }
            },
            range(10, 4),
        )
        .expect_err("the second read fails");

        assert!(std::error::Error::source(&error).is_some());

        let ReadError::Io {
            range: failed,
            offset,
            source,
        } = error
        else {
            panic!("expected an I/O error, got {error:?}");
        };
        assert_eq!(failed, range(10, 4));
        assert_eq!(offset, 12);
        assert_eq!(source.kind(), ErrorKind::PermissionDenied);
    }

    #[test]
    fn read_scheduled_returns_the_exact_completion_and_releases_only_on_drop() {
        with_fixture_file("scheduled-success", |file| {
            let (scheduler, admission) = admitted_single(2, 3, 3);
            let id = admission.id();
            assert_eq!(scheduler.in_flight_bytes(), 3);
            assert_eq!(scheduler.available_bytes(), 0);

            let completed =
                read_scheduled(file, admission).expect("the range is inside the fixture");

            assert_eq!(completed.id(), id);
            assert_eq!(completed.id().logical_range_index(), 0);
            assert_eq!(completed.id().operation_index(), 0);
            assert_eq!(completed.range(), range(2, 3));
            assert_eq!(completed.bytes(), b"234");
            assert_eq!(scheduler.in_flight_bytes(), 3);
            assert_eq!(scheduler.available_bytes(), 0);

            drop(completed);
            assert_eq!(scheduler.in_flight_bytes(), 0);
            assert_eq!(scheduler.available_bytes(), 3);
        });
    }

    #[test]
    fn read_scheduled_leaves_the_file_cursor_unchanged() {
        with_fixture_file("scheduled-cursor", |file| {
            file.seek(SeekFrom::Start(7)).expect("fixture file seeks");
            let (_scheduler, admission) = admitted_single(2, 3, 3);

            let completed =
                read_scheduled(file, admission).expect("the range is inside the fixture");
            drop(completed);

            let cursor = file
                .stream_position()
                .expect("fixture file reports its cursor");
            assert_eq!(cursor, 7);
        });
    }

    #[test]
    fn read_scheduled_reports_partial_eof_and_restores_the_budget() {
        with_file_content("scheduled-partial-eof", b"abc", |file| {
            file.seek(SeekFrom::Start(1)).expect("fixture file seeks");
            let (scheduler, admission) = admitted_single(1, 4, 4);
            assert_eq!(scheduler.in_flight_bytes(), 4);

            let error =
                read_scheduled(file, admission).expect_err("the file ends inside the range");

            assert!(matches!(
                error,
                ReadError::UnexpectedEof {
                    range: failed,
                    expected: 4,
                    actual: 2,
                } if failed == range(1, 4)
            ));
            assert_eq!(scheduler.in_flight_bytes(), 0);
            assert_eq!(scheduler.available_bytes(), 4);

            let cursor = file
                .stream_position()
                .expect("fixture file reports its cursor");
            assert_eq!(cursor, 1);
        });
    }

    #[test]
    fn read_scheduled_with_completes_short_reads_into_one_exact_completion() {
        let (scheduler, admission) = admitted_single(10, 3, 3);
        let mut calls = Vec::new();
        let mut step = 0;

        let completed = read_scheduled_with(
            |buffer, offset| {
                calls.push((buffer.len(), offset));
                step += 1;
                match step {
                    1 => {
                        buffer[..2].copy_from_slice(b"ab");
                        Ok(2)
                    }
                    2 => {
                        buffer[..1].copy_from_slice(b"c");
                        Ok(1)
                    }
                    _ => panic!("no further read expected"),
                }
            },
            admission,
        )
        .expect("the scripted reads fill the range");

        assert_eq!(calls, vec![(3, 10), (1, 12)]);
        assert_eq!(completed.range(), range(10, 3));
        assert_eq!(completed.bytes(), b"abc");
        assert_eq!(scheduler.in_flight_bytes(), 3);
    }

    #[test]
    fn read_scheduled_with_retries_interruption_without_advancing_or_releasing() {
        let (scheduler, admission) = admitted_single(10, 3, 3);
        let mut calls = Vec::new();
        let mut step = 0;

        let completed = read_scheduled_with(
            |buffer, offset| {
                calls.push((buffer.len(), offset));
                step += 1;
                match step {
                    1 => Err(io::Error::new(ErrorKind::Interrupted, "signal")),
                    2 => {
                        buffer.copy_from_slice(b"abc");
                        Ok(3)
                    }
                    _ => panic!("no further read expected"),
                }
            },
            admission,
        )
        .expect("the retried read fills the range");

        assert_eq!(calls, vec![(3, 10), (3, 10)]);
        assert_eq!(completed.bytes(), b"abc");
        assert_eq!(scheduler.in_flight_bytes(), 3);

        drop(completed);
        assert_eq!(scheduler.in_flight_bytes(), 0);
    }

    #[test]
    fn read_scheduled_with_reports_eof_before_the_first_byte_and_restores_the_budget() {
        let (scheduler, admission) = admitted_single(10, 4, 4);

        let error = read_scheduled_with(|_buffer, _offset| Ok(0), admission)
            .expect_err("the file ends before the range");

        assert!(matches!(
            error,
            ReadError::UnexpectedEof {
                expected: 4,
                actual: 0,
                ..
            }
        ));
        assert_eq!(scheduler.in_flight_bytes(), 0);
        assert_eq!(scheduler.available_bytes(), 4);
    }

    #[test]
    fn read_scheduled_with_preserves_the_io_error_and_restores_the_budget() {
        let (scheduler, admission) = admitted_single(10, 4, 4);
        let mut step = 0;

        let error = read_scheduled_with(
            |buffer, _offset| {
                step += 1;
                match step {
                    1 => {
                        buffer[..2].copy_from_slice(b"ab");
                        Ok(2)
                    }
                    _ => Err(io::Error::new(ErrorKind::PermissionDenied, "denied")),
                }
            },
            admission,
        )
        .expect_err("the second read fails");

        assert!(std::error::Error::source(&error).is_some());

        let ReadError::Io {
            range: failed,
            offset,
            source,
        } = error
        else {
            panic!("expected an I/O error, got {error:?}");
        };
        assert_eq!(failed, range(10, 4));
        assert_eq!(offset, 12);
        assert_eq!(source.kind(), ErrorKind::PermissionDenied);
        assert_eq!(scheduler.in_flight_bytes(), 0);
        assert_eq!(scheduler.available_bytes(), 4);
    }

    #[test]
    fn read_range_exact_rejects_an_unallocatable_length() {
        // On 64-bit targets `u64::MAX` converts to `usize`, so the fallible
        // reservation is what must reject the request, before any read runs.
        // No `ScheduledRead` can carry such a length any more — planning caps
        // one physical read at `ReadSize::MAX_BYTES` — so the raw range is
        // the only way to exercise this allocation guard; the budget-release
        // behavior of a failing scheduled read stays covered by the EOF and
        // I/O failure tests above.
        let error = read_range_exact(
            |_buffer, _offset| panic!("no read is expected before a buffer exists"),
            range(0, u64::MAX),
        )
        .expect_err("no buffer of u64::MAX bytes can be reserved");

        assert!(matches!(error, ReadError::BufferAllocation { .. }));
    }
}
