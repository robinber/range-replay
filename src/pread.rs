//! Synchronous positioned-read reference backend.
//!
//! This backend executes an already validated [`ReadPlan`] against an already
//! open [`File`] through the safe Unix positioned-read API
//! ([`FileExt::read_at`], `pread` semantics). Planning stays pure; this module
//! owns the first real I/O boundary and is the correctness reference that
//! later backends must match byte for byte.

use std::collections::TryReserveError;
use std::fs::File;
use std::io;
use std::num::TryFromIntError;
use std::os::unix::fs::FileExt;

use thiserror::Error;

use crate::plan::ReadPlan;
use crate::range::ReadRange;

/// Reason executing a read plan against a file failed.
///
/// Every variant carries the range whose read failed, so the error points
/// back into the plan. Execution is fail-closed: the first failing range
/// aborts the whole call, and output for previously completed ranges is never
/// observable.
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

/// One fully read range and the owned bytes covering it exactly.
///
/// A value only exists after a successful exact read, so the bytes always
/// cover the associated range completely: `bytes().len()` equals the range
/// length. Both fields stay private so no caller can construct or mutate an
/// output that breaks this invariant.
#[derive(Debug, PartialEq, Eq)]
pub struct RangeOutput {
    range: ReadRange,
    bytes: Vec<u8>,
}

impl RangeOutput {
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
/// reports more bytes than the unfilled remainder, and [`ReadError::Io`] for
/// every other I/O failure, preserving the original [`io::Error`] as its
/// source.
///
/// # Examples
///
/// ```
/// use std::fs::File;
///
/// use range_replay::{ReadPlan, ReadRange, read_plan};
///
/// let path = std::env::temp_dir().join("range-replay-doc-read-plan");
/// std::fs::write(&path, b"0123456789abcdef")?;
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
            read_range_exact(|buffer, offset| file.read_at(buffer, offset), range)
                .map(|bytes| RangeOutput { range, bytes })
        })
        .collect()
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
    use std::io::{ErrorKind, Seek, SeekFrom};
    use std::path::PathBuf;
    use std::{env, fs, io, process};

    use super::{ReadError, read_plan, read_range_exact};
    use crate::plan::ReadPlan;
    use crate::range::ReadRange;

    const FIXTURE: &[u8] = b"0123456789abcdef";

    fn range(offset: u64, length: u64) -> ReadRange {
        ReadRange::try_new(offset, length).expect("test ranges are valid")
    }

    fn plan(schedule: &[ReadRange]) -> ReadPlan {
        ReadPlan::try_from_schedule(schedule).expect("test schedules are not empty")
    }

    fn fixture_path(test: &str) -> PathBuf {
        env::temp_dir().join(format!("range-replay-pread-{test}-{}", process::id()))
    }

    fn with_fixture_file<T>(test: &str, run: impl FnOnce(&mut File) -> T) -> T {
        let path = fixture_path(test);
        fs::write(&path, FIXTURE).expect("fixture file is writable");
        let mut file = File::open(&path).expect("fixture file opens");

        let result = run(&mut file);

        drop(file);
        fs::remove_file(&path).expect("fixture file is removable");

        result
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
    fn read_range_exact_rejects_an_unallocatable_length() {
        // On 64-bit targets `u64::MAX` converts to `usize`, so the fallible
        // reservation is what must reject the request, before any read runs.
        let error = read_range_exact(
            |_buffer, _offset| panic!("no read is expected before a buffer exists"),
            range(0, u64::MAX),
        )
        .expect_err("no buffer of u64::MAX bytes can be reserved");

        assert!(matches!(error, ReadError::BufferAllocation { .. }));
    }
}
