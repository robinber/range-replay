//! Validated file-range requests.
//!
//! A [`ReadRange`] is the smallest unit a later schedule, coalescer, or backend
//! operates on. It describes *where* to start reading and *how many* bytes to
//! request; it never touches a file.

use thiserror::Error;

/// Reason a [`ReadRange`] could not be constructed.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum RangeError {
    /// The requested length was `0`, so the range would cover no byte at all.
    #[error("range length must be greater than zero")]
    ZeroLength,
    /// The exclusive end `offset + length` is not representable as a `u64`.
    #[error("range end overflows u64")]
    EndOverflow,
}

/// A validated half-open file-range request.
///
/// The range covers the byte offsets in `[offset, offset + length)`: the start
/// offset is included and the end offset is excluded. An `offset` of `100` with
/// a `length` of `4` therefore covers offsets `100`, `101`, `102`, and `103`,
/// and its exclusive end is `104`.
///
/// Half-open bounds keep adjacency arithmetic trivial for the coalescing work
/// that comes later: two ranges touch exactly when one range's end equals the
/// other range's offset, with no off-by-one correction.
///
/// The only way to build a value is [`ReadRange::try_new`], so every existing
/// `ReadRange` covers at least one byte and has an exclusive end that fits in a
/// `u64`. Nothing here checks the range against a real file: sizes and EOF
/// belong to a later slice.
///
/// # Examples
///
/// ```
/// use range_replay::{RangeError, ReadRange};
///
/// let range = ReadRange::try_new(100, 4)?;
/// assert_eq!(range.offset(), 100);
/// assert_eq!(range.length(), 4);
/// assert_eq!(range.end(), 104);
///
/// assert_eq!(ReadRange::try_new(100, 0), Err(RangeError::ZeroLength));
/// assert_eq!(ReadRange::try_new(u64::MAX, 1), Err(RangeError::EndOverflow));
/// # Ok::<(), RangeError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadRange {
    offset: u64,
    length: u64,
}

impl ReadRange {
    /// Creates a range covering `length` bytes starting at `offset`.
    ///
    /// # Errors
    ///
    /// Returns [`RangeError::ZeroLength`] when `length` is `0`, and
    /// [`RangeError::EndOverflow`] when the exclusive end `offset + length`
    /// does not fit in a `u64`.
    pub const fn try_new(offset: u64, length: u64) -> Result<Self, RangeError> {
        if length == 0 {
            return Err(RangeError::ZeroLength);
        }

        match offset.checked_add(length) {
            Some(_) => Ok(Self { offset, length }),
            None => Err(RangeError::EndOverflow),
        }
    }

    /// Returns the inclusive start offset of the range.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the byte count covered by the range, which is always at least
    /// `1`.
    #[must_use]
    pub const fn length(&self) -> u64 {
        self.length
    }

    /// Returns the exclusive end offset of the range, which is always
    /// representable as a `u64`.
    #[must_use]
    pub const fn end(&self) -> u64 {
        // `try_new` rejects any range whose end is not representable, so
        // saturation is unreachable here and only keeps this accessor free of
        // an overflow panic path.
        self.offset.saturating_add(self.length)
    }
}

#[cfg(test)]
mod tests {
    use super::{RangeError, ReadRange};

    #[test]
    fn try_new_accepts_a_non_empty_range() {
        let range = ReadRange::try_new(100, 4).expect("100 + 4 fits in u64");

        assert_eq!(range.offset(), 100);
        assert_eq!(range.length(), 4);
        assert_eq!(range.end(), 104);
    }

    #[test]
    fn try_new_rejects_a_zero_length() {
        assert_eq!(ReadRange::try_new(100, 0), Err(RangeError::ZeroLength));
    }

    #[test]
    fn try_new_rejects_an_unrepresentable_end() {
        assert_eq!(
            ReadRange::try_new(u64::MAX, 1),
            Err(RangeError::EndOverflow)
        );
    }

    #[test]
    fn try_new_accepts_the_largest_representable_end() {
        let range = ReadRange::try_new(u64::MAX - 1, 1).expect("u64::MAX is a representable end");

        assert_eq!(range.offset(), u64::MAX - 1);
        assert_eq!(range.length(), 1);
        assert_eq!(range.end(), u64::MAX);
    }
}
