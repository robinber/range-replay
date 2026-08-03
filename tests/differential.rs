//! Differential tests proving the reference oracle and the executor agree.
//!
//! [`read_plan`] is the synchronous reference oracle; [`execute_pread`] is
//! the budget-aware executor. For equal inputs the correctness contract
//! requires identical logical outputs from both — the same canonical ranges
//! in the same order, byte-for-byte equal payloads, and matching checksums —
//! and agreement on failure when a range crosses end of file. The in-memory
//! fixture bytes serve as a third, independent expectation on the success
//! path, so a bug shared by both backends cannot hide behind their mutual
//! agreement.
//!
//! Hand-calculated cases pin exact expectations; proptest cases sweep small
//! random files, schedules, and configurations. A failing proptest input is
//! persisted under `proptest-regressions/`, which must be committed so the
//! shrunk counterexample replays on every later run.
#![expect(
    unused_crate_dependencies,
    reason = "only the library target and proptest are exercised by this differential test"
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, fs, process};

use proptest::prelude::{Just, Strategy, any, proptest};
use proptest::{collection, prop_compose};
use range_replay::{
    ByteBudget, ExecutionConfig, ExecutionPlan, RangeOutput, ReadPlan, ReadRange, ReadSize,
    checksum, execute_pread, read_plan,
};

/// Distinguishes concurrently created fixture files within one process.
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

/// A temporary fixture file removed when the test case ends.
struct Fixture {
    path: PathBuf,
}

impl Fixture {
    #[expect(
        clippy::expect_used,
        reason = "test helpers panic with diagnostics like the tests they serve"
    )]
    fn new(contents: &[u8]) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path =
            env::temp_dir().join(format!("range-replay-differential-{}-{id}", process::id()));
        fs::write(&path, contents).expect("fixture file is writable");

        Self { path }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _removed = fs::remove_file(&self.path);
    }
}

#[expect(
    clippy::expect_used,
    reason = "test helpers panic with diagnostics like the tests they serve"
)]
fn ranges(pairs: &[(u64, u64)]) -> Vec<ReadRange> {
    pairs
        .iter()
        .map(|&(offset, length)| ReadRange::try_new(offset, length).expect("test ranges are valid"))
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "test helpers panic with diagnostics like the tests they serve"
)]
fn config(read_size: u64, byte_budget: u64) -> ExecutionConfig {
    ExecutionConfig::try_new(
        ReadSize::try_new(read_size).expect("test read sizes are non-zero"),
        ByteBudget::try_new(byte_budget).expect("test budgets are non-zero"),
    )
    .expect("test read sizes fit under test budgets")
}

/// Runs both backends over one fixture and returns their outputs.
#[expect(
    clippy::expect_used,
    reason = "test helpers panic with diagnostics like the tests they serve"
)]
fn run_both_backends(
    data: &[u8],
    schedule: &[(u64, u64)],
    read_size: u64,
    byte_budget: u64,
) -> (Vec<RangeOutput>, Vec<RangeOutput>) {
    let plan = ReadPlan::try_from_schedule(&ranges(schedule)).expect("test schedules are valid");
    let execution = ExecutionPlan::try_from_read_plan(&plan, config(read_size, byte_budget))
        .expect("valid plans and configurations derive a physical plan");

    let fixture = Fixture::new(data);
    let file = fs::File::open(&fixture.path).expect("fixture file opens");

    let oracle = read_plan(&file, &plan).expect("the oracle reads in-bounds test ranges");
    let executed =
        execute_pread(&file, execution).expect("the executor reads in-bounds test ranges");

    (oracle, executed)
}

/// Asserts full agreement between both backends and the in-memory truth.
#[expect(
    clippy::expect_used,
    reason = "test helpers panic with diagnostics like the tests they serve"
)]
fn assert_backends_agree(data: &[u8], schedule: &[(u64, u64)], read_size: u64, byte_budget: u64) {
    let (oracle, executed) = run_both_backends(data, schedule, read_size, byte_budget);

    assert_eq!(
        oracle.len(),
        executed.len(),
        "both backends produce one output per canonical range"
    );

    for (oracle_output, executed_output) in oracle.iter().zip(&executed) {
        assert_eq!(oracle_output.range(), executed_output.range());
        assert_eq!(
            oracle_output.bytes(),
            executed_output.bytes(),
            "payloads are byte-for-byte identical for range {:?}",
            oracle_output.range()
        );
        assert_eq!(checksum(oracle_output), checksum(executed_output));

        let offset =
            usize::try_from(oracle_output.range().offset()).expect("test offsets fit in usize");
        let end = offset
            + usize::try_from(oracle_output.range().length()).expect("test lengths fit in usize");
        assert_eq!(
            oracle_output.bytes(),
            &data[offset..end],
            "the oracle matches the in-memory fixture bytes"
        );
    }
}

