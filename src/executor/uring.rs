//! Bounded Linux `io_uring` execution of one physical plan.
//!
//! [`execute_uring`] is the second production backend behind the private
//! generic driver of the parent module: it consumes one
//! [`ExecutionPlan`], borrows one [`File`], and drives the same
//! scheduling, submission, completion, and assembly loop as
//! [`execute_pread`](crate::execute_pread), but submits every admitted
//! physical read to a real kernel `io_uring` instance. Several reads may
//! be active in the kernel at once — bounded independently by the plan's
//! in-flight byte budget and by the validated [`UringQueueDepth`] — while
//! the call itself stays synchronous: no Rust async runtime, thread, or
//! channel exists here.
//!
//! # Ownership while the kernel reads
//!
//! Every successfully submitted operation is represented by one private
//! `InFlight` value owning the zero-initialized physical buffer and the
//! admitted [`ScheduledRead`]. The session keeps that value in a
//! token-indexed table from before the SQE is pushed until the
//! operation's terminal CQE is consumed, so the buffer allocation is
//! stable for the whole time the kernel may write into it, and the
//! reservation stays counted for at least as long as the buffer exists.
//! Tokens are opaque, non-repeating `u64` values used as SQE user data;
//! no raw pointer is ever encoded there.
//!
//! # Fail-closed drainage
//!
//! Failures follow the parent module's model: after a primary failure no
//! new work is submitted, every already-submitted operation is awaited
//! naturally, successful drainage completions are destroyed without
//! being recorded, and the typed primary failure is returned with no
//! partial output. When completions can no longer be observed at all —
//! the completion wait itself fails, or an unknown token proves the
//! session protocol broken beyond local repair — the session abandons
//! the affected operations by *leaking* their buffers and reservations
//! instead of freeing memory the kernel may still write into, and
//! reports the unproven drainage as a typed error.
//!
//! # The single unsafe boundary
//!
//! The one `unsafe` block in this crate is the SQE push inside the
//! private kernel ring adapter. Its safety proof rests on the ownership
//! rules above; everything around it — validation, allocation, token
//! bookkeeping, adjudication — is safe code.

use std::collections::TryReserveError;
use std::collections::hash_map::{Entry as TableEntry, HashMap};
use std::fs::File;
use std::num::TryFromIntError;
use std::os::fd::AsRawFd;
use std::{io, mem};

use io_uring::{IoUring, opcode, types};
use thiserror::Error;

use super::{BackendSession, DriverFailure, execute_with_session};
use crate::completion::CompletedRead;
use crate::execution::ExecutionPlan;
use crate::output::{AssemblyError, RangeOutput};
use crate::range::ReadRange;
use crate::scheduler::{ScheduledRead, SchedulerError};

/// Reason a [`UringQueueDepth`] could not be constructed.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum UringQueueDepthError {
    /// The requested queue depth was `0`, so no read could ever be
    /// submitted.
    #[error("queue depth must be greater than zero")]
    ZeroQueueDepth,
}

/// A validated, non-zero bound on simultaneously submitted kernel reads.
///
/// The queue depth bounds how many physical reads one
/// [`execute_uring`] run keeps submitted to the kernel at once. It is
/// independent of the plan's in-flight byte budget: the budget bounds
/// admitted *bytes*, the depth bounds submitted *operations*, and a read
/// is only submitted when both admit it. Like
/// [`ReadSize`](crate::ReadSize), the depth is an explicit tuning input,
/// not a measured optimum. `UringQueueDepth` is [`Copy`] because it is
/// immutable configuration.
///
/// # Examples
///
/// ```
/// use range_replay::{UringQueueDepth, UringQueueDepthError};
///
/// let depth = UringQueueDepth::try_new(3)?;
/// assert_eq!(depth.operations(), 3);
///
/// assert_eq!(
///     UringQueueDepth::try_new(0),
///     Err(UringQueueDepthError::ZeroQueueDepth)
/// );
/// # Ok::<(), UringQueueDepthError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UringQueueDepth {
    operations: u32,
}

impl UringQueueDepth {
    /// Creates a depth allowing up to `operations` simultaneously
    /// submitted reads.
    ///
    /// # Errors
    ///
    /// Returns [`UringQueueDepthError::ZeroQueueDepth`] when `operations`
    /// is `0`.
    pub const fn try_new(operations: u32) -> Result<Self, UringQueueDepthError> {
        if operations == 0 {
            return Err(UringQueueDepthError::ZeroQueueDepth);
        }

        Ok(Self { operations })
    }

    /// Returns the maximum number of simultaneously submitted reads,
    /// which is always at least `1`.
    #[must_use]
    pub const fn operations(&self) -> u32 {
        self.operations
    }
}

