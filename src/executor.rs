//! Fail-closed synchronous execution of one physical plan against one open
//! file.
//!
//! [`execute_pread`] is the first execution boundary that owns the whole
//! lifecycle the lower primitives left to "a future executor": it consumes
//! one [`ExecutionPlan`], borrows one [`File`](std::fs::File), and drives
//! scheduling, submission, completion, and assembly to *global* success,
//! returning plan-order [`RangeOutput`] values only when the scheduler is
//! exhausted, no backend operation remains active, and every logical byte
//! was assembled. Everything in between — the backend session, the generic
//! driver, and their failure plumbing — stays private, so callers can
//! neither inject foreign completions nor mix two independent runs.
//!
//! Internally a small dependency-inversion seam exists: the driver is
//! generic over a private [`BackendSession`] trait, and the only production
//! implementation is a synchronous [`PreadSession`] over the existing
//! [`read_scheduled`] exact-read adapter. The trait is a testing and
//! evolution seam, not a public extension point; a later `io_uring` session
//! must earn its own contract before anything here becomes public.
//!
//! The call is deliberately blocking. Kernel-side concurrency — several
//! admitted reads in flight under the byte budget — needs no Rust async
//! API, threads, or channels: the budget bounds admitted bytes, the
//! session decides how submissions progress, and the synchronous session
//! simply completes each read at submission time.
//!
//! Failure handling is fail-closed drainage. After the first fatal error,
//! no new work is scheduled or submitted, the session is drained until no
//! operation can still access its owned resources, successful completions
//! observed during drainage are destroyed without being recorded, every
//! reservation releases through RAII, the incomplete assembler is
//! destroyed, and the primary typed failure is returned with no partial
//! output. A drainage failure never erases the primary failure.

use std::collections::{TryReserveError, VecDeque};
use std::fs::File;

use thiserror::Error;

use crate::completion::CompletedRead;
use crate::execution::ExecutionPlan;
use crate::output::{AssemblyError, OutputAssembler, RangeOutput};
use crate::pread::{ReadError, read_scheduled};
use crate::scheduler::{ScheduleDecision, ScheduledRead, Scheduler, SchedulerError};

#[cfg(test)]
mod tests;

