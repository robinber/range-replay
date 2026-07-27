//! Parsing of textual range schedules.
//!
//! A schedule is the caller's ordered list of read requests. Parsing turns
//! text into validated [`ReadRange`] values and nothing more: sorting and
//! merging belong to [`coalesce`](crate::coalesce), which reduces a schedule
//! to a canonical plan while the original schedule stays available for
//! provenance and reporting.

use std::num::ParseIntError;

use thiserror::Error;

use crate::range::{RangeError, ReadRange};

/// Reason a textual schedule could not be parsed.
///
/// Every variant names the one-based physical line number of the first
/// invalid line, counting ignored blank lines, so the error points back into
/// the source text. [`RangeError`] stays the source of
/// [`ScheduleError::InvalidRange`] rather than being flattened away: both
/// fields of a line can parse as integers and still describe no valid range.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ScheduleError {
    /// The line did not hold exactly two comma-separated fields.
    #[error("line {line}: expected exactly two comma-separated fields")]
    InvalidFieldCount {
        /// One-based physical line number of the invalid line.
        line: usize,
    },
    /// The offset field was not a base-10 `u64`.
    #[error("line {line}: invalid offset")]
    InvalidOffset {
        /// One-based physical line number of the invalid line.
        line: usize,
        /// The underlying integer parse failure.
        source: ParseIntError,
    },
    /// The length field was not a base-10 `u64`.
    #[error("line {line}: invalid length")]
    InvalidLength {
        /// One-based physical line number of the invalid line.
        line: usize,
        /// The underlying integer parse failure.
        source: ParseIntError,
    },
    /// Both fields parsed as integers, but they describe no valid range.
    #[error("line {line}: invalid range")]
    InvalidRange {
        /// One-based physical line number of the invalid line.
        line: usize,
        /// The underlying range validation failure.
        source: RangeError,
    },
}

/// Parses a textual schedule into an ordered list of validated ranges.
///
/// Each non-blank physical line describes one range as two comma-separated
/// base-10 `u64` fields:
///
/// ```text
/// offset,length
/// ```
///
/// Whitespace surrounding a line or a field is ignored, and blank or
/// whitespace-only lines are skipped. Exactly two fields are required; a
/// missing or additional field is an error, never silently dropped. The
/// format has no headers, comments, or quoting: any other non-blank line is
/// invalid input.
///
/// The returned schedule is newly owned, contains only ranges built through
/// [`ReadRange::try_new`], and preserves the source order of the range lines.
/// Parsing never sorts or merges; pass the schedule to
/// [`coalesce`](crate::coalesce) to produce the canonical plan. Empty or
/// whitespace-only input parses to an empty `Vec`: rejecting an empty
/// schedule stays a planning decision
/// ([`PlanError::EmptySchedule`](crate::PlanError::EmptySchedule)), not a
/// parsing one.
///
/// Parsing is pure and deterministic: it performs no I/O, leaves the borrowed
/// input untouched, and returns the same result for equal inputs.
///
/// # Errors
///
/// Returns the [`ScheduleError`] describing the first invalid non-blank line,
/// identified by its one-based physical line number with blank lines counted.
/// The whole input is rejected: no partial schedule is ever observable.
///
/// # Examples
///
/// ```
/// use range_replay::{ReadRange, ScheduleError, parse_schedule};
///
/// let schedule = parse_schedule("10,5\n12, 8\n\n30,2\n")?;
///
/// assert_eq!(
///     schedule,
///     vec![
///         ReadRange::try_new(10, 5)?,
///         ReadRange::try_new(12, 8)?,
///         ReadRange::try_new(30, 2)?,
///     ],
/// );
///
/// assert_eq!(parse_schedule(""), Ok(Vec::new()));
/// assert_eq!(
///     parse_schedule("100,4,999"),
///     Err(ScheduleError::InvalidFieldCount { line: 1 })
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn parse_schedule(input: &str) -> Result<Vec<ReadRange>, ScheduleError> {
    let mut schedule = Vec::new();

    for (index, raw_line) in input.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw_line.trim();

        if trimmed.is_empty() {
            continue;
        }

        let mut fields = trimmed.split(',');
        let (Some(offset_field), Some(length_field), None) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Err(ScheduleError::InvalidFieldCount { line });
        };

        let offset = offset_field
            .trim()
            .parse()
            .map_err(|source| ScheduleError::InvalidOffset { line, source })?;
        let length = length_field
            .trim()
            .parse()
            .map_err(|source| ScheduleError::InvalidLength { line, source })?;
        let range = ReadRange::try_new(offset, length)
            .map_err(|source| ScheduleError::InvalidRange { line, source })?;

        schedule.push(range);
    }

    Ok(schedule)
}