/// Reason one [`execute_uring`] run failed globally.
///
/// Every variant is terminal for its run: after any of them, no new work
/// was scheduled or submitted and no partial output is observable. In
/// almost every case the session was also drained to a proven idle
/// state; the one exception is deliberate — see
/// [`Self::CompletionWaitFailed`] and [`Self::DrainageUnproven`], which
/// report operations whose buffers were leaked because freeing memory
/// the kernel may still write into would be unsound.
#[derive(Debug, Error)]
pub enum UringExecutionError {
    /// Preparing logical outputs, recording one completion, or finishing
    /// assembly failed.
    #[error("assembling logical outputs failed")]
    Assembly(#[source] AssemblyError),
    /// Constructing the scheduler or making one scheduling decision
    /// failed permanently.
    ///
    /// Temporary budget backpressure is never an error; the driver waits
    /// on active work instead.
    #[error("scheduling physical reads failed")]
    Scheduling(#[source] SchedulerError),
    /// Creating the kernel `io_uring` instance failed.
    ///
    /// This is where an unsupported or restricted kernel surfaces:
    /// `ENOSYS`, `EPERM`, and resource limits all arrive here unchanged.
    #[error("creating the io_uring instance failed")]
    RingCreation {
        /// The underlying failure reported by ring setup.
        source: io::Error,
    },
    /// A physical range length does not fit in `usize`, so no buffer of
    /// that size is representable on this platform.
    #[error(
        "range [{}, {}): length {} is not representable as a buffer size",
        .range.offset(),
        .range.end(),
        .range.length()
    )]
    UnrepresentableBufferLength {
        /// The range whose length no buffer can represent.
        range: ReadRange,
        /// The underlying integer conversion failure.
        source: TryFromIntError,
    },
    /// A physical range length does not fit in the 32-bit SQE length
    /// field, so no single `io_uring` read can express it.
    #[error(
        "range [{}, {}): length {} does not fit the 32-bit SQE length field",
        .range.offset(),
        .range.end(),
        .range.length()
    )]
    UnrepresentableSqeLength {
        /// The range whose length no SQE can express.
        range: ReadRange,
        /// The underlying integer conversion failure.
        source: TryFromIntError,
    },
    /// The physical read buffer could not be reserved.
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
    /// The token table could not reserve room for one more in-flight
    /// operation.
    #[error("cannot reserve token-table capacity for one more in-flight read")]
    TokenTableAllocation {
        /// The underlying reservation failure reported by Rust.
        source: TryReserveError,
    },
    /// The non-repeating completion-token space was exhausted.
    ///
    /// Tokens are allocated from a monotonically increasing `u64`, so
    /// this guards an arithmetic impossibility for any realistic run
    /// instead of describing a reachable input; the admitted read is
    /// destroyed and its reservation released before this returns.
    #[error("the io_uring completion-token space is exhausted")]
    TokenSpaceExhausted,
    /// A freshly allocated completion token was already tracking an
    /// in-flight operation.
    ///
    /// This cannot occur while tokens are allocated monotonically; it
    /// guards the table against ever holding two operations under one
    /// token, which would make completions ambiguous. The already
    /// tracked operation is untouched and the new read is destroyed.
    #[error("completion token {token} already tracks an in-flight read")]
    TokenCollision {
        /// The token that was unexpectedly in use.
        token: u64,
    },
    /// The submission queue rejected the SQE before it was published.
    ///
    /// The push happens before any kernel visibility, so this failure
    /// rolls back completely: the tracked entry is removed and its
    /// buffer and reservation are destroyed. It cannot occur while the
    /// session bounds submitted operations by a queue depth no larger
    /// than the ring, and guards that bound instead of describing a
    /// reachable input.
    #[error(
        "range [{}, {}): the submission queue rejected the SQE before publication",
        .range.offset(),
        .range.end()
    )]
    SqePushRejected {
        /// The range whose SQE was rejected.
        range: ReadRange,
    },
    /// Submitting published SQEs to the kernel failed.
    ///
    /// The SQE was already published when this happens, so the kernel
    /// may have observed it: the operation stays tracked and owned, no
    /// further work is submitted, and the run drains before this error
    /// becomes observable.
    #[error("submitting published SQEs to the kernel failed")]
    SubmissionFailed {
        /// The underlying failure reported by the submission call.
        source: io::Error,
    },
    /// Waiting for one completion failed, so completions can no longer
    /// be observed.
    ///
    /// Without completions the session can never again prove that the
    /// kernel is done with any submitted buffer, so every still-tracked
    /// operation was abandoned: its buffer and reservation were leaked —
    /// never freed — which keeps the leaked bytes counted against the
    /// budget and keeps the kernel from ever writing into freed memory.
    #[error(
        "waiting for an io_uring completion failed; {abandoned} unfinished operations were \
         abandoned with their buffers leaked"
    )]
    CompletionWaitFailed {
        /// The underlying failure reported by the completion wait.
        source: io::Error,
        /// Number of operations whose buffers and reservations leaked.
        abandoned: usize,
    },
    /// One read completed with a negative result, preserving the kernel
    /// errno.
    #[error("range [{}, {}): the io_uring read failed", .range.offset(), .range.end())]
    CompletionIo {
        /// The range whose read failed.
        range: ReadRange,
        /// The kernel failure, reconstructed from the CQE errno.
        source: io::Error,
    },
    /// One read completed with fewer bytes than its admitted range.
    ///
    /// Short results are not retried or resubmitted in this backend;
    /// the run fails instead of silently succeeding over unread bytes.
    #[error(
        "range [{}, {}): the io_uring read returned {actual} of {expected} bytes",
        .range.offset(),
        .range.end()
    )]
    ShortRead {
        /// The range whose read came up short.
        range: ReadRange,
        /// The byte count the range requires.
        expected: u64,
        /// The byte count the read actually returned.
        actual: u64,
    },
    /// One read completed with `0` bytes: the file ends at or before the
    /// admitted range.
    #[error(
        "range [{}, {}): unexpected end of file before any of {expected} bytes",
        .range.offset(),
        .range.end()
    )]
    UnexpectedEof {
        /// The range the file ended before.
        range: ReadRange,
        /// The byte count the range requires.
        expected: u64,
    },
    /// One read reported more bytes than its admitted range.
    ///
    /// A conforming kernel can never report more bytes than the buffer
    /// it was given, so this guards the completion protocol instead of
    /// describing a reachable input; rejecting it keeps a contract
    /// violation from becoming silent success.
    #[error(
        "range [{}, {}): the io_uring read reported {reported} bytes for {expected}",
        .range.offset(),
        .range.end()
    )]
    OverreportedResult {
        /// The range whose read over-reported.
        range: ReadRange,
        /// The byte count the range requires.
        expected: u64,
        /// The byte count the read claimed.
        reported: u64,
    },
    /// A completion arrived for a token no tracked operation owns.
    ///
    /// An unknown token proves the session protocol broken — a duplicate
    /// or foreign completion — and is never ignored: the session stops
    /// accepting submissions, drains every known operation first, and
    /// only then reports this error, so no tracked buffer is ever freed
    /// while its own completion could still be pending.
    #[error("an io_uring completion arrived for unknown token {token}")]
    UnknownCompletionToken {
        /// The token no tracked operation owns.
        token: u64,
    },
    /// A completed buffer did not cover its admitted range exactly.
    ///
    /// The session only constructs a completion after adjudicating an
    /// exact result, so this guards completion construction instead of
    /// describing a reachable input.
    #[error(
        "range [{}, {}): a completed buffer holds {actual} bytes for a {expected}-byte read",
        .range.offset(),
        .range.end()
    )]
    CompletionConstruction {
        /// The admitted physical range the buffer was meant to cover.
        range: ReadRange,
        /// The byte count the range requires.
        expected: u64,
        /// The byte count the rejected buffer actually holds.
        actual: usize,
    },
    /// A submission was attempted after the session was poisoned.
    ///
    /// The session poisons itself when a completion failure proves its
    /// protocol or observability broken, and the driver never submits
    /// after any failure, so this guards an unreachable ordering instead
    /// of describing a reachable input; the admitted read is destroyed
    /// and its reservation released before this returns.
    #[error("a read was submitted to a poisoned io_uring session")]
    PoisonedSubmission,
    /// No progress was possible because no backend operation was active.
    ///
    /// The driver reports this when the scheduler asks it to wait for
    /// budget — or the session reports exhausted submission capacity —
    /// while nothing is active, and the session reports the same
    /// impossibility when a completion is requested from an idle
    /// session. Both states are unreachable through a correct session,
    /// so the driver returns this typed error instead of spinning.
    #[error("no progress is possible: no backend operation is active")]
    StalledWithoutActiveWork,
    /// The session could not prove an idle safe state during drainage.
    ///
    /// Some operations were abandoned earlier — their buffers and
    /// reservations leaked because completions could no longer be
    /// observed — so idleness is unprovable and is reported instead of
    /// claimed. The leaked buffers are never freed.
    #[error(
        "io_uring drainage could not prove an idle session; {abandoned} operations remain \
         abandoned with their buffers leaked"
    )]
    DrainageUnproven {
        /// Number of operations whose buffers and reservations leaked.
        abandoned: usize,
    },
    /// Draining the session after a primary failure reported an
    /// additional failure.
    ///
    /// The primary failure stays the source of this variant, so the
    /// cause chain always surfaces what originally failed; the drainage
    /// failure is a sibling field outside the
    /// [`source`](std::error::Error::source) chain — it appears in the
    /// display text, and programmatic inspection must match this variant
    /// to observe both failures. Several cleanup failures nest, and the
    /// innermost end of the chain is always the failure that originally
    /// ended the run.
    #[error("draining the io_uring session after a failure reported another failure: {drainage}")]
    DrainageFailed {
        /// The failure that ended the run before drainage began.
        #[source]
        primary: Box<UringExecutionError>,
        /// The additional failure drainage itself reported.
        drainage: Box<UringExecutionError>,
    },
}

/// Typed terminal failure of one [`UringSession`] operation.
#[derive(Debug)]
enum UringSessionError {
    /// The range length does not fit in `usize`.
    UnrepresentableBufferLength {
        range: ReadRange,
        source: TryFromIntError,
    },
    /// The range length does not fit the 32-bit SQE length field.
    UnrepresentableSqeLength {
        range: ReadRange,
        source: TryFromIntError,
    },
    /// The physical buffer could not be reserved.
    BufferAllocation {
        range: ReadRange,
        source: TryReserveError,
    },
    /// The token table could not reserve one more slot.
    TokenTableAllocation { source: TryReserveError },
    /// The non-repeating token space was exhausted.
    TokenSpaceExhausted,
    /// A fresh token was already tracking an operation.
    TokenCollision { token: u64 },
    /// The submission queue rejected the SQE before publication.
    SqePushRejected { range: ReadRange },
    /// Submitting published SQEs to the kernel failed.
    SubmissionFailed { source: io::Error },
    /// Waiting for a completion failed; tracked operations were leaked.
    CompletionWaitFailed { source: io::Error, abandoned: usize },
    /// One read completed with a negative result.
    CompletionIo { range: ReadRange, source: io::Error },
    /// One read returned fewer bytes than its range, but more than zero.
    ShortRead {
        range: ReadRange,
        expected: u64,
        actual: u64,
    },
    /// One read returned zero bytes.
    UnexpectedEof { range: ReadRange, expected: u64 },
    /// One read reported more bytes than its range.
    OverreportedResult {
        range: ReadRange,
        expected: u64,
        reported: u64,
    },
    /// A completion arrived for a token no tracked operation owns.
    UnknownCompletionToken { token: u64 },
    /// A completed buffer did not cover its range exactly.
    CompletionConstruction {
        range: ReadRange,
        expected: u64,
        actual: usize,
    },
    /// A submission was attempted after the session was poisoned.
    PoisonedSubmission,
    /// A completion was requested while nothing was active.
    NothingActive,
    /// Idleness is unprovable because operations were abandoned.
    DrainageUnproven { abandoned: usize },
}

