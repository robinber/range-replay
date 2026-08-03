use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::rc::Rc;

use super::{
    BackendSession, DriverFailure, PreadExecutionError, execute_pread, execute_with_session,
};
use crate::budget::ByteBudget;
use crate::completion::CompletedRead;
use crate::execution::{ExecutionConfig, ExecutionPlan, ReadSize};
use crate::output::{AssemblyError, RangeOutput};
use crate::plan::ReadPlan;
use crate::pread::{ReadError, read_plan};
use crate::range::ReadRange;
use crate::scheduler::{ScheduleDecision, ScheduledRead, Scheduler};
use crate::test_support::with_file_content;

/// The hand-calculated fixture source: `[0, 14)` at read size 4 under a
/// 10-byte budget splits into A `[0, 4)`, B `[4, 8)`, C `[8, 12)`, and the
/// tail D `[12, 14)`.
const FIXTURE: &[u8] = b"abcdefghijklmn";

/// One operation identity as the plain index pair of its [`OperationId`],
/// because tests cannot construct the real identity type.
type Op = (usize, u64);

const A: Op = (0, 0);
const B: Op = (0, 1);
const C: Op = (0, 2);
const D: Op = (0, 3);

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

fn bdac_plan() -> ExecutionPlan {
    execution(&[span(0, 14)], 4, 10)
}

/// Externally observable behavior of one [`FakeSession`] run.
///
/// The driver consumes the session by value, so the test keeps its own
/// [`Rc`] handle on this state to assert events and emptiness afterwards.
#[derive(Debug, Default)]
struct FakeState {
    active: Vec<ScheduledRead>,
    events: Vec<Event>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Submitted(Op),
    SubmitRejected(Op),
    Completed(Op),
    CompletionFailed(Op),
    ForeignReturned,
    Drained,
}

#[derive(Debug, PartialEq, Eq)]
enum FakeFailure {
    Submit(Op),
    Wait(Op),
    Drain,
}

/// One scripted outcome for one `wait_for_completion` call, in call order.
enum WaitStep {
    /// Complete the named retained operation with its exact fixture bytes.
    Complete(Op),
    /// Fail the named retained operation terminally, destroying it.
    Fail(Op),
    /// Return the pre-built completion of an unrelated run.
    Foreign,
}

/// Deterministic scripted backend session that performs no file I/O.
///
/// It retains the real non-`Clone` [`ScheduledRead`] values it accepts and
/// converts them into valid [`CompletedRead`] values through the
/// crate-private construction path, so every reservation lifetime is the
/// real one. It proves driver decisions only, never that `pread` or
/// operating-system I/O works.
struct FakeSession {
    state: Rc<RefCell<FakeState>>,
    wait_steps: VecDeque<WaitStep>,
    submit_failure: Option<Op>,
    foreign: Option<CompletedRead>,
    report_no_active: bool,
    fail_drain: bool,
}

fn fake_session(wait_steps: Vec<WaitStep>) -> (FakeSession, Rc<RefCell<FakeState>>) {
    let state = Rc::new(RefCell::new(FakeState::default()));
    let session = FakeSession {
        state: Rc::clone(&state),
        wait_steps: wait_steps.into(),
        submit_failure: None,
        foreign: None,
        report_no_active: false,
        fail_drain: false,
    };

    (session, state)
}

fn op_of(scheduled: &ScheduledRead) -> Op {
    (
        scheduled.id().logical_range_index(),
        scheduled.id().operation_index(),
    )
}

fn complete_from_fixture(scheduled: ScheduledRead) -> CompletedRead {
    let range = scheduled.range();
    let start = usize::try_from(range.offset()).expect("fixture offsets fit in usize");
    let end = usize::try_from(range.end()).expect("fixture ends fit in usize");

    CompletedRead::try_new(FIXTURE[start..end].to_vec(), scheduled)
        .expect("fixture bytes cover the admitted range exactly")
}