#[cfg(test)]
mod tests {
    use super::{ScheduleError, parse_schedule};
    use crate::range::{RangeError, ReadRange};

    fn range(offset: u64, length: u64) -> ReadRange {
        ReadRange::try_new(offset, length).expect("test ranges are valid")
    }

    #[test]
    fn parse_schedule_accepts_a_single_line() {
        assert_eq!(parse_schedule("10,5"), Ok(vec![range(10, 5)]));
    }

    #[test]
    fn parse_schedule_preserves_the_source_order() {
        assert_eq!(
            parse_schedule("30,2\n10,5\n12,8"),
            Ok(vec![range(30, 2), range(10, 5), range(12, 8)])
        );
    }

    #[test]
    fn parse_schedule_trims_line_and_field_whitespace() {
        assert_eq!(
            parse_schedule("  10 , 5  \n\t12,\t8\n"),
            Ok(vec![range(10, 5), range(12, 8)])
        );
    }

    #[test]
    fn parse_schedule_ignores_blank_and_whitespace_only_lines() {
        assert_eq!(
            parse_schedule("10,5\n\n   \n\t\n30,2"),
            Ok(vec![range(10, 5), range(30, 2)])
        );
    }

    #[test]
    fn parse_schedule_reports_the_physical_line_number_after_blank_lines() {
        assert_eq!(
            parse_schedule("10,5\n\n   \n100"),
            Err(ScheduleError::InvalidFieldCount { line: 4 })
        );
    }

    #[test]
    fn parse_schedule_returns_an_empty_schedule_for_blank_input() {
        assert_eq!(parse_schedule(""), Ok(Vec::new()));
        assert_eq!(parse_schedule(" \n\t\n  "), Ok(Vec::new()));
    }

    #[test]
    fn parse_schedule_rejects_a_missing_field() {
        assert_eq!(
            parse_schedule("100"),
            Err(ScheduleError::InvalidFieldCount { line: 1 })
        );
    }

    #[test]
    fn parse_schedule_rejects_an_additional_field() {
        assert_eq!(
            parse_schedule("100,4,999"),
            Err(ScheduleError::InvalidFieldCount { line: 1 })
        );
    }

    #[test]
    fn parse_schedule_distinguishes_offset_and_length_parse_failures() {
        let source = "abc".parse::<u64>().expect_err("not a base-10 u64");

        assert_eq!(
            parse_schedule("abc,5"),
            Err(ScheduleError::InvalidOffset {
                line: 1,
                source: source.clone(),
            })
        );
        assert_eq!(
            parse_schedule("5,abc"),
            Err(ScheduleError::InvalidLength { line: 1, source })
        );
    }

    #[test]
    fn parse_schedule_rejects_a_zero_length_as_an_invalid_range() {
        assert_eq!(
            parse_schedule("100,0"),
            Err(ScheduleError::InvalidRange {
                line: 1,
                source: RangeError::ZeroLength,
            })
        );
    }

    #[test]
    fn parse_schedule_rejects_an_unrepresentable_end_as_an_invalid_range() {
        let max = u64::MAX;

        assert_eq!(
            parse_schedule(&format!("{max},1")),
            Err(ScheduleError::InvalidRange {
                line: 1,
                source: RangeError::EndOverflow,
            })
        );
    }

    #[test]
    fn parse_schedule_accepts_the_largest_representable_end() {
        let offset = u64::MAX - 1;

        assert_eq!(
            parse_schedule(&format!("{offset},1")),
            Ok(vec![range(offset, 1)])
        );
    }

    #[test]
    fn parse_schedule_rejects_the_entire_input_on_one_invalid_line() {
        let source = "abc".parse::<u64>().expect_err("not a base-10 u64");

        assert_eq!(
            parse_schedule("10,5\nabc,5\n30,2"),
            Err(ScheduleError::InvalidOffset { line: 2, source })
        );
    }
}
