//! Differential tests proving the reference oracle and the executor agree.
//!
//! [`read_plan`] is the synchronous reference oracle; [`execute_pread`] is
//! the budget-aware executor, and on Linux [`execute_uring`] is the bounded
//! `io_uring` backend exercised at several queue depths. For equal inputs
//! the correctness contract requires identical logical outputs from every
//! backend — the same canonical ranges in the same order with byte-for-byte
//! equal payloads — and agreement on a typed end-of-file failure when the
//! canonical plan crosses EOF. Each
//! output sequence is compared against the canonical plan itself, not only
//! against the other backend, so a regression shared by both — omitting,
//! duplicating, or identically reordering canonical ranges — cannot pass on
//! mutual agreement alone. The in-memory fixture bytes serve as a further
//! independent expectation for every payload on the success path.
//!
//! Checksum agreement is implied rather than asserted: the checksum is a
//! deterministic function of the payload bytes alone, pinned by the
//! known-answer tests in `src/checksum.rs`, so byte-for-byte equal outputs
//! cannot produce unequal checksums and a separate assertion here would be
//! vacuous.
//!
//! The suite observes logical outputs and terminal errors only, so it
//! cannot see budget admission: an executor that admitted physical reads
//! beyond the in-flight budget could still return correct bytes here. That
//! hard admission invariant is owned by the scripted-session tests in
//! `src/executor/tests.rs` and the limiter tests in `src/budget.rs`.
//!
//! Hand-calculated cases pin exact expectations; proptest cases sweep small
//! random files, schedules, and configurations. A failing proptest input is
//! persisted next to this file in `differential.proptest-regressions`, which
//! must be committed so the shrunk counterexample replays on every later run.
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
    ByteBudget, ExecutionConfig, ExecutionPlan, PreadExecutionError, RangeOutput, ReadError,
    ReadPlan, ReadRange, ReadSize, execute_pread, read_plan,
};
#[cfg(target_os = "linux")]
use range_replay::{UringExecutionError, UringQueueDepth, execute_uring};

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
        ReadSize::try_new(read_size).expect("test read sizes are within the valid domain"),
        ByteBudget::try_new(byte_budget).expect("test budgets are non-zero"),
    )
    .expect("test read sizes fit under test budgets")
}

/// Runs both backends over one fixture and returns the canonical plan next
/// to their outputs.
#[expect(
    clippy::expect_used,
    reason = "test helpers panic with diagnostics like the tests they serve"
)]
fn run_both_backends(
    data: &[u8],
    schedule: &[(u64, u64)],
    read_size: u64,
    byte_budget: u64,
) -> (ReadPlan, Vec<RangeOutput>, Vec<RangeOutput>) {
    let plan = ReadPlan::try_from_schedule(&ranges(schedule)).expect("test schedules are valid");
    let execution = ExecutionPlan::try_from_read_plan(&plan, config(read_size, byte_budget))
        .expect("valid plans and configurations derive a physical plan");

    let fixture = Fixture::new(data);
    let file = fs::File::open(&fixture.path).expect("fixture file opens");

    let oracle = read_plan(&file, &plan).expect("the oracle reads in-bounds test ranges");
    let executed =
        execute_pread(&file, execution).expect("the executor reads in-bounds test ranges");

    (plan, oracle, executed)
}