fn take_active(state: &mut FakeState, op: Op) -> ScheduledRead {
    let index = state
        .active
        .iter()
        .position(|scheduled| op_of(scheduled) == op)
        .expect("scripted operations were submitted before their outcome");

    state.active.remove(index)
}

impl BackendSession for FakeSession {
    type Error = FakeFailure;

    fn submit(&mut self, scheduled: ScheduledRead) -> Result<(), FakeFailure> {
        let op = op_of(&scheduled);
        let mut state = self.state.borrow_mut();

        if self.submit_failure == Some(op) {
            state.events.push(Event::SubmitRejected(op));
            // Returning destroys the scheduled read, releasing its
            // reservation, exactly like a real failed submission.
            return Err(FakeFailure::Submit(op));
        }

        state.events.push(Event::Submitted(op));
        state.active.push(scheduled);

        Ok(())
    }

    fn wait_for_completion(&mut self) -> Result<CompletedRead, FakeFailure> {
        let mut state = self.state.borrow_mut();

        match self.wait_steps.pop_front() {
            Some(WaitStep::Complete(op)) => {
                let scheduled = take_active(&mut state, op);
                state.events.push(Event::Completed(op));

                Ok(complete_from_fixture(scheduled))
            }
            Some(WaitStep::Fail(op)) => {
                // A terminal failure destroys the operation's resources
                // before returning, releasing its reservation.
                drop(take_active(&mut state, op));
                state.events.push(Event::CompletionFailed(op));

                Err(FakeFailure::Wait(op))
            }
            Some(WaitStep::Foreign) => {
                state.events.push(Event::ForeignReturned);

                Ok(self
                    .foreign
                    .take()
                    .expect("a foreign completion was scripted"))
            }
            None => {
                // Unscripted waits — the driver draining after a failure —
                // complete the earliest retained operation.
                assert!(
                    !state.active.is_empty(),
                    "the driver only waits while the session reports active work"
                );
                let scheduled = state.active.remove(0);
                let op = op_of(&scheduled);
                state.events.push(Event::Completed(op));

                Ok(complete_from_fixture(scheduled))
            }
        }
    }

    fn has_active(&self) -> bool {
        if self.report_no_active {
            return false;
        }

        !self.state.borrow().active.is_empty()
    }

    fn drain(&mut self) -> Result<(), FakeFailure> {
        let mut state = self.state.borrow_mut();
        state.events.push(Event::Drained);

        if self.fail_drain {
            return Err(FakeFailure::Drain);
        }

        state.active.clear();

        Ok(())
    }
}

/// A valid completion of an unrelated run over a different plan, used to
/// force an assembly failure inside the driver.
fn foreign_completion() -> CompletedRead {
    let mut scheduler = Scheduler::try_new(execution(&[span(20, 24)], 4, 4))
        .expect("test schedulers construct without failure");

    match scheduler
        .schedule_next()
        .expect("test scheduling decisions succeed")
    {
        ScheduleDecision::Ready(admitted) => CompletedRead::try_new(b"wxyz".to_vec(), admitted)
            .expect("four bytes cover the four-byte foreign range"),
        decision => panic!("expected a ready decision, got {decision:?}"),
    }
}

type FakeResult = Result<Vec<RangeOutput>, DriverFailure<FakeFailure>>;

fn run_bdac() -> (FakeResult, Vec<Event>, bool) {
    let (session, state) = fake_session(vec![
        WaitStep::Complete(B),
        WaitStep::Complete(D),
        WaitStep::Complete(A),
        WaitStep::Complete(C),
    ]);

    let result = execute_with_session(bdac_plan(), session);
    let events = state.borrow().events.clone();
    let empty = state.borrow().active.is_empty();

    (result, events, empty)
}

#[test]
fn the_greedy_prefix_submits_a_b_and_the_fitting_tail_before_waiting() {
    let (result, events, _) = run_bdac();

    assert!(result.is_ok());
    assert_eq!(
        events[..4],
        [
            Event::Submitted(A),
            Event::Submitted(B),
            Event::Submitted(D),
            Event::Completed(B),
        ]
    );
}