/// Asserts both backends reject a schedule whose canonical plan crosses EOF.
#[expect(
    clippy::expect_used,
    reason = "test helpers panic with diagnostics like the tests they serve"
)]
fn assert_backends_agree_on_eof(
    data: &[u8],
    schedule: &[(u64, u64)],
    read_size: u64,
    byte_budget: u64,
) {
    let plan = ReadPlan::try_from_schedule(&ranges(schedule)).expect("test schedules are valid");
    let execution = ExecutionPlan::try_from_read_plan(&plan, config(read_size, byte_budget))
        .expect("valid plans and configurations derive a physical plan");

    let fixture = Fixture::new(data);
    let file = fs::File::open(&fixture.path).expect("fixture file opens");

    assert!(
        read_plan(&file, &plan).is_err(),
        "the oracle rejects a plan crossing EOF"
    );
    assert!(
        execute_pread(&file, execution).is_err(),
        "the executor rejects a plan crossing EOF"
    );
}

#[test]
fn backends_agree_on_the_hand_calculated_fixture() {
    let (oracle, executed) = run_both_backends(b"0123456789abcdef", &[(10, 4), (2, 3)], 4, 10);

    let expected: Vec<(u64, u64, &[u8])> = vec![(2, 3, b"234"), (10, 4, b"abcd")];
    for outputs in [&oracle, &executed] {
        assert_eq!(outputs.len(), expected.len());
        for (output, &(offset, length, bytes)) in outputs.iter().zip(&expected) {
            assert_eq!(output.range().offset(), offset);
            assert_eq!(output.range().length(), length);
            assert_eq!(output.bytes(), bytes);
        }
    }

    assert_backends_agree(b"0123456789abcdef", &[(10, 4), (2, 3)], 4, 10);
}

#[test]
fn backends_agree_when_every_physical_read_is_one_byte() {
    assert_backends_agree(b"0123456789abcdef", &[(2, 2), (0, 2), (1, 1), (9, 5)], 1, 1);
}

#[test]
fn backends_agree_when_one_read_covers_the_whole_file() {
    assert_backends_agree(b"0123456789abcdef", &[(0, 16)], 1000, 1000);
}

#[test]
fn backends_agree_on_a_single_byte_file() {
    assert_backends_agree(&[0xff], &[(0, 1)], 1, 1);
}

#[test]
fn backends_agree_on_failure_for_the_hand_calculated_eof_schedule() {
    assert_backends_agree_on_eof(b"0123456789abcdef", &[(0, 4), (100, 4)], 4, 10);
}

prop_compose! {
    /// One non-empty small file plus a schedule fully inside its bounds.
    fn file_with_in_bounds_schedule()(data in collection::vec(any::<u8>(), 1..=200))(
        schedule in in_bounds_schedule(data.len()),
        data in Just(data),
    ) -> (Vec<u8>, Vec<(u64, u64)>) {
        (data, schedule)
    }
}

#[expect(
    clippy::expect_used,
    reason = "test helpers panic with diagnostics like the tests they serve"
)]
fn in_bounds_schedule(file_len: usize) -> impl Strategy<Value = Vec<(u64, u64)>> {
    let len = u64::try_from(file_len).expect("test files are small");

    collection::vec(
        (0..len).prop_flat_map(move |offset| (Just(offset), 1..=(len - offset))),
        1..=6,
    )
}

/// One validated configuration pair with `read_size <= byte_budget`.
fn valid_config() -> impl Strategy<Value = (u64, u64)> {
    (1_u64..=48).prop_flat_map(|read_size| (Just(read_size), read_size..=144))
}

proptest! {
    #[test]
    fn backends_agree_for_random_in_bounds_schedules(
        (data, schedule) in file_with_in_bounds_schedule(),
        (read_size, byte_budget) in valid_config(),
    ) {
        assert_backends_agree(&data, &schedule, read_size, byte_budget);
    }

    #[test]
    fn backends_agree_on_failure_for_random_eof_crossing_schedules(
        (data, schedule) in file_with_eof_crossing_schedule(),
        (read_size, byte_budget) in valid_config(),
    ) {
        assert_backends_agree_on_eof(&data, &schedule, read_size, byte_budget);
    }
}

prop_compose! {
    /// One non-empty small file plus a schedule with one range past its end.
    fn file_with_eof_crossing_schedule()(data in collection::vec(any::<u8>(), 1..=200))(
        schedule in eof_crossing_schedule(data.len()),
        data in Just(data),
    ) -> (Vec<u8>, Vec<(u64, u64)>) {
        (data, schedule)
    }
}

#[expect(
    clippy::expect_used,
    reason = "test helpers panic with diagnostics like the tests they serve"
)]
fn eof_crossing_schedule(file_len: usize) -> impl Strategy<Value = Vec<(u64, u64)>> {
    let len = u64::try_from(file_len).expect("test files are small");

    let crossing = (0..=len).prop_flat_map(move |offset| {
        let minimum_crossing_length = len - offset + 1;
        (
            Just(offset),
            minimum_crossing_length..=(minimum_crossing_length + 40),
        )
    });

    (in_bounds_schedule(file_len), crossing).prop_map(|(mut schedule, crossing)| {
        schedule.push(crossing);
        schedule
    })
}