/// Asserts full agreement between both backends, the canonical plan, and
/// the in-memory truth.
///
/// The canonical plan is the structural expectation: each backend must
/// return exactly one output per canonical range, in plan order. Comparing
/// both sequences against the plan — not only against each other — catches
/// a shared regression that omits, duplicates, or identically reorders
/// canonical ranges.
#[expect(
    clippy::expect_used,
    reason = "test helpers panic with diagnostics like the tests they serve"
)]
fn assert_backends_agree(data: &[u8], schedule: &[(u64, u64)], read_size: u64, byte_budget: u64) {
    let (plan, oracle, executed) = run_both_backends(data, schedule, read_size, byte_budget);

    let oracle_ranges: Vec<ReadRange> = oracle.iter().map(RangeOutput::range).collect();
    assert_eq!(
        oracle_ranges,
        plan.ranges(),
        "the oracle returns exactly the canonical ranges in plan order"
    );

    let executed_ranges: Vec<ReadRange> = executed.iter().map(RangeOutput::range).collect();
    assert_eq!(
        executed_ranges,
        plan.ranges(),
        "the executor returns exactly the canonical ranges in plan order"
    );

    for (oracle_output, executed_output) in oracle.iter().zip(&executed) {
        assert_eq!(
            oracle_output.bytes(),
            executed_output.bytes(),
            "payloads are byte-for-byte identical for range {:?}",
            oracle_output.range()
        );

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

    #[cfg(target_os = "linux")]
    assert_uring_agrees(data, schedule, read_size, byte_budget, &oracle);
}

/// Asserts the `io_uring` backend returns the oracle's exact outputs at
/// several queue depths.
///
/// Depth 1 is the common single-in-flight baseline; the deeper queue
/// exercises real kernel concurrency and out-of-order completion under
/// the same hard byte budget.
#[cfg(target_os = "linux")]
#[expect(
    clippy::expect_used,
    reason = "test helpers panic with diagnostics like the tests they serve"
)]
fn assert_uring_agrees(
    data: &[u8],
    schedule: &[(u64, u64)],
    read_size: u64,
    byte_budget: u64,
    oracle: &[RangeOutput],
) {
    let plan = ReadPlan::try_from_schedule(&ranges(schedule)).expect("test schedules are valid");
    let fixture = Fixture::new(data);
    let file = fs::File::open(&fixture.path).expect("fixture file opens");

    for operations in [1, 3] {
        let execution = ExecutionPlan::try_from_read_plan(&plan, config(read_size, byte_budget))
            .expect("valid plans and configurations derive a physical plan");
        let depth = UringQueueDepth::try_new(operations).expect("test depths are non-zero");

        let outputs = execute_uring(&file, execution, depth)
            .expect("the io_uring backend reads in-bounds test ranges");

        assert_eq!(
            outputs, oracle,
            "io_uring outputs match the oracle at queue depth {operations}"
        );
    }
}

/// Asserts both backends reject a plan crossing EOF with a typed EOF error.
///
/// Both sides must report the end-of-file condition itself — not merely any
/// failure — so an executor that surfaced EOF as a scheduling, assembly, or
/// stall error would be caught. The two error payloads are not compared:
/// the oracle fails on a whole logical range while the executor fails on
/// one physical read inside it.
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

    let oracle_error = read_plan(&file, &plan).expect_err("the oracle rejects a plan crossing EOF");
    assert!(
        matches!(oracle_error, ReadError::UnexpectedEof { .. }),
        "the oracle reports the EOF itself, not another failure: {oracle_error:?}"
    );

    let executor_error =
        execute_pread(&file, execution).expect_err("the executor rejects a plan crossing EOF");
    assert!(
        matches!(
            executor_error,
            PreadExecutionError::Read(ReadError::UnexpectedEof { .. })
        ),
        "the executor reports the EOF through its read stage, not another failure: \
         {executor_error:?}"
    );

    // The io_uring backend never retries a short kernel result, so an
    // EOF-crossing plan surfaces as either a zero-byte EOF or a short
    // read on one physical operation — both are the end-of-file
    // condition itself, never another failure and never partial output.
    #[cfg(target_os = "linux")]
    {
        let execution = ExecutionPlan::try_from_read_plan(&plan, config(read_size, byte_budget))
            .expect("valid plans and configurations derive a physical plan");
        let depth = UringQueueDepth::try_new(2).expect("test depths are non-zero");

        let uring_error = execute_uring(&file, execution, depth)
            .expect_err("the io_uring backend rejects a plan crossing EOF");

        // With a queue depth above one, several physical reads of the
        // crossing range can fail concurrently: the first failure stays
        // primary while later ones surface as nested drainage layers, so
        // the end-of-file condition is asserted on the innermost primary.
        let mut primary = &uring_error;
        while let UringExecutionError::DrainageFailed { primary: inner, .. } = primary {
            primary = inner;
        }
        assert!(
            matches!(
                primary,
                UringExecutionError::UnexpectedEof { .. } | UringExecutionError::ShortRead { .. }
            ),
            "the io_uring backend reports the end-of-file condition itself: {uring_error:?}"
        );
    }
}

#[test]
fn backends_agree_on_the_hand_calculated_fixture() {
    let (_plan, oracle, executed) =
        run_both_backends(b"0123456789abcdef", &[(10, 4), (2, 3)], 4, 10);

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
fn backends_agree_when_a_range_splits_into_full_reads_and_a_tail() {
    assert_backends_agree(b"0123456789abcdef", &[(0, 10)], 4, 6);
}

#[test]
fn backends_agree_on_failure_for_the_hand_calculated_eof_schedule() {
    assert_backends_agree_on_eof(b"0123456789abcdef", &[(0, 4), (100, 4)], 4, 10);
}

#[test]
fn backends_agree_on_failure_for_an_empty_file() {
    assert_backends_agree_on_eof(&[], &[(0, 1)], 4, 10);
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