impl From<UringSessionError> for UringExecutionError {
    fn from(failure: UringSessionError) -> Self {
        match failure {
            UringSessionError::UnrepresentableBufferLength { range, source } => {
                Self::UnrepresentableBufferLength { range, source }
            }
            UringSessionError::UnrepresentableSqeLength { range, source } => {
                Self::UnrepresentableSqeLength { range, source }
            }
            UringSessionError::BufferAllocation { range, source } => {
                Self::BufferAllocation { range, source }
            }
            UringSessionError::TokenTableAllocation { source } => {
                Self::TokenTableAllocation { source }
            }
            UringSessionError::TokenSpaceExhausted => Self::TokenSpaceExhausted,
            UringSessionError::TokenCollision { token } => Self::TokenCollision { token },
            UringSessionError::SqePushRejected { range } => Self::SqePushRejected { range },
            UringSessionError::SubmissionFailed { source } => Self::SubmissionFailed { source },
            UringSessionError::CompletionWaitFailed { source, abandoned } => {
                Self::CompletionWaitFailed { source, abandoned }
            }
            UringSessionError::CompletionIo { range, source } => {
                Self::CompletionIo { range, source }
            }
            UringSessionError::ShortRead {
                range,
                expected,
                actual,
            } => Self::ShortRead {
                range,
                expected,
                actual,
            },
            UringSessionError::UnexpectedEof { range, expected } => {
                Self::UnexpectedEof { range, expected }
            }
            UringSessionError::OverreportedResult {
                range,
                expected,
                reported,
            } => Self::OverreportedResult {
                range,
                expected,
                reported,
            },
            UringSessionError::UnknownCompletionToken { token } => {
                Self::UnknownCompletionToken { token }
            }
            UringSessionError::CompletionConstruction {
                range,
                expected,
                actual,
            } => Self::CompletionConstruction {
                range,
                expected,
                actual,
            },
            UringSessionError::PoisonedSubmission => Self::PoisonedSubmission,
            UringSessionError::NothingActive => Self::StalledWithoutActiveWork,
            UringSessionError::DrainageUnproven { abandoned } => {
                Self::DrainageUnproven { abandoned }
            }
        }
    }
}

impl From<DriverFailure<UringSessionError>> for UringExecutionError {
    fn from(failure: DriverFailure<UringSessionError>) -> Self {
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

/// The token and signed result of one reaped CQE.
#[derive(Clone, Copy, Debug)]
struct RingCompletion {
    token: u64,
    result: i32,
}

/// Marker for an SQE push that failed before publication.
///
/// The push never became visible to the kernel, so the caller may roll
/// the operation back completely.
#[derive(Clone, Copy, Debug)]
struct SqePushRejected;

/// Internal contract one ring adapter offers the [`UringSession`].
///
/// The trait is a private testing seam under the session, not a public
/// extension point: production uses the one [`KernelRing`] over a real
/// `io_uring` instance, and tests script deterministic outcomes the
/// kernel cannot be forced to produce. The session is the only caller
/// and upholds the cross-call obligation the safe signature cannot
/// encode: a buffer passed to [`Self::push_read`] stays owned by the
/// session's token table, unmoved and unresized, until the completion
/// for its token is returned by [`Self::wait_completion`] or the
/// session abandons it by leaking.
trait Ring {
    /// Pushes one read SQE for `token` covering `buffer` at absolute
    /// file `offset`, without publishing it to the kernel on its own.
    ///
    /// An error means the entry was *not* published and the operation
    /// can be rolled back safely.
    fn push_read(
        &mut self,
        token: u64,
        buffer: &mut [u8],
        offset: u64,
    ) -> Result<(), SqePushRejected>;

    /// Publishes previously pushed SQEs to the kernel.
    ///
    /// After a successful push, an error here no longer proves the
    /// kernel could not have observed the entry.
    fn submit(&mut self) -> io::Result<()>;

    /// Blocks until one completion is available, publishing any still
    /// pending SQEs first, and returns its token and signed result.
    fn wait_completion(&mut self) -> io::Result<RingCompletion>;
}

/// The production [`Ring`] over one kernel `io_uring` instance.
///
/// The adapter borrows the file for its whole lifetime, so the
/// descriptor referenced by every pushed SQE cannot be closed by safe
/// Rust before the ring itself is destroyed; for operations the kernel
/// has started, the kernel additionally holds its own file reference.
struct KernelRing<'file> {
    ring: IoUring,
    file: &'file File,
}

impl Ring for KernelRing<'_> {
    fn push_read(
        &mut self,
        token: u64,
        buffer: &mut [u8],
        offset: u64,
    ) -> Result<(), SqePushRejected> {
        // The session validates the 32-bit bound before allocating, so
        // this re-derivation cannot fail; failing pre-publication keeps
        // even a hypothetically broken caller safe.
        let Ok(length) = u32::try_from(buffer.len()) else {
            return Err(SqePushRejected);
        };

        let entry = opcode::Read::new(
            types::Fd(self.file.as_raw_fd()),
            buffer.as_mut_ptr(),
            length,
        )
        .offset(offset)
        .build()
        .user_data(token);

        let mut queue = self.ring.submission();

        #[expect(
            unsafe_code,
            reason = "the io_uring SQ push is the single approved unsafe boundary of the crate; \
                      the session's token-table ownership proves buffer and descriptor stability"
        )]
        // SAFETY: The pointer and length describe the caller's buffer,
        // which the session's token table owns under `token` — inserted
        // before this call — and keeps allocated, unmoved, and unresized
        // until the operation's terminal CQE is consumed or the entry is
        // deliberately leaked; a `Vec`'s heap allocation is stable while
        // it is neither resized nor dropped, so the address stays valid
        // for the whole kernel-side lifetime of the read. The file
        // descriptor stays open for at least as long: `self` borrows the
        // `File` for the ring's whole lifetime and the kernel holds its
        // own reference to started operations. The reservation admitted
        // for the read is owned by the same table entry, and the session
        // never frees the buffer without either consuming the terminal
        // CQE or leaking the entry, so the kernel can never write into
        // freed or reused memory through this SQE.
        let pushed = unsafe { queue.push(&entry) };

        pushed.map_err(|_full| SqePushRejected)
    }

    fn submit(&mut self) -> io::Result<()> {
        loop {
            match self.ring.submit() {
                Ok(_submitted) => return Ok(()),
                Err(interrupted) if interrupted.kind() == io::ErrorKind::Interrupted => {}
                Err(source) => return Err(source),
            }
        }
    }

    fn wait_completion(&mut self) -> io::Result<RingCompletion> {
        loop {
            if let Some(entry) = self.ring.completion().next() {
                return Ok(RingCompletion {
                    token: entry.user_data(),
                    result: entry.result(),
                });
            }

            // Waiting also submits still-pending SQEs, so an operation
            // whose eager submission failed after its push is published
            // here at the latest.
            match self.ring.submit_and_wait(1) {
                Ok(_submitted) => {}
                Err(interrupted) if interrupted.kind() == io::ErrorKind::Interrupted => {}
                Err(source) => return Err(source),
            }
        }
    }
}

/// One submitted physical read the kernel may still be writing.
///
/// The session owns every value exclusively through its token table.
/// The buffer is declared before the scheduled handle, so destruction
/// destroys the physical buffer before the reservation releases —
/// the same ordering [`CompletedRead`] guarantees after completion.
#[derive(Debug)]
struct InFlight {
    buffer: Vec<u8>,
    scheduled: ScheduledRead,
}

/// Bounded backend session over one [`Ring`].
///
/// The session tracks every submitted operation in a token-indexed
/// table and enforces the queue depth by reporting exhausted submission
/// capacity to the driver, which then waits for a completion instead of
/// scheduling replacement work. It is deliberately not `Clone` and no
/// work survives it: on destruction, any operation still tracked —
/// unreachable through a correct driver — is leaked rather than freed,
/// as the last-resort ownership guard against a kernel write into
/// freed memory.
#[derive(Debug)]
struct UringSession<R> {
    ring: R,
    depth: usize,
    in_flight: HashMap<u64, InFlight>,
    next_token: u64,
    poisoned: bool,
    abandoned: Option<usize>,
}