#[test]
fn the_bdac_completion_order_produces_the_exact_logical_bytes() {
    let (result, events, _) = run_bdac();

    assert_eq!(
        events,
        vec![
            Event::Submitted(A),
            Event::Submitted(B),
            Event::Submitted(D),
            Event::Completed(B),
            Event::Submitted(C),
            Event::Completed(D),
            Event::Completed(A),
            Event::Completed(C),
        ]
    );

    let outputs = result.expect("the scripted run succeeds");
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].range(), span(0, 14));
    assert_eq!(outputs[0].bytes(), FIXTURE);
}

#[test]
fn c_is_submitted_only_after_recording_b_released_its_bytes() {
    let (result, events, _) = run_bdac();

    assert!(result.is_ok());

    // Under the 10-byte hard budget C can only be admitted after B's four
    // bytes were released, which happens when the driver records B. The
    // one wait between D and C — never a second one — proves the refill
    // came from consuming exactly that completion.
    let waited = events
        .iter()
        .position(|event| *event == Event::Completed(B))
        .expect("B completes");
    let submitted_c = events
        .iter()
        .position(|event| *event == Event::Submitted(C))
        .expect("C is submitted");
    assert_eq!(submitted_c, waited + 1);
}

#[test]
fn scheduler_exhaustion_does_not_finish_while_completions_remain_active() {
    let (result, events, _) = run_bdac();

    // After C's submission the scheduler is exhausted, yet D, A, and C are
    // still active: the run must integrate all three before succeeding
    // with every logical byte present.
    let submitted_c = events
        .iter()
        .position(|event| *event == Event::Submitted(C))
        .expect("C is submitted");
    assert_eq!(
        events[submitted_c + 1..],
        [
            Event::Completed(D),
            Event::Completed(A),
            Event::Completed(C),
        ]
    );

    let outputs = result.expect("the scripted run succeeds");
    assert_eq!(outputs[0].bytes(), FIXTURE);
}

#[test]
fn a_successful_run_finishes_only_after_the_session_is_idle() {
    let (result, _, empty) = run_bdac();

    assert!(result.is_ok());
    assert!(empty, "no operation may stay active after success");
}

#[test]
fn a_submit_failure_stops_later_submissions_drains_and_exposes_no_output() {
    let (mut session, state) = fake_session(Vec::new());
    session.submit_failure = Some(D);

    let result = execute_with_session(bdac_plan(), session);

    assert!(matches!(
        result,
        Err(DriverFailure::Backend(FakeFailure::Submit(op))) if op == D
    ));

    let state = state.borrow();
    assert_eq!(
        state.events,
        vec![
            Event::Submitted(A),
            Event::Submitted(B),
            Event::SubmitRejected(D),
            Event::Completed(A),
            Event::Completed(B),
            Event::Drained,
        ]
    );
    assert!(state.active.is_empty(), "accepted work was drained");
}

#[test]
fn a_completion_failure_with_other_work_active_drains_the_rest() {
    let (session, state) = fake_session(vec![WaitStep::Fail(B)]);

    let result = execute_with_session(bdac_plan(), session);

    assert!(matches!(
        result,
        Err(DriverFailure::Backend(FakeFailure::Wait(op))) if op == B
    ));

    let state = state.borrow();
    assert!(
        !state.events.contains(&Event::Submitted(C)),
        "no submission may follow the failure"
    );
    assert_eq!(
        state.events,
        vec![
            Event::Submitted(A),
            Event::Submitted(B),
            Event::Submitted(D),
            Event::CompletionFailed(B),
            Event::Completed(A),
            Event::Completed(D),
            Event::Drained,
        ]
    );
    assert!(state.active.is_empty(), "remaining work was drained");
}