/// Reason one [`execute_pread`] run failed globally.
///
/// Every variant is terminal for its run: after any of them, no new work
/// was scheduled or submitted, the internal session was drained, every
/// reservation was released, and no partial output is observable. The
/// variants preserve the typed source of the stage that failed instead of
/// erasing it behind a string.
#[derive(Debug, Error)]
pub enum PreadExecutionError {
    /// Preparing logical outputs, recording one completion, or finishing
    /// assembly failed.
    #[error("assembling logical outputs failed")]
    Assembly(#[source] AssemblyError),
    /// Constructing the scheduler or making one scheduling decision failed
    /// permanently.
    ///
    /// Temporary budget backpressure is never an error; the driver waits on
    /// active work instead.
    #[error("scheduling physical reads failed")]
    Scheduling(#[source] SchedulerError),
    /// Submitting or exactly completing one positioned read failed.
    ///
    /// The source is the same typed [`ReadError`] the one-operation
    /// [`read_scheduled`] adapter reports: EOF inside an admitted range,
    /// a partial read the file could not complete, buffer allocation, and
    /// I/O failures all arrive here unchanged.
    #[error("executing one positioned read failed")]
    Read(#[source] ReadError),
    /// The session could not retain one more completed read.
    ///
    /// The synchronous session reserves its retention slot fallibly before
    /// performing each read, so an allocator failure surfaces as this typed
    /// variant before any byte moves instead of aborting mid-run.
    #[error("cannot retain one more completed read in the pread session")]
    CompletionRetention {
        /// Allocator failure reported by the reservation.
        #[source]
        source: TryReserveError,
    },
    /// No progress was possible because no backend operation was active.
    ///
    /// The driver reports this when the scheduler asks it to wait for
    /// budget while nothing is active — pending work, nothing admitted,
    /// nothing to wait for — and the internal session reports the same
    /// impossibility when a completion is requested from an idle session.
    /// Both states are unreachable through a correct session — every
    /// admitted reservation belongs to an active operation or a retained
    /// completion — so the driver returns this typed error instead of
    /// spinning, sleeping, or misreporting exhaustion.
    #[error("no progress is possible: no backend operation is active")]
    StalledWithoutActiveWork,
    /// Draining the session after a primary failure reported an additional
    /// failure.
    ///
    /// The primary failure stays the source of this variant, so the cause
    /// chain always surfaces what originally failed; the drainage failure
    /// is preserved alongside it instead of being discarded. The drainage
    /// failure is a sibling field outside the
    /// [`source`](std::error::Error::source) chain — it appears in the
    /// display text, and programmatic inspection must match this variant
    /// to observe both failures. Several cleanup failures nest: when the
    /// drainage pull and the final drain both fail, the primary of the
    /// outer variant is itself a [`Self::DrainageFailed`], and the
    /// innermost end of the chain is always the failure that originally
    /// ended the run.
    #[error("draining the pread session after a failure reported another failure: {drainage}")]
    DrainageFailed {
        /// The failure that ended the run before drainage began.
        #[source]
        primary: Box<PreadExecutionError>,
        /// The additional failure drainage itself reported.
        drainage: Box<PreadExecutionError>,
    },
}

/// Internal contract one backend session offers the generic driver for one
/// run.
///
/// The trait is a private dependency-inversion and testing seam, not a
/// public extension point: implementations own submitted work exclusively,
/// are never `Clone`, and live exactly as long as one driver run. A
/// successful [`Self::submit`] transfers unique ownership of the admitted
/// [`ScheduledRead`] into the session, which keeps its reservation live
/// until the operation reaches exactly one terminal transition: a
/// successful exact completion returned by [`Self::wait_for_completion`], a
/// typed terminal failure that destroys the operation's owned resources, or
/// destruction during [`Self::drain`].
trait BackendSession {
    /// Typed terminal failure of one session operation.
    type Error;

    /// Takes unique ownership of one admitted read and submits it.
    ///
    /// On failure the scheduled read and every resource created for it are
    /// destroyed before returning, releasing its reservation; the operation
    /// is never retried or requeued.
    fn submit(&mut self, scheduled: ScheduledRead) -> Result<(), Self::Error>;

    /// Blocks until one submitted operation reaches a terminal outcome.
    ///
    /// A returned completion still owns its reservation. A returned error
    /// is terminal for exactly one previously active operation, whose owned
    /// resources were destroyed before returning — every outcome therefore
    /// strictly shrinks the active set, which is what lets the driver's
    /// drainage pull terminate. Callers must only call this while
    /// [`Self::has_active`] reports active work; a session with nothing
    /// active reports a typed error instead of blocking forever.
    fn wait_for_completion(&mut self) -> Result<CompletedRead, Self::Error>;

    /// Returns whether any submitted operation has not yet been handed
    /// back through [`Self::wait_for_completion`].
    fn has_active(&self) -> bool;

    /// Establishes an idle safe state, destroying every still-active
    /// operation and all resources it could access.
    ///
    /// An error means the session could not prove that idle state; it must
    /// never claim successful drainage otherwise.
    fn drain(&mut self) -> Result<(), Self::Error>;
}

/// Terminal failure of one generic driver run, before public mapping.
#[derive(Debug)]
enum DriverFailure<E> {
    /// Preparing, recording, or finishing logical outputs failed.
    Assembly(AssemblyError),
    /// Scheduler construction or one scheduling decision failed.
    Scheduling(SchedulerError),
    /// One session operation failed terminally.
    Backend(E),
    /// Budget backpressure was reported while nothing was active.
    StalledWithoutActiveWork,
    /// Drainage after the primary failure reported an additional failure.
    Drainage {
        /// The failure that ended the run before drainage began.
        primary: Box<DriverFailure<E>>,
        /// The additional session failure drainage reported.
        drainage: E,
    },
}

/// Drives one owned session over one consumed plan to global success.
///
/// The assembler is prepared by borrowing the plan before any submission,
/// so every logical destination exists before backend work starts; the plan
/// then moves into the scheduler without cloning. A preparation failure
/// returns immediately because no operation is active yet. Any later
/// failure passes through [`drain_after_failure`] so the session reaches an
/// idle safe state before the error becomes observable.
fn execute_with_session<S>(
    plan: ExecutionPlan,
    mut session: S,
) -> Result<Vec<RangeOutput>, DriverFailure<S::Error>>
where
    S: BackendSession,
{
    let assembler = OutputAssembler::try_new(&plan).map_err(DriverFailure::Assembly)?;
    let scheduler = Scheduler::try_new(plan).map_err(DriverFailure::Scheduling)?;

    match drive(&mut session, scheduler, assembler) {
        Ok(outputs) => Ok(outputs),
        Err(primary) => Err(drain_after_failure(&mut session, primary)),
    }
}

/// Runs the normal scheduling, submission, completion, and assembly loop.
///
/// Returning `Err` leaves the session to the caller's drainage; the
/// incomplete assembler and the scheduler are destroyed here, which is the
/// fail-closed destruction of every private buffer they still own.
fn drive<S>(
    session: &mut S,
    mut scheduler: Scheduler,
    mut assembler: OutputAssembler,
) -> Result<Vec<RangeOutput>, DriverFailure<S::Error>>
where
    S: BackendSession,
{
    loop {
        match scheduler
            .schedule_next()
            .map_err(DriverFailure::Scheduling)?
        {
            ScheduleDecision::Ready(admitted) => {
                session.submit(admitted).map_err(DriverFailure::Backend)?;
            }
            ScheduleDecision::WaitingForBudget => {
                if !session.has_active() {
                    return Err(DriverFailure::StalledWithoutActiveWork);
                }

                // Recording consumes the completion, destroying its
                // physical buffer and releasing its reservation, so the
                // budget is refilled before the scheduler is asked for
                // replacement work.
                let completed = session
                    .wait_for_completion()
                    .map_err(DriverFailure::Backend)?;
                assembler
                    .record(completed)
                    .map_err(DriverFailure::Assembly)?;
            }
            ScheduleDecision::Exhausted => {
                // Exhaustion only ends distribution. Global success
                // additionally needs every active operation completed and
                // recorded, and the assembler finishing completely.
                while session.has_active() {
                    let completed = session
                        .wait_for_completion()
                        .map_err(DriverFailure::Backend)?;
                    assembler
                        .record(completed)
                        .map_err(DriverFailure::Assembly)?;
                }

                return assembler.finish().map_err(DriverFailure::Assembly);
            }
        }
    }
}

/// Drains the session after `primary` so no operation can still access its
/// owned resources, then returns the failure to expose.
///
/// The run is already globally failed, so successful completions pulled
/// here are destroyed without being recorded — destroying each one drops
/// its physical buffer and releases its reservation. Every
/// [`BackendSession::wait_for_completion`] outcome is terminal for exactly
/// one previously active operation, so the pull strictly shrinks the
/// active set until nothing remains; the session's own
/// [`BackendSession::drain`] then proves the idle safe state. No cleanup
/// failure is ever discarded: each additional failure — from the pull or
/// from the final drain — nests another [`DriverFailure::Drainage`] layer
/// whose innermost failure is always the original primary. A session that
/// still could not prove idleness is destroyed with the run — the facade
/// drops it by value before returning — so its `Drop` remains the
/// last-resort ownership guard while the returned error reports the failed
/// drainage.
fn drain_after_failure<S>(
    session: &mut S,
    primary: DriverFailure<S::Error>,
) -> DriverFailure<S::Error>
where
    S: BackendSession,
{
    let mut failure = primary;

    while session.has_active() {
        match session.wait_for_completion() {
            Ok(completed) => drop(completed),
            Err(additional) => {
                failure = DriverFailure::Drainage {
                    primary: Box::new(failure),
                    drainage: additional,
                };
            }
        }
    }

    if let Err(additional) = session.drain() {
        failure = DriverFailure::Drainage {
            primary: Box::new(failure),
            drainage: additional,
        };
    }

    failure
}

/// Typed terminal failure of one [`PreadSession`] operation.
#[derive(Debug)]
enum PreadSessionError {
    /// The one-operation exact read failed.
    Read(ReadError),
    /// The completion retention slot could not be reserved.
    CompletionRetention(TryReserveError),
    /// A completion was requested while nothing was active.
    NothingActive,
}

/// Synchronous backend session over one borrowed file.
///
/// The session is deliberately not `Clone` and borrows the file for the
/// whole run, so it can never outlive it. Submission performs the whole
/// positioned exact read immediately through [`read_scheduled`] — the
/// shared file cursor never moves — and retains the resulting
/// [`CompletedRead`] until the driver asks for it, so every retained
/// completion keeps its reservation counted exactly like a still-running
/// operation would. No work survives the session.
#[derive(Debug)]
struct PreadSession<'file> {
    file: &'file File,
    completed: VecDeque<CompletedRead>,
}

impl<'file> PreadSession<'file> {
    fn new(file: &'file File) -> Self {
        Self {
            file,
            completed: VecDeque::new(),
        }
    }
}

impl BackendSession for PreadSession<'_> {
    type Error = PreadSessionError;

    fn submit(&mut self, scheduled: ScheduledRead) -> Result<(), PreadSessionError> {
        // The retention slot is reserved before the read runs, so a
        // successfully read completion can never be lost to a failed queue
        // growth. Either failure path consumes and destroys the scheduled
        // read, releasing its reservation.
        self.completed
            .try_reserve(1)
            .map_err(PreadSessionError::CompletionRetention)?;

        let completed = read_scheduled(self.file, scheduled).map_err(PreadSessionError::Read)?;
        self.completed.push_back(completed);

        Ok(())
    }

    fn wait_for_completion(&mut self) -> Result<CompletedRead, PreadSessionError> {
        self.completed
            .pop_front()
            .ok_or(PreadSessionError::NothingActive)
    }

    fn has_active(&self) -> bool {
        !self.completed.is_empty()
    }

    fn drain(&mut self) -> Result<(), PreadSessionError> {
        // Destroying each retained completion drops its physical buffer
        // before its reservation releases; nothing else can still access
        // them, so the idle state is always establishable.
        self.completed.clear();

        Ok(())
    }
}

impl From<PreadSessionError> for PreadExecutionError {
    fn from(failure: PreadSessionError) -> Self {
        match failure {
            PreadSessionError::Read(source) => Self::Read(source),
            PreadSessionError::CompletionRetention(source) => Self::CompletionRetention { source },
            PreadSessionError::NothingActive => Self::StalledWithoutActiveWork,
        }
    }
}

impl From<DriverFailure<PreadSessionError>> for PreadExecutionError {
    fn from(failure: DriverFailure<PreadSessionError>) -> Self {
        match failure {
            DriverFailure::Assembly(source) => Self::Assembly(source),
            DriverFailure::Scheduling(source) => Self::Scheduling(source),
            DriverFailure::Backend(source) => source.into(),
            DriverFailure::StalledWithoutActiveWork => Self::StalledWithoutActiveWork,
            DriverFailure::Drainage { primary, drainage } => Self::DrainageFailed {
                primary: Box::new((*primary).into()),
                drainage: Box::new(drainage.into()),
            },
        }
    }
}

/// Executes one physical plan synchronously against one open file.
///
/// The plan is consumed by value — moving it performs no clone — while the
/// file is only borrowed and its shared cursor never moves, because every
/// read goes through the positioned `pread` path of [`read_scheduled`].
/// Every logical destination is prepared before the first backend
/// submission, then one private, uniquely owned session drives scheduling,
/// submission, completion, and assembly to global success. Because the
/// scheduler, session, and assembler of a run stay encapsulated together,
/// no caller can feed this executor a completion from another run; the
/// low-level [`OutputAssembler`] same-run pairing precondition is satisfied
/// by construction.
///
/// The call blocks until the run ends. On success the result holds exactly
/// one [`RangeOutput`] per canonical logical range, in plan order, each
/// covering its range exactly — returned only once the scheduler is
/// exhausted, no operation remains active, and every logical byte was
/// assembled. Exhaustion alone is never treated as success.
///
/// On any failure the run is fail-closed: new scheduling and submission
/// stop, the session is drained until no operation can still access its
/// owned resources, successful completions observed during drainage are
/// destroyed without being recorded, every reservation releases through
/// RAII, and no partial output is observable.
///
/// # Errors
///
/// Returns [`PreadExecutionError::Assembly`] when preparing, recording, or
/// finishing logical outputs fails, [`PreadExecutionError::Scheduling`]
/// when scheduler construction or a scheduling decision fails permanently,
/// [`PreadExecutionError::Read`] when one positioned read fails —
/// including EOF inside an admitted range and partial reads the file
/// cannot complete — [`PreadExecutionError::CompletionRetention`] when the
/// session cannot retain one more completion,
/// [`PreadExecutionError::StalledWithoutActiveWork`] for the impossible
/// waiting-with-nothing-active state, and
/// [`PreadExecutionError::DrainageFailed`] when drainage after a primary
/// failure reports an additional failure.
///
/// # Examples
///
/// The hand-calculated fixture: one logical range `[0, 14)` split at read
/// size 4 under a 10-byte budget.
///
/// ```
/// use std::fs::File;
/// use std::io::Write;
///
/// use range_replay::{
///     ByteBudget, ExecutionConfig, ExecutionPlan, ReadPlan, ReadRange, ReadSize, execute_pread,
/// };
///
/// let path = std::env::temp_dir()
///     .join(format!("range-replay-doc-execute-pread-{}", std::process::id()));
/// File::create_new(&path)?.write_all(b"abcdefghijklmn")?;
/// let file = File::open(&path)?;
///
/// let plan = ReadPlan::try_from_schedule(&[ReadRange::try_new(0, 14)?])?;
/// let config = ExecutionConfig::try_new(ReadSize::try_new(4)?, ByteBudget::try_new(10)?)?;
/// let execution = ExecutionPlan::try_from_read_plan(&plan, config)?;
///
/// let outputs = execute_pread(&file, execution)?;
///
/// assert_eq!(outputs.len(), 1);
/// assert_eq!(outputs[0].range(), ReadRange::try_new(0, 14)?);
/// assert_eq!(outputs[0].bytes(), b"abcdefghijklmn");
///
/// std::fs::remove_file(&path)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn execute_pread(
    file: &File,
    plan: ExecutionPlan,
) -> Result<Vec<RangeOutput>, PreadExecutionError> {
    execute_with_session(plan, PreadSession::new(file)).map_err(PreadExecutionError::from)
}