impl<R: Ring> UringSession<R> {
    fn new(ring: R, queue_depth: UringQueueDepth) -> Self {
        // On a platform whose `usize` cannot hold a `u32` the depth
        // saturates, which keeps it a hard bound either way.
        let depth = usize::try_from(queue_depth.operations()).unwrap_or(usize::MAX);

        Self {
            ring,
            depth,
            in_flight: HashMap::new(),
            next_token: 0,
            poisoned: false,
            abandoned: None,
        }
    }

    /// Abandons every tracked operation by leaking it, and returns the
    /// total number of operations abandoned so far.
    ///
    /// Called only when completions can no longer be observed: freeing a
    /// buffer the kernel may still write into would be unsound, so the
    /// buffers and their reservations are deliberately leaked. The
    /// leaked bytes stay counted against the budget, which is the honest
    /// accounting for memory that never returns.
    fn abandon_in_flight(&mut self) -> usize {
        let abandoned = self.in_flight.len();

        for (_token, entry) in self.in_flight.drain() {
            mem::forget(entry);
        }

        self.poisoned = true;
        let total = self.abandoned.unwrap_or(0).saturating_add(abandoned);
        self.abandoned = Some(total);

        total
    }

    /// Turns one reaped CQE into its terminal outcome.
    ///
    /// Exactly one tracked operation is removed for a known token; its
    /// signed result is adjudicated into an exact completion or a typed
    /// error that destroys the buffer before the reservation releases.
    /// An unknown token poisons the session and drains every known
    /// operation before the protocol error becomes observable, so no
    /// tracked buffer is freed while its own completion may be pending.
    fn adjudicate(
        &mut self,
        completion: RingCompletion,
    ) -> Result<CompletedRead, UringSessionError> {
        let Some(entry) = self.in_flight.remove(&completion.token) else {
            self.poisoned = true;

            while !self.in_flight.is_empty() {
                match self.ring.wait_completion() {
                    Ok(drained) => drop(self.in_flight.remove(&drained.token)),
                    Err(_source) => {
                        // The typed protocol error below stays primary;
                        // the leak surfaces as unproven drainage when the
                        // driver drains the session.
                        self.abandon_in_flight();
                        break;
                    }
                }
            }

            return Err(UringSessionError::UnknownCompletionToken {
                token: completion.token,
            });
        };

        let range = entry.scheduled.range();
        let expected = range.length();

        let Ok(reported) = u64::try_from(completion.result) else {
            let errno = completion.result.checked_neg().unwrap_or(i32::MAX);
            drop(entry);

            return Err(UringSessionError::CompletionIo {
                range,
                source: io::Error::from_raw_os_error(errno),
            });
        };

        if reported == 0 {
            drop(entry);
            return Err(UringSessionError::UnexpectedEof { range, expected });
        }

        if reported < expected {
            drop(entry);
            return Err(UringSessionError::ShortRead {
                range,
                expected,
                actual: reported,
            });
        }

        if reported > expected {
            drop(entry);
            return Err(UringSessionError::OverreportedResult {
                range,
                expected,
                reported,
            });
        }

        let InFlight { buffer, scheduled } = entry;

        CompletedRead::try_new(buffer, scheduled).map_err(|mismatch| {
            UringSessionError::CompletionConstruction {
                range,
                expected: mismatch.expected,
                actual: mismatch.actual,
            }
        })
    }
}

impl<R: Ring> BackendSession for UringSession<R> {
    type Error = UringSessionError;

    fn submit(&mut self, scheduled: ScheduledRead) -> Result<(), UringSessionError> {
        // Every early return below destroys `scheduled` — and any buffer
        // local declared after it drops first — so a failed submission
        // always releases its reservation after its buffer.
        if self.poisoned {
            return Err(UringSessionError::PoisonedSubmission);
        }

        let range = scheduled.range();

        let length = usize::try_from(range.length())
            .map_err(|source| UringSessionError::UnrepresentableBufferLength { range, source })?;
        // The SQE length field is 32-bit; refuse a read the ring cannot
        // express instead of truncating it.
        u32::try_from(range.length())
            .map_err(|source| UringSessionError::UnrepresentableSqeLength { range, source })?;

        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(length)
            .map_err(|source| UringSessionError::BufferAllocation { range, source })?;
        buffer.resize(length, 0);

        self.in_flight
            .try_reserve(1)
            .map_err(|source| UringSessionError::TokenTableAllocation { source })?;

        let token = self.next_token;
        let Some(bumped) = token.checked_add(1) else {
            return Err(UringSessionError::TokenSpaceExhausted);
        };
        self.next_token = bumped;

        let entry = match self.in_flight.entry(token) {
            TableEntry::Occupied(_existing) => {
                return Err(UringSessionError::TokenCollision { token });
            }
            TableEntry::Vacant(slot) => slot.insert(InFlight { buffer, scheduled }),
        };

        // The entry is tracked before the SQE exists, so a completion
        // can never arrive for an untracked token, and the buffer
        // pointer taken here stays valid under the table's ownership.
        if self
            .ring
            .push_read(token, &mut entry.buffer, range.offset())
            .is_err()
        {
            // Pre-publication failure: the kernel never saw the SQE, so
            // the rollback destroys the buffer and reservation safely.
            drop(self.in_flight.remove(&token));
            return Err(UringSessionError::SqePushRejected { range });
        }

        if let Err(source) = self.ring.submit() {
            // Post-publication failure: the kernel may have observed the
            // SQE, so the operation stays tracked and owned; the run
            // fails closed and drains it before the error is observable.
            self.poisoned = true;
            return Err(UringSessionError::SubmissionFailed { source });
        }

        Ok(())
    }

    fn wait_for_completion(&mut self) -> Result<CompletedRead, UringSessionError> {
        if self.in_flight.is_empty() {
            return Err(UringSessionError::NothingActive);
        }

        match self.ring.wait_completion() {
            Ok(completion) => self.adjudicate(completion),
            Err(source) => {
                let abandoned = self.abandon_in_flight();

                Err(UringSessionError::CompletionWaitFailed { source, abandoned })
            }
        }
    }

    fn has_active(&self) -> bool {
        !self.in_flight.is_empty()
    }

    fn has_submission_capacity(&self) -> bool {
        self.in_flight.len() < self.depth
    }

    fn drain(&mut self) -> Result<(), UringSessionError> {
        if let Some(abandoned) = self.abandoned {
            return Err(UringSessionError::DrainageUnproven { abandoned });
        }

        while !self.in_flight.is_empty() {
            match self.ring.wait_completion() {
                // Unknown tokens match no entry and drop nothing; every
                // known completion destroys its buffer before its
                // reservation releases through the entry's field order.
                Ok(completion) => drop(self.in_flight.remove(&completion.token)),
                Err(source) => {
                    let abandoned = self.abandon_in_flight();

                    return Err(UringSessionError::CompletionWaitFailed { source, abandoned });
                }
            }
        }

        Ok(())
    }
}

impl<R> Drop for UringSession<R> {
    fn drop(&mut self) {
        // Unreachable through a correct driver, which always drains the
        // session before destroying it. Any operation still tracked here
        // may still be written by the kernel, so it is leaked — never
        // freed — as the last-resort ownership guard.
        for (_token, entry) in self.in_flight.drain() {
            mem::forget(entry);
        }
    }
}