#[test]
fn an_assembly_failure_with_backend_work_active_follows_the_same_drainage() {
    let (mut session, state) = fake_session(vec![WaitStep::Foreign]);
    session.foreign = Some(foreign_completion());

    let result = execute_with_session(bdac_plan(), session);

    match result {
        Err(DriverFailure::Assembly(AssemblyError::RangeMismatch {
            id,
            expected,
            actual,
        })) => {
            assert_eq!(id.logical_range_index(), 0);
            assert_eq!(id.operation_index(), 0);
            assert_eq!(expected, span(0, 4));
            assert_eq!(actual, span(20, 24));
        }
        other => panic!("expected an assembly range mismatch, got {other:?}"),
    }

    let state = state.borrow();
    assert_eq!(
        state.events,
        vec![
            Event::Submitted(A),
            Event::Submitted(B),
            Event::Submitted(D),
            Event::ForeignReturned,
            Event::Completed(A),
            Event::Completed(B),
            Event::Completed(D),
            Event::Drained,
        ]
    );
    assert!(state.active.is_empty(), "active work was drained");
}

#[test]
fn drainage_completions_are_destroyed_without_being_recorded() {
    let (session, state) = fake_session(vec![WaitStep::Fail(B)]);

    let result = execute_with_session(bdac_plan(), session);

    // The run failed before A and D completed, so their drainage
    // completions can only have been destroyed: no output is observable,
    // and nothing retains them after the session drained.
    assert!(result.is_err(), "no output is exposed after a failure");
    let state = state.borrow();
    assert!(state.events.contains(&Event::Completed(A)));
    assert!(state.events.contains(&Event::Completed(D)));
    assert!(state.active.is_empty());
}

#[test]
fn waiting_with_no_active_work_is_a_typed_error_instead_of_a_hang() {
    let (mut session, state) = fake_session(Vec::new());
    session.report_no_active = true;

    let result = execute_with_session(bdac_plan(), session);

    assert!(matches!(
        result,
        Err(DriverFailure::StalledWithoutActiveWork)
    ));

    let state = state.borrow();
    assert_eq!(
        state.events,
        vec![
            Event::Submitted(A),
            Event::Submitted(B),
            Event::Submitted(D),
            Event::Drained,
        ]
    );
    assert!(state.active.is_empty(), "drain released the retained work");
}

#[test]
fn a_drainage_wait_failure_is_preserved_alongside_the_primary_failure() {
    let (mut session, state) = fake_session(vec![WaitStep::Fail(A)]);
    session.submit_failure = Some(D);

    let result = execute_with_session(bdac_plan(), session);

    match result {
        Err(DriverFailure::Drainage { primary, drainage }) => {
            assert!(matches!(
                *primary,
                DriverFailure::Backend(FakeFailure::Submit(op)) if op == D
            ));
            assert_eq!(drainage, FakeFailure::Wait(A));
        }
        other => panic!("expected a drainage failure, got {other:?}"),
    }

    let state = state.borrow();
    assert_eq!(
        *state.events.last().expect("events were recorded"),
        Event::Drained
    );
    assert!(state.active.is_empty(), "the final drain released B");
}

#[test]
fn a_failing_drain_is_preserved_alongside_the_primary_failure() {
    let (mut session, state) = fake_session(Vec::new());
    session.submit_failure = Some(D);
    session.fail_drain = true;

    let result = execute_with_session(bdac_plan(), session);

    match result {
        Err(DriverFailure::Drainage { primary, drainage }) => {
            assert!(matches!(
                *primary,
                DriverFailure::Backend(FakeFailure::Submit(op)) if op == D
            ));
            assert_eq!(drainage, FakeFailure::Drain);
        }
        other => panic!("expected a drainage failure, got {other:?}"),
    }

    // The completion pulls already emptied the session before the drain
    // reported its failure.
    assert!(state.borrow().active.is_empty());
}

const REAL: &[u8] = b"0123456789abcdef";

