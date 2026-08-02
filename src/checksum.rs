//! Deterministic per-range SHA-256 checksums over completed range outputs.
//!
//! A checksum here is compact audit evidence: for equal inputs, later backends
//! must produce byte-for-byte identical outputs *and* matching checksums.
//! Because hash collisions are theoretically possible, checksum equality is
//! never treated as mathematical proof of byte equality; it complements the
//! byte comparison instead of replacing it.
//!
//! Only the payload bytes of one successful [`RangeOutput`] are hashed. The
//! range travels next to the digest as separate metadata, so no framing or
//! serialization protocol is invented while a naked digest still cannot pose
//! as a complete range-output identity.

use sha2::{Digest, Sha256};

use crate::output::RangeOutput;
use crate::range::ReadRange;

/// The SHA-256 checksum of one completed range output.
///
/// The digest covers the payload bytes only; the [`ReadRange`] is carried
/// alongside it as metadata and is never fed into the hash. Two outputs with
/// equal bytes at different ranges therefore have equal [`Self::sha256`]
/// values but unequal complete `RangeChecksum` values.
///
/// Both fields stay private: a value only exists for bytes that were actually
/// read by the backend, so no caller can associate arbitrary bytes with a
/// range.
///
/// Checksum equality is compact evidence, not collision-free proof that two
/// byte sequences are equal; the correctness contract still requires
/// byte-for-byte comparison between backends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeChecksum {
    range: ReadRange,
    sha256: [u8; 32],
}

impl RangeChecksum {
    /// Returns the range whose payload bytes the checksum covers.
    #[must_use]
    pub const fn range(&self) -> ReadRange {
        self.range
    }

    /// Returns the raw 32 SHA-256 bytes of the payload.
    ///
    /// The value is the fixed-size digest of the payload bytes only; the
    /// associated range is exposed by [`Self::range`] and never influences
    /// these bytes.
    #[must_use]
    pub const fn sha256(&self) -> [u8; 32] {
        self.sha256
    }
}

/// Computes the deterministic SHA-256 checksum of one completed range output.
///
/// Exactly the payload bytes of the borrowed [`RangeOutput`] are hashed; the
/// associated range is copied next to the digest without being fed into the
/// hash. Hashing bytes that are already owned in memory cannot fail, so the
/// call is infallible and performs no I/O, validation, sorting, or coalescing.
///
/// For equal byte sequences the digest is identical on every supported
/// machine and across repeated executions.
#[must_use]
pub fn checksum(output: &RangeOutput) -> RangeChecksum {
    RangeChecksum {
        range: output.range(),
        sha256: Sha256::digest(output.bytes()).into(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::PathBuf;
    use std::{env, fs, process};

    use super::{RangeChecksum, checksum};
    use crate::output::RangeOutput;
    use crate::plan::ReadPlan;
    use crate::pread::read_plan;
    use crate::range::ReadRange;

    /// The NIST known-answer SHA-256 digest of the three bytes `abc`.
    const ABC_SHA256: [u8; 32] = [
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22,
        0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00,
        0x15, 0xad,
    ];

    fn range(offset: u64, length: u64) -> ReadRange {
        ReadRange::try_new(offset, length).expect("test ranges are valid")
    }

    fn fixture_path(test: &str) -> PathBuf {
        env::temp_dir().join(format!("range-replay-checksum-{test}-{}", process::id()))
    }

    /// Produces real backend outputs for `ranges` over a file holding `data`.
    fn outputs_from_fixture(test: &str, data: &[u8], ranges: &[ReadRange]) -> Vec<RangeOutput> {
        let path = fixture_path(test);
        fs::write(&path, data).expect("fixture file is writable");
        let file = File::open(&path).expect("fixture file opens");

        let plan = ReadPlan::try_from_schedule(ranges).expect("test schedules are not empty");
        let outputs = read_plan(&file, &plan).expect("test ranges are inside the fixture");

        drop(file);
        fs::remove_file(&path).expect("fixture file is removable");

        outputs
    }

    #[test]
    fn checksum_matches_the_abc_known_answer_vector() {
        let outputs = outputs_from_fixture("known-answer", b"abc", &[range(0, 3)]);

        assert_eq!(checksum(&outputs[0]).sha256(), ABC_SHA256);
    }

    #[test]
    fn checksum_preserves_the_range_of_the_borrowed_output() {
        let outputs = outputs_from_fixture("range-association", b"0123456789", &[range(4, 3)]);

        assert_eq!(checksum(&outputs[0]).range(), range(4, 3));
    }

    #[test]
    fn checksum_is_deterministic_for_repeated_calculation() {
        let outputs = outputs_from_fixture("deterministic", b"abc", &[range(0, 3)]);

        assert_eq!(checksum(&outputs[0]), checksum(&outputs[0]));
    }

    #[test]
    fn changing_one_payload_byte_changes_the_sha256_value() {
        let base = outputs_from_fixture("mutation-base", b"abc", &[range(0, 3)]);
        let mutated = outputs_from_fixture("mutation-mutated", b"abd", &[range(0, 3)]);

        assert_ne!(checksum(&base[0]).sha256(), checksum(&mutated[0]).sha256());
    }

    #[test]
    fn equal_bytes_at_different_ranges_share_the_sha256_but_not_the_checksum() {
        let outputs =
            outputs_from_fixture("distinct-ranges", b"abcXabc", &[range(0, 3), range(4, 3)]);
        assert_eq!(outputs[0].bytes(), outputs[1].bytes());

        let first: RangeChecksum = checksum(&outputs[0]);
        let second: RangeChecksum = checksum(&outputs[1]);

        assert_eq!(first.sha256(), second.sha256());
        assert_ne!(first.range(), second.range());
        assert_ne!(first, second);
    }
}
