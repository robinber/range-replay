//! Shared deterministic fixtures for the crate's unit tests.
//!
//! Every helper panics with a diagnostic message on invalid fixture input,
//! which is intentional for test code: a broken fixture is a test bug, not
//! a runtime condition.

use std::fs::File;
use std::path::PathBuf;
use std::{env, fs, process};

use crate::budget::ByteBudget;
use crate::execution::{ExecutionConfig, ExecutionPlan, ReadSize};
use crate::plan::ReadPlan;
use crate::range::ReadRange;
use crate::scheduler::{ScheduleDecision, ScheduledRead, Scheduler};

/// The sixteen-byte backend fixture: the byte at each offset spells that
/// offset's hexadecimal digit.
pub(crate) const HEX_FIXTURE: &[u8] = b"0123456789abcdef";

/// The fourteen-byte source of the hand-calculated B, D, A, C scheduling
/// fixture: `[0, 14)` at read size 4 under a 10-byte budget splits into
/// A `[0, 4)`, B `[4, 8)`, C `[8, 12)`, and the tail D `[12, 14)`.
pub(crate) const BDAC_FIXTURE: &[u8] = b"abcdefghijklmn";

/// Returns the validated range `[start, end)`.
///
/// End-exclusive convention: the second argument is the exclusive end
/// offset, never a length. [`range`] is the length-based twin.
pub(crate) fn span(start: u64, end: u64) -> ReadRange {
    ReadRange::try_new(start, end - start).expect("test spans are valid ranges")
}

/// Returns the validated range covering `length` bytes from `offset`.
///
/// Length-based convention: the second argument is a byte count, never an
/// end offset. [`span`] is the end-exclusive twin.
pub(crate) fn range(offset: u64, length: u64) -> ReadRange {
    ReadRange::try_new(offset, length).expect("test ranges are valid")
}

/// Coalesces `schedule` into its canonical logical plan.
pub(crate) fn plan(schedule: &[ReadRange]) -> ReadPlan {
    ReadPlan::try_from_schedule(schedule).expect("test schedules are not empty")
}

/// Derives the physical plan of `schedule` under one read size and one
/// in-flight byte budget.
pub(crate) fn execution(
    schedule: &[ReadRange],
    read_size_bytes: u64,
    budget_bytes: u64,
) -> ExecutionPlan {
    let read_size =
        ReadSize::try_new(read_size_bytes).expect("test read sizes are within the valid domain");
    let budget = ByteBudget::try_new(budget_bytes).expect("test budgets are non-zero");
    let config = ExecutionConfig::try_new(read_size, budget)
        .expect("test configurations pair a read size with a large enough budget");

    ExecutionPlan::try_from_read_plan(&plan(schedule), config)
        .expect("test plans derive without failure")
}

/// Builds a scheduler owning `execution` with every operation pending.
pub(crate) fn scheduler_for(execution: ExecutionPlan) -> Scheduler {
    Scheduler::try_new(execution).expect("test schedulers construct without failure")
}

/// Unwraps the next scheduling decision into its admitted read.
pub(crate) fn ready(scheduler: &mut Scheduler) -> ScheduledRead {
    match scheduler
        .schedule_next()
        .expect("test scheduling decisions succeed")
    {
        ScheduleDecision::Ready(read) => read,
        decision => panic!("expected a ready decision, got {decision:?}"),
    }
}

/// Asserts that the next scheduling decision is temporary backpressure.
pub(crate) fn assert_waiting(scheduler: &mut Scheduler) {
    assert!(matches!(
        scheduler
            .schedule_next()
            .expect("test scheduling decisions succeed"),
        ScheduleDecision::WaitingForBudget
    ));
}

/// Asserts that the next scheduling decision is exhaustion.
pub(crate) fn assert_exhausted(scheduler: &mut Scheduler) {
    assert!(matches!(
        scheduler
            .schedule_next()
            .expect("test scheduling decisions succeed"),
        ScheduleDecision::Exhausted
    ));
}

/// Builds a one-operation scheduler over `[offset, offset + length)` and
/// admits that single operation.
///
/// The read size is fixed to `length`, so the derived plan always holds
/// exactly one physical operation.
pub(crate) fn admitted_single(
    offset: u64,
    length: u64,
    budget_bytes: u64,
) -> (Scheduler, ScheduledRead) {
    let mut scheduler = scheduler_for(execution(&[range(offset, length)], length, budget_bytes));
    let admitted = ready(&mut scheduler);

    (scheduler, admitted)
}

/// Runs `run` against an open read-only file containing exactly `contents`.
///
/// The file lives under the system temporary directory with a name derived
/// from `test`, the process id, and a process-wide atomic counter, so every
/// call gets a unique path even across parallel tests; the `test` name only
/// aids debugging when a leftover file must be traced back to its test. The
/// file is removed after `run` returns.
pub(crate) fn with_file_content<T>(
    test: &str,
    contents: &[u8],
    run: impl FnOnce(&mut File) -> T,
) -> T {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path: PathBuf =
        env::temp_dir().join(format!("range-replay-{test}-{}-{unique}", process::id()));
    fs::write(&path, contents).expect("fixture file is writable");
    let mut file = File::open(&path).expect("fixture file opens");

    let result = run(&mut file);

    drop(file);
    fs::remove_file(&path).expect("fixture file is removable");

    result
}