#[test]
fn execute_pread_returns_exact_outputs_for_canonical_ranges() {
    with_file_content("executor-success", REAL, |file| {
        let single = execute_pread(file, execution(&[span(2, 5)], 4, 4))
            .expect("the range is inside the fixture");
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].range(), span(2, 5));
        assert_eq!(single[0].bytes(), b"234");

        let several = execute_pread(file, execution(&[span(2, 5), span(10, 14)], 4, 8))
            .expect("both ranges are inside the fixture");
        assert_eq!(several.len(), 2);
        assert_eq!(several[0].range(), span(2, 5));
        assert_eq!(several[0].bytes(), b"234");
        assert_eq!(several[1].range(), span(10, 14));
        assert_eq!(several[1].bytes(), b"abcd");
    });
}

#[test]
fn the_bdac_fixture_executes_through_the_real_session_under_backpressure() {
    with_file_content("executor-bdac", FIXTURE, |file| {
        let outputs =
            execute_pread(file, bdac_plan()).expect("the whole fixture range is readable");

        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].range(), span(0, 14));
        assert_eq!(outputs[0].bytes(), FIXTURE);
    });
}

#[test]
fn execute_pread_matches_the_read_plan_reference() {
    with_file_content("executor-parity", REAL, |file| {
        let plan = ReadPlan::try_from_schedule(&[span(10, 14), span(2, 5)])
            .expect("test schedules are not empty");
        let reference = read_plan(file, &plan).expect("the reference backend succeeds");

        let read_size = ReadSize::try_new(4).expect("test read sizes are non-zero");
        let budget = ByteBudget::try_new(8).expect("test budgets are non-zero");
        let config = ExecutionConfig::try_new(read_size, budget)
            .expect("test configurations pair a read size with a large enough budget");
        let execution = ExecutionPlan::try_from_read_plan(&plan, config)
            .expect("test plans derive without failure");

        let executed = execute_pread(file, execution).expect("the executor succeeds");

        assert_eq!(executed, reference);
    });
}

#[test]
fn a_range_past_eof_is_a_typed_error_with_no_partial_output() {
    with_file_content("executor-eof", b"abc", |file| {
        let error = execute_pread(file, execution(&[span(1, 5)], 4, 4))
            .expect_err("the file ends inside the range");

        assert!(matches!(
            error,
            PreadExecutionError::Read(ReadError::UnexpectedEof {
                range,
                expected: 4,
                actual: 2,
            }) if range == span(1, 5)
        ));
    });
}

#[test]
fn a_later_operation_failure_exposes_no_earlier_output() {
    with_file_content("executor-later-eof", b"abcdefgh", |file| {
        // [0, 4) reads completely; [6, 10) ends after two bytes. The typed
        // error is the only observable result.
        let error = execute_pread(file, execution(&[span(0, 4), span(6, 10)], 4, 8))
            .expect_err("the second range passes EOF");

        assert!(matches!(
            error,
            PreadExecutionError::Read(ReadError::UnexpectedEof {
                range,
                expected: 4,
                actual: 2,
            }) if range == span(6, 10)
        ));
    });
}

#[test]
fn the_borrowed_file_stays_usable_with_an_unchanged_cursor() {
    with_file_content("executor-cursor", REAL, |file| {
        file.seek(SeekFrom::Start(7)).expect("fixture file seeks");

        let outputs = execute_pread(file, execution(&[span(2, 5)], 4, 4))
            .expect("the range is inside the fixture");
        assert_eq!(outputs[0].bytes(), b"234");

        let cursor = file
            .stream_position()
            .expect("fixture file reports its cursor");
        assert_eq!(cursor, 7);

        let mut rest = [0_u8; 9];
        file.read_exact(&mut rest)
            .expect("the file stays readable after the call");
        assert_eq!(&rest, b"789abcdef");
    });
}

#[test]
fn outputs_outlive_the_consumed_plan_and_the_borrowed_file() {
    let outputs = with_file_content("executor-consumed", REAL, |file| {
        execute_pread(file, execution(&[span(0, 4)], 4, 4))
            .expect("the range is inside the fixture")
    });

    // The plan was consumed and the file is closed and removed; the
    // outputs stay valid on their own.
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].bytes(), b"0123");
}