/// Executes one physical plan against one open file through a bounded
/// kernel `io_uring` session.
///
/// The plan is consumed by value — moving it performs no clone — while
/// the file is only borrowed and its shared cursor never moves, because
/// every read is a positioned `io_uring` read at an explicit offset.
/// Scheduling, submission, completion, and assembly follow exactly the
/// same fail-closed driver as [`execute_pread`](crate::execute_pread):
/// on success the result holds exactly one [`RangeOutput`] per canonical
/// logical range, in plan order, each covering its range exactly, and
/// for equal inputs both backends return identical bytes and checksums.
///
/// Up to `queue_depth` reads stay submitted to the kernel at once, on
/// top of the plan's own hard in-flight byte budget: a read is submitted
/// only when the budget admits its bytes *and* the session has
/// submission capacity, and capacity freed by a completion is reused
/// only after the completed bytes were recorded into their logical
/// output. The call blocks until the run ends; kernel-side concurrency
/// needs no Rust async runtime, thread, or channel.
///
/// On any failure the run is fail-closed: new scheduling and submission
/// stop, every already-submitted operation is awaited, successful
/// drainage completions are destroyed without being recorded, and no
/// partial output is observable. If completions can no longer be
/// observed at all, the affected buffers are leaked rather than freed —
/// see [`UringExecutionError::CompletionWaitFailed`] — so the kernel can
/// never write into memory this process has reused.
///
/// # Platform
///
/// The call needs a Linux kernel with `io_uring` read support (5.6 or
/// newer) and permission to create a ring; a restricted or unsupported
/// kernel surfaces as [`UringExecutionError::RingCreation`].
///
/// # Errors
///
/// Returns the [`UringExecutionError`] variant naming the stage that
/// failed: ring creation, validation, allocation, token bookkeeping,
/// SQE push or submission, completion wait, result adjudication — I/O,
/// EOF, short and over-reported reads, unknown tokens — completion
/// construction, scheduling, assembly, the impossible stalled state,
/// and drainage failures that preserve the primary cause.
///
/// # Examples
///
/// The hand-calculated fixture: one logical range `[0, 12)` split at
/// read size 4 under an 8-byte budget and queue depth 3.
///
/// ```
/// use std::fs::File;
/// use std::io::Write;
///
/// use range_replay::{
///     ByteBudget, ExecutionConfig, ExecutionPlan, ReadPlan, ReadRange, ReadSize,
///     UringQueueDepth, execute_uring,
/// };
///
/// let path = std::env::temp_dir()
///     .join(format!("range-replay-doc-execute-uring-{}", std::process::id()));
/// File::create_new(&path)?.write_all(b"abcdefghijkl")?;
/// let file = File::open(&path)?;
///
/// let plan = ReadPlan::try_from_schedule(&[ReadRange::try_new(0, 12)?])?;
/// let config = ExecutionConfig::try_new(ReadSize::try_new(4)?, ByteBudget::try_new(8)?)?;
/// let execution = ExecutionPlan::try_from_read_plan(&plan, config)?;
///
/// let outputs = execute_uring(&file, execution, UringQueueDepth::try_new(3)?)?;
///
/// assert_eq!(outputs.len(), 1);
/// assert_eq!(outputs[0].range(), ReadRange::try_new(0, 12)?);
/// assert_eq!(outputs[0].bytes(), b"abcdefghijkl");
///
/// std::fs::remove_file(&path)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn execute_uring(
    file: &File,
    plan: ExecutionPlan,
    queue_depth: UringQueueDepth,
) -> Result<Vec<RangeOutput>, UringExecutionError> {
    let ring = IoUring::new(queue_depth.operations())
        .map_err(|source| UringExecutionError::RingCreation { source })?;

    let session = UringSession::new(KernelRing { ring, file }, queue_depth);

    execute_with_session(plan, session).map_err(UringExecutionError::from)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{self, ErrorKind, Read as _, Seek, SeekFrom};

    use super::super::{BackendSession, DriverFailure, execute_pread, execute_with_session};
    use super::{
        InFlight, Ring, RingCompletion, SqePushRejected, UringExecutionError, UringQueueDepth,
        UringQueueDepthError, UringSession, UringSessionError, execute_uring,
    };
    use crate::checksum::checksum;
    use crate::output::OutputAssembler;
    use crate::test_support::{
        BDAC_FIXTURE, HEX_FIXTURE, admitted_single, assert_waiting, execution, ready,
        scheduler_for, span, with_file_content,
    };

    /// One scripted outcome for one `wait_completion` call, in call
    /// order.
    enum WaitScript {
        /// Reap a CQE with this token and signed result.
        Cqe { token: u64, result: i32 },
        /// Fail the completion wait terminally.
        Fail(ErrorKind),
    }

    /// Deterministic scripted ring that performs no kernel I/O.
    ///
    /// Pushed reads are "performed" at push time by copying fixture
    /// bytes into the buffer, like the kernel write they stand in for;
    /// completion outcomes are scripted, so protocol states a real
    /// kernel cannot be forced to produce — short results, duplicates,
    /// unknown tokens, over-reports, wait failures — stay
    /// deterministically testable.
    struct ScriptedRing {
        file: Vec<u8>,
        pushes: Vec<(u64, u64, usize)>,
        submissions: usize,
        reject_next_push: bool,
        fail_next_submit: Option<ErrorKind>,
        wait_script: VecDeque<WaitScript>,
    }

    impl ScriptedRing {
        fn over(file: &[u8], wait_script: Vec<WaitScript>) -> Self {
            Self {
                file: file.to_vec(),
                pushes: Vec::new(),
                submissions: 0,
                reject_next_push: false,
                fail_next_submit: None,
                wait_script: wait_script.into(),
            }
        }
    }

    impl Ring for ScriptedRing {
        fn push_read(
            &mut self,
            token: u64,
            buffer: &mut [u8],
            offset: u64,
        ) -> Result<(), SqePushRejected> {
            if self.reject_next_push {
                self.reject_next_push = false;
                return Err(SqePushRejected);
            }

            let start = usize::try_from(offset).expect("test offsets fit in usize");
            for (index, byte) in buffer.iter_mut().enumerate() {
                *byte = self.file.get(start + index).copied().unwrap_or(0);
            }

            self.pushes.push((token, offset, buffer.len()));

            Ok(())
        }

        fn submit(&mut self) -> io::Result<()> {
            if let Some(kind) = self.fail_next_submit.take() {
                return Err(io::Error::from(kind));
            }

            self.submissions += 1;

            Ok(())
        }

        fn wait_completion(&mut self) -> io::Result<RingCompletion> {
            match self
                .wait_script
                .pop_front()
                .expect("the wait script covers every wait")
            {
                WaitScript::Cqe { token, result } => Ok(RingCompletion { token, result }),
                WaitScript::Fail(kind) => Err(io::Error::from(kind)),
            }
        }
    }

    fn depth(operations: u32) -> UringQueueDepth {
        UringQueueDepth::try_new(operations).expect("test depths are non-zero")
    }

    fn session_over(
        file: &[u8],
        operations: u32,
        wait_script: Vec<WaitScript>,
    ) -> UringSession<ScriptedRing> {
        UringSession::new(ScriptedRing::over(file, wait_script), depth(operations))
    }

    #[test]
    fn queue_depth_rejects_zero_and_preserves_its_exact_value() {
        assert_eq!(
            UringQueueDepth::try_new(0),
            Err(UringQueueDepthError::ZeroQueueDepth)
        );
        assert_eq!(depth(1).operations(), 1);
        assert_eq!(depth(u32::MAX).operations(), u32::MAX);
    }

    #[test]
    fn submitting_pushes_the_sqe_and_submits_it_to_the_kernel() {
        let mut scheduler = scheduler_for(execution(&[span(10, 14)], 4, 4));
        let mut session = session_over(HEX_FIXTURE, 1, Vec::new());

        session
            .submit(ready(&mut scheduler))
            .expect("the single read submits");

        assert_eq!(session.ring.pushes, vec![(0, 10, 4)]);
        assert_eq!(session.ring.submissions, 1);
        assert!(session.has_active());
        assert!(!session.has_submission_capacity());
    }

    #[test]
    fn a_rejected_sqe_push_rolls_back_before_publication() {
        let mut scheduler = scheduler_for(execution(&[span(0, 8)], 4, 8));
        let mut session = session_over(HEX_FIXTURE, 2, Vec::new());
        session.ring.reject_next_push = true;

        let error = session
            .submit(ready(&mut scheduler))
            .expect_err("the scripted push rejection surfaces");

        assert!(matches!(
            error,
            UringSessionError::SqePushRejected { range } if range == span(0, 4)
        ));
        assert!(!session.has_active(), "the rolled-back entry is gone");
        assert!(session.ring.pushes.is_empty());
        assert_eq!(session.ring.submissions, 0, "nothing was submitted");
        assert_eq!(scheduler.in_flight_bytes(), 0, "the reservation released");

        // The rollback is complete: the session accepts the next
        // admission — the [4, 8) read — with a fresh token.
        session
            .submit(ready(&mut scheduler))
            .expect("the session stays usable after a pre-publication rollback");
        assert_eq!(session.ring.pushes, vec![(1, 4, 4)]);
    }

    #[test]
    fn a_post_push_submission_failure_retains_ownership_until_drainage() {
        let mut scheduler = scheduler_for(execution(&[span(10, 14)], 4, 4));
        let mut session = session_over(
            HEX_FIXTURE,
            1,
            vec![WaitScript::Cqe {
                token: 0,
                result: 4,
            }],
        );
        session.ring.fail_next_submit = Some(ErrorKind::Other);

        let error = session
            .submit(ready(&mut scheduler))
            .expect_err("the scripted submission failure surfaces");

        assert!(matches!(error, UringSessionError::SubmissionFailed { .. }));
        assert!(
            session.has_active(),
            "a published SQE may have been observed by the kernel, so its operation stays owned"
        );
        assert_eq!(session.ring.pushes, vec![(0, 10, 4)]);
        assert_eq!(
            scheduler.in_flight_bytes(),
            4,
            "the reservation stays counted while the kernel may still write the buffer"
        );

        // Drainage then consumes the operation's terminal CQE and
        // destroys it, releasing the reservation.
        let drained = session
            .wait_for_completion()
            .expect("the drained completion is exact");
        drop(drained);
        assert!(!session.has_active());
        session.drain().expect("the session proves an idle state");
        assert_eq!(scheduler.in_flight_bytes(), 0);
    }

    #[test]
    fn an_exact_cqe_returns_the_completion_and_holds_its_reservation() {
        let mut scheduler = scheduler_for(execution(&[span(10, 14)], 4, 4));
        let mut session = session_over(
            HEX_FIXTURE,
            1,
            vec![WaitScript::Cqe {
                token: 0,
                result: 4,
            }],
        );
        session
            .submit(ready(&mut scheduler))
            .expect("the single read submits");

        let completed = session
            .wait_for_completion()
            .expect("the exact result completes");

        assert_eq!(completed.range(), span(10, 14));
        assert_eq!(completed.bytes(), b"abcd");
        assert!(!session.has_active());
        assert_eq!(
            scheduler.in_flight_bytes(),
            4,
            "the completion owns the reservation until it is destroyed"
        );

        drop(completed);
        assert_eq!(scheduler.in_flight_bytes(), 0);
    }

    #[test]
    fn a_short_cqe_is_a_typed_short_read_that_releases_its_reservation() {
        let mut scheduler = scheduler_for(execution(&[span(10, 14)], 4, 4));
        let mut session = session_over(
            HEX_FIXTURE,
            1,
            vec![WaitScript::Cqe {
                token: 0,
                result: 2,
            }],
        );
        session
            .submit(ready(&mut scheduler))
            .expect("the single read submits");

        let error = session
            .wait_for_completion()
            .expect_err("a short result must not become silent success");

        assert!(matches!(
            error,
            UringSessionError::ShortRead {
                range,
                expected: 4,
                actual: 2,
            } if range == span(10, 14)
        ));
        assert!(!session.has_active());
        assert_eq!(scheduler.in_flight_bytes(), 0);
    }

    #[test]
    fn a_zero_cqe_is_a_typed_unexpected_eof() {
        let mut scheduler = scheduler_for(execution(&[span(10, 14)], 4, 4));
        let mut session = session_over(
            HEX_FIXTURE,
            1,
            vec![WaitScript::Cqe {
                token: 0,
                result: 0,
            }],
        );
        session
            .submit(ready(&mut scheduler))
            .expect("the single read submits");

        let error = session
            .wait_for_completion()
            .expect_err("a zero result is the end of the file");

        assert!(matches!(
            error,
            UringSessionError::UnexpectedEof {
                range,
                expected: 4,
            } if range == span(10, 14)
        ));
        assert_eq!(scheduler.in_flight_bytes(), 0);
    }

    #[test]
    fn a_negative_cqe_preserves_its_errno() {
        let mut scheduler = scheduler_for(execution(&[span(10, 14)], 4, 4));
        let mut session = session_over(
            HEX_FIXTURE,
            1,
            vec![WaitScript::Cqe {
                token: 0,
                result: -5,
            }],
        );
        session
            .submit(ready(&mut scheduler))
            .expect("the single read submits");

        let error = session
            .wait_for_completion()
            .expect_err("a negative result is a kernel failure");

        let UringSessionError::CompletionIo { range, source } = error else {
            panic!("expected a completion I/O failure, got {error:?}");
        };
        assert_eq!(range, span(10, 14));
        assert_eq!(source.raw_os_error(), Some(5));
        assert_eq!(scheduler.in_flight_bytes(), 0);
    }

    #[test]
    fn an_overreported_cqe_is_rejected_instead_of_trusted() {
        let mut scheduler = scheduler_for(execution(&[span(10, 14)], 4, 4));
        let mut session = session_over(
            HEX_FIXTURE,
            1,
            vec![WaitScript::Cqe {
                token: 0,
                result: 5,
            }],
        );
        session
            .submit(ready(&mut scheduler))
            .expect("the single read submits");

        let error = session
            .wait_for_completion()
            .expect_err("an over-reported result must not become silent success");

        assert!(matches!(
            error,
            UringSessionError::OverreportedResult {
                range,
                expected: 4,
                reported: 5,
            } if range == span(10, 14)
        ));
        assert_eq!(scheduler.in_flight_bytes(), 0);
    }

    #[test]
    fn a_duplicate_cqe_drains_known_work_before_reporting_the_unknown_token() {
        let mut scheduler = scheduler_for(execution(&[span(0, 8)], 4, 8));
        let mut session = session_over(
            HEX_FIXTURE,
            2,
            vec![
                WaitScript::Cqe {
                    token: 0,
                    result: 4,
                },
                WaitScript::Cqe {
                    token: 0,
                    result: 4,
                },
                WaitScript::Cqe {
                    token: 1,
                    result: 4,
                },
            ],
        );
        session
            .submit(ready(&mut scheduler))
            .expect("the first read submits");
        session
            .submit(ready(&mut scheduler))
            .expect("the second read submits");

        let completed = session
            .wait_for_completion()
            .expect("the first completion is exact");

        // The duplicate token matches no tracked operation any more; the
        // session drains the second read internally — destroying it
        // without recording — before the protocol error surfaces.
        let error = session
            .wait_for_completion()
            .expect_err("a duplicate completion is a protocol breach");

        assert!(matches!(
            error,
            UringSessionError::UnknownCompletionToken { token: 0 }
        ));
        assert!(!session.has_active(), "known work was drained first");
        assert_eq!(
            scheduler.in_flight_bytes(),
            4,
            "only the held completion still owns budget bytes"
        );

        drop(completed);
        assert_eq!(scheduler.in_flight_bytes(), 0);
        session
            .drain()
            .expect("the drained session proves idleness");
    }

    #[test]
    fn an_unknown_token_poisons_the_session_against_further_submissions() {
        let mut scheduler = scheduler_for(execution(&[span(0, 4), span(10, 14)], 4, 4));
        let mut session = session_over(
            HEX_FIXTURE,
            2,
            vec![
                WaitScript::Cqe {
                    token: 99,
                    result: 4,
                },
                WaitScript::Cqe {
                    token: 0,
                    result: 4,
                },
            ],
        );
        session
            .submit(ready(&mut scheduler))
            .expect("the first read submits");

        let error = session
            .wait_for_completion()
            .expect_err("an unknown token is a protocol breach");

        assert!(matches!(
            error,
            UringSessionError::UnknownCompletionToken { token: 99 }
        ));
        assert!(!session.has_active(), "the known read was drained first");
        assert_eq!(scheduler.in_flight_bytes(), 0);

        let error = session
            .submit(ready(&mut scheduler))
            .expect_err("a poisoned session refuses new submissions");

        assert!(matches!(error, UringSessionError::PoisonedSubmission));
        assert_eq!(
            scheduler.in_flight_bytes(),
            0,
            "the refused admission released its reservation"
        );
    }

    #[test]
    fn token_space_exhaustion_fails_closed_before_tracking_anything() {
        let mut scheduler = scheduler_for(execution(&[span(10, 14)], 4, 4));
        let mut session = session_over(HEX_FIXTURE, 1, Vec::new());
        session.next_token = u64::MAX;

        let error = session
            .submit(ready(&mut scheduler))
            .expect_err("the token space cannot bump past u64::MAX");

        assert!(matches!(error, UringSessionError::TokenSpaceExhausted));
        assert!(!session.has_active());
        assert!(session.ring.pushes.is_empty());
        assert_eq!(scheduler.in_flight_bytes(), 0);
    }

    #[test]
    fn a_token_collision_fails_closed_and_leaves_the_tracked_read_untouched() {
        let (occupant_scheduler, occupant) = admitted_single(0, 3, 3);
        let mut scheduler = scheduler_for(execution(&[span(10, 14)], 4, 4));
        let mut session = session_over(HEX_FIXTURE, 2, Vec::new());
        session.next_token = 7;
        session.in_flight.insert(
            7,
            InFlight {
                buffer: vec![0; 3],
                scheduled: occupant,
            },
        );

        let error = session
            .submit(ready(&mut scheduler))
            .expect_err("a colliding token must never track two reads");

        assert!(matches!(
            error,
            UringSessionError::TokenCollision { token: 7 }
        ));
        assert_eq!(
            scheduler.in_flight_bytes(),
            0,
            "the rejected admission released its reservation"
        );
        assert!(session.has_active(), "the tracked read is untouched");
        assert_eq!(occupant_scheduler.in_flight_bytes(), 3);

        drop(session.in_flight.remove(&7));
        assert_eq!(occupant_scheduler.in_flight_bytes(), 0);
    }

    #[test]
    fn a_wait_failure_abandons_every_operation_and_leaks_instead_of_freeing() {
        let mut scheduler = scheduler_for(execution(&[span(0, 8)], 4, 8));
        let mut session = session_over(HEX_FIXTURE, 2, vec![WaitScript::Fail(ErrorKind::Other)]);
        session
            .submit(ready(&mut scheduler))
            .expect("the first read submits");
        session
            .submit(ready(&mut scheduler))
            .expect("the second read submits");

        let error = session
            .wait_for_completion()
            .expect_err("the scripted wait failure surfaces");

        assert!(matches!(
            error,
            UringSessionError::CompletionWaitFailed { abandoned: 2, .. }
        ));
        assert!(
            !session.has_active(),
            "abandoned operations are no longer tracked"
        );

        let error = session
            .drain()
            .expect_err("idleness is unprovable after abandonment");
        assert!(matches!(
            error,
            UringSessionError::DrainageUnproven { abandoned: 2 }
        ));

        drop(session);
        assert_eq!(
            scheduler.in_flight_bytes(),
            8,
            "leaked buffers keep their bytes counted against the budget forever"
        );
    }

    #[test]
    fn submission_capacity_is_consumed_by_active_reads_and_freed_by_completions() {
        let mut scheduler = scheduler_for(execution(&[span(0, 12)], 4, 12));
        let mut session = session_over(
            HEX_FIXTURE,
            2,
            vec![WaitScript::Cqe {
                token: 0,
                result: 4,
            }],
        );

        assert!(session.has_submission_capacity());
        session
            .submit(ready(&mut scheduler))
            .expect("the first read submits");
        assert!(session.has_submission_capacity());
        session
            .submit(ready(&mut scheduler))
            .expect("the second read submits");
        assert!(
            !session.has_submission_capacity(),
            "two active reads exhaust a depth of two"
        );

        let completed = session
            .wait_for_completion()
            .expect("the first completion is exact");
        assert!(
            session.has_submission_capacity(),
            "consuming a terminal CQE frees its queue slot"
        );
        drop(completed);
    }

    #[test]
    fn the_abc_fixture_proves_a_cqe_does_not_release_budget_before_recording() {
        // The hand-calculated ownership fixture: "abcdefghijkl", one
        // logical range [0, 12) at read size 4, byte budget 8, queue
        // depth 3. A = [0, 4), B = [4, 8), C = [8, 12).
        let plan = execution(&[span(0, 12)], 4, 8);
        let mut assembler =
            OutputAssembler::try_new(&plan).expect("the fixture plan prepares its outputs");
        let mut scheduler = scheduler_for(plan);
        let mut session = session_over(
            b"abcdefghijkl",
            3,
            vec![
                WaitScript::Cqe {
                    token: 1,
                    result: 4,
                },
                WaitScript::Cqe {
                    token: 0,
                    result: 4,
                },
                WaitScript::Cqe {
                    token: 2,
                    result: 4,
                },
            ],
        );

        session.submit(ready(&mut scheduler)).expect("A submits");
        assert_eq!(scheduler.available_bytes(), 4);

        session.submit(ready(&mut scheduler)).expect("B submits");
        assert_eq!(scheduler.available_bytes(), 0);

        // The budget, not the queue depth, is what blocks now.
        assert_waiting(&mut scheduler);
        assert!(session.has_submission_capacity());

        let completed = session.wait_for_completion().expect("B completes exactly");
        assert_eq!(completed.range(), span(4, 8));
        assert_eq!(completed.bytes(), b"efgh");

        // Receiving the CQE alone must not release budget capacity.
        assert_eq!(scheduler.available_bytes(), 0);
        assert_eq!(scheduler.in_flight_bytes(), 8);

        // Recording destroys B's physical buffer and releases its
        // reservation; only now does replacement work fit.
        assembler
            .record(completed)
            .expect("B records into its logical output");
        assert_eq!(scheduler.available_bytes(), 4);

        session.submit(ready(&mut scheduler)).expect("C submits");
        assert_eq!(scheduler.available_bytes(), 0);

        // Finish the run: record A and C, prove the session idle, and
        // assemble the exact logical bytes.
        for _ in 0..2 {
            let completed = session
                .wait_for_completion()
                .expect("the remaining completions are exact");
            assembler
                .record(completed)
                .expect("the remaining completions record");
        }
        assert!(!session.has_active());
        session.drain().expect("the idle session proves drainage");

        let outputs = assembler
            .finish()
            .expect("every logical byte was assembled");
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].bytes(), b"abcdefghijkl");
        assert_eq!(scheduler.available_bytes(), 8);
    }

    #[test]
    fn the_failure_fixture_keeps_a_primary_and_drains_c_without_output() {
        // Continues the ownership fixture: A fails terminally while C
        // remains active; C is drained and destroyed without being
        // recorded, and no output is observable.
        let plan = execution(&[span(0, 12)], 4, 8);
        let mut assembler =
            OutputAssembler::try_new(&plan).expect("the fixture plan prepares its outputs");
        let mut scheduler = scheduler_for(plan);
        let mut session = session_over(
            b"abcdefghijkl",
            3,
            vec![
                WaitScript::Cqe {
                    token: 1,
                    result: 4,
                },
                WaitScript::Cqe {
                    token: 0,
                    result: -5,
                },
                WaitScript::Cqe {
                    token: 2,
                    result: 4,
                },
            ],
        );

        session.submit(ready(&mut scheduler)).expect("A submits");
        session.submit(ready(&mut scheduler)).expect("B submits");
        let completed = session.wait_for_completion().expect("B completes exactly");
        assembler
            .record(completed)
            .expect("B records into its logical output");
        session.submit(ready(&mut scheduler)).expect("C submits");

        // A fails terminally while C remains active; A is removed and
        // destroyed, releasing its reservation.
        let error = session
            .wait_for_completion()
            .expect_err("A's completion reports the kernel failure");
        let UringSessionError::CompletionIo { range, source } = error else {
            panic!("expected A's I/O failure, got {error:?}");
        };
        assert_eq!(range, span(0, 4));
        assert_eq!(source.raw_os_error(), Some(5));
        assert_eq!(scheduler.in_flight_bytes(), 4, "only C stays admitted");
        assert!(session.has_active());

        // Drainage destroys the successful C without recording it.
        let drained = session
            .wait_for_completion()
            .expect("C completes during drainage");
        drop(drained);
        assert!(!session.has_active());
        session
            .drain()
            .expect("the drained session proves idleness");
        assert_eq!(scheduler.in_flight_bytes(), 0);

        // The incomplete assembler is destroyed; no partial output can
        // be observed anywhere.
        drop(assembler);
    }

    #[test]
    fn a_failing_drainage_completion_is_preserved_alongside_the_primary() {
        // The failure fixture's second half: C also fails during
        // drainage. Both failures stay observable and A remains the
        // original cause at the innermost end of the chain.
        let plan = execution(&[span(0, 12)], 4, 8);
        let session = session_over(
            b"abcdefghijkl",
            3,
            vec![
                WaitScript::Cqe {
                    token: 1,
                    result: 4,
                },
                WaitScript::Cqe {
                    token: 0,
                    result: -5,
                },
                WaitScript::Cqe {
                    token: 2,
                    result: -22,
                },
            ],
        );

        let result = execute_with_session(plan, session);

        match result {
            Err(DriverFailure::Drainage { primary, drainage }) => {
                assert!(matches!(
                    *primary,
                    DriverFailure::Backend(UringSessionError::CompletionIo { range, .. })
                        if range == span(0, 4)
                ));
                assert!(matches!(
                    drainage,
                    UringSessionError::CompletionIo { range, .. } if range == span(8, 12)
                ));
            }
            other => panic!("expected a drainage failure, got {other:?}"),
        }
    }

    #[test]
    fn an_abandoning_drainage_nests_the_unproven_idle_state_around_the_primary() {
        // A fails; draining C then fails at the wait itself, so C is
        // abandoned and the final drain reports the unproven idle state.
        // Every layer stays observable and A remains the innermost cause.
        let plan = execution(&[span(0, 12)], 4, 8);
        let session = session_over(
            b"abcdefghijkl",
            3,
            vec![
                WaitScript::Cqe {
                    token: 1,
                    result: 4,
                },
                WaitScript::Cqe {
                    token: 0,
                    result: -5,
                },
                WaitScript::Fail(ErrorKind::Other),
            ],
        );

        let result = execute_with_session(plan, session);

        match result {
            Err(DriverFailure::Drainage { primary, drainage }) => {
                assert!(matches!(
                    drainage,
                    UringSessionError::DrainageUnproven { abandoned: 1 }
                ));
                match *primary {
                    DriverFailure::Drainage {
                        primary: inner,
                        drainage: pull_failure,
                    } => {
                        assert!(matches!(
                            pull_failure,
                            UringSessionError::CompletionWaitFailed { abandoned: 1, .. }
                        ));
                        assert!(matches!(
                            *inner,
                            DriverFailure::Backend(UringSessionError::CompletionIo { range, .. })
                                if range == span(0, 4)
                        ));
                    }
                    other => panic!("expected the nested pull failure, got {other:?}"),
                }
            }
            other => panic!("expected a drainage failure, got {other:?}"),
        }
    }

    #[test]
    fn the_driver_assembles_out_of_order_scripted_completions_in_plan_order() {
        // The BDAC fixture through the real driver over a scripted ring:
        // submission order A, B, D under the 10-byte budget, completions
        // scripted B, D, A, C.
        let plan = execution(&[span(0, 14)], 4, 10);
        let session = session_over(
            BDAC_FIXTURE,
            4,
            vec![
                WaitScript::Cqe {
                    token: 1,
                    result: 4,
                },
                WaitScript::Cqe {
                    token: 2,
                    result: 2,
                },
                WaitScript::Cqe {
                    token: 0,
                    result: 4,
                },
                WaitScript::Cqe {
                    token: 3,
                    result: 4,
                },
            ],
        );

        let outputs = execute_with_session(plan, session).expect("the scripted run succeeds");

        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].range(), span(0, 14));
        assert_eq!(outputs[0].bytes(), BDAC_FIXTURE);
    }

    #[test]
    fn a_scripted_short_read_fails_the_whole_run_with_no_partial_output() {
        // [0, 8) over a six-byte file: the second physical read [4, 8)
        // returns 2 of 4 bytes, which a real kernel reports for a range
        // crossing the end of the file.
        let plan = execution(&[span(0, 8)], 4, 8);
        let session = session_over(
            b"abcdef",
            2,
            vec![
                WaitScript::Cqe {
                    token: 0,
                    result: 4,
                },
                WaitScript::Cqe {
                    token: 1,
                    result: 2,
                },
            ],
        );

        let result = execute_with_session(plan, session);

        assert!(matches!(
            result,
            Err(DriverFailure::Backend(UringSessionError::ShortRead {
                range,
                expected: 4,
                actual: 2,
            })) if range == span(4, 8)
        ));
    }

    // The tests below need a real Linux kernel with io_uring support.
    // They are deliberately not skipped when ring creation fails: an
    // environment that cannot create a ring has not satisfied this
    // slice's gate.

    #[test]
    fn execute_uring_returns_one_exact_read() {
        with_file_content("uring-single", HEX_FIXTURE, |file| {
            let outputs = execute_uring(file, execution(&[span(2, 5)], 4, 4), depth(1))
                .expect("the range is inside the fixture");

            assert_eq!(outputs.len(), 1);
            assert_eq!(outputs[0].range(), span(2, 5));
            assert_eq!(outputs[0].bytes(), b"234");
        });
    }

    #[test]
    fn execute_uring_reads_serially_at_queue_depth_one() {
        with_file_content("uring-depth-one", HEX_FIXTURE, |file| {
            let outputs = execute_uring(
                file,
                execution(&[span(0, 10), span(12, 16)], 4, 8),
                depth(1),
            )
            .expect("both ranges are inside the fixture");

            assert_eq!(outputs.len(), 2);
            assert_eq!(outputs[0].bytes(), b"0123456789");
            assert_eq!(outputs[1].bytes(), b"cdef");
        });
    }

    #[test]
    fn execute_uring_reads_concurrently_at_a_deeper_queue() {
        with_file_content("uring-concurrent", BDAC_FIXTURE, |file| {
            let outputs = execute_uring(file, execution(&[span(0, 14)], 4, 10), depth(3))
                .expect("the whole fixture range is readable");

            assert_eq!(outputs.len(), 1);
            assert_eq!(outputs[0].range(), span(0, 14));
            assert_eq!(outputs[0].bytes(), BDAC_FIXTURE);
        });
    }

    #[test]
    fn execute_uring_assembles_scattered_ranges_and_tails_in_plan_order() {
        with_file_content("uring-scattered", HEX_FIXTURE, |file| {
            let outputs = execute_uring(
                file,
                execution(&[span(10, 15), span(2, 5), span(6, 8)], 4, 8),
                depth(2),
            )
            .expect("every range is inside the fixture");

            assert_eq!(outputs.len(), 3);
            assert_eq!(outputs[0].range(), span(2, 5));
            assert_eq!(outputs[0].bytes(), b"234");
            assert_eq!(outputs[1].range(), span(6, 8));
            assert_eq!(outputs[1].bytes(), b"67");
            assert_eq!(outputs[2].range(), span(10, 15));
            assert_eq!(outputs[2].bytes(), b"abcde");
        });
    }

    #[test]
    fn execute_uring_matches_execute_pread_with_equal_checksums() {
        with_file_content("uring-parity", HEX_FIXTURE, |file| {
            let schedule = [span(10, 14), span(2, 5), span(0, 16)];

            let reference = execute_pread(file, execution(&schedule, 4, 8))
                .expect("the reference backend succeeds");

            for operations in [1, 2, 5] {
                let outputs = execute_uring(file, execution(&schedule, 4, 8), depth(operations))
                    .expect("the io_uring backend succeeds");

                assert_eq!(outputs, reference);
                for (uring_output, pread_output) in outputs.iter().zip(&reference) {
                    assert_eq!(checksum(uring_output), checksum(pread_output));
                }
            }
        });
    }

    #[test]
    fn a_range_crossing_eof_is_a_typed_short_read_with_no_partial_output() {
        with_file_content("uring-short-eof", b"abcdef", |file| {
            let error = execute_uring(file, execution(&[span(0, 8)], 4, 8), depth(2))
                .expect_err("the file ends inside the second physical read");

            assert!(matches!(
                error,
                UringExecutionError::ShortRead {
                    range,
                    expected: 4,
                    actual: 2,
                } if range == span(4, 8)
            ));
        });
    }

    #[test]
    fn a_range_past_eof_is_a_typed_unexpected_eof_with_no_partial_output() {
        with_file_content("uring-past-eof", b"abc", |file| {
            let error = execute_uring(file, execution(&[span(4, 8)], 4, 4), depth(1))
                .expect_err("the range starts past the end of the file");

            assert!(matches!(
                error,
                UringExecutionError::UnexpectedEof {
                    range,
                    expected: 4,
                } if range == span(4, 8)
            ));
        });
    }

    #[test]
    fn execute_uring_leaves_the_cursor_unchanged_and_the_file_usable() {
        with_file_content("uring-cursor", HEX_FIXTURE, |file| {
            file.seek(SeekFrom::Start(7)).expect("fixture file seeks");

            let outputs = execute_uring(file, execution(&[span(2, 5)], 4, 4), depth(1))
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
}
