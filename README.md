# range-replay

`range-replay` is a small educational Rust project for planning and executing
file-range read schedules on Linux.

> Given a file-range schedule and an in-flight memory budget, can a synchronous
> `pread` reference backend and an `io_uring` backend return exactly the same
> bytes while reporting the physical work honestly?

## Status

**Early implementation.** The whole synchronous path exists as a library;
the crate rustdoc (`cargo doc`) is the authoritative description of every
exported item and its exact contract.

- Validated `ReadRange` values, deterministic coalescing, the textual
  `offset,length` schedule format (`parse_schedule`), and the validated
  `ReadPlan` boundary type.
- Deterministic per-range SHA-256 checksums (`checksum`; library-only, the
  CLI neither renders nor compares them).
- Validated execution configuration and compact physical planning
  (`ReadSize`, `ByteBudget`, `ExecutionConfig` proves
  `read_size <= byte_budget`, `ExecutionPlan` computes each physical read
  on demand).
- The in-flight byte budget enforced as a hard limit through uniquely
  owned RAII `Reservation` guards (`BudgetLimiter`).
- Budget-aware greedy scheduling of physical reads (`Scheduler`,
  `ScheduledRead`, `OperationId`); backpressure stays distinct from
  exhaustion, and exhaustion is not execution completion.
- The synchronous positioned-read reference backend (`read_plan`), the
  one-operation adapter (`read_scheduled`, `CompletedRead`), and
  out-of-order logical assembly (`OutputAssembler`, `RangeOutput`).
- A fail-closed synchronous executor (`execute_pread`) driving
  scheduling, submission, completion, and assembly end to end over a
  private backend session: global success requires an exhausted
  scheduler, an idle session, and complete assembly, and every failure
  drains the run and exposes no partial output
  (`PreadExecutionError`).
- A minimal synchronous CLI that validates an explicit execution
  configuration, derives the compact physical plan, and executes it
  through the budget-aware `execute_pread` executor.
- A Linux-only `io_uring` correctness backend (`execute_uring`) driving
  the same physical plans through the same fail-closed driver: a
  validated queue depth (`UringQueueDepth`) bounds simultaneously
  submitted kernel reads on top of the hard byte budget, completions may
  arrive out of order while outputs stay in plan order, and every
  failure is a typed `UringExecutionError` with no partial output. It
  needs a Linux kernel with `io_uring` read support (5.6 or newer) at
  runtime; on other platforms the library surface is unchanged and the
  `io-uring` dependency is not compiled.

The byte budget limits the physical read buffers simultaneously in flight,
not the final logical output buffers or total process memory. The library
now has an `io_uring` correctness backend, but no CLI backend selector,
workload matrix, comparison report, measurement report, or displayed
checksum exists yet; the CLI still executes only the `pread` backend.

## Usage

```text
range-replay --read-size <OCTETS> --byte-budget <OCTETS> <DATA_FILE> <SCHEDULE_FILE>
```

Both options are required raw decimal byte counts with no default:
`--read-size` bounds the length of one physical read and `--byte-budget`
bounds the physical read bytes simultaneously in flight. No KiB/MiB, SI,
hexadecimal, or expression suffixes are accepted, and the read size must
not exceed the byte budget.

The schedule file is UTF-8 text with one `offset,length` line per requested
range. The configuration is validated first, then the schedule is parsed,
coalesced into the canonical plan, split into the compact physical plan, and
executed against the data file with the synchronous budget-aware `pread`
executor. On success, stdout holds exactly one line per canonical range,
ordered by ascending offset:

```text
offset,length,hex
```

`hex` is the complete range payload, each byte rendered as exactly two
lowercase, zero-padded hexadecimal characters; payload bytes are never
interpreted as text. For a data file containing `0123456789abcdef`, a
schedule containing:

```text
10,4
2,3
```

and the invocation:

```text
range-replay --read-size 4 --byte-budget 10 data.bin schedule.txt
```

the exact output is:

```text
2,3,323334
10,4,61626364
```

The read size and byte budget shape only the physical execution, so every
valid configuration produces identical logical output for equal inputs.

Any failure — an invalid configuration, an unreadable or invalid schedule, an
empty plan, an unopenable data file, or an executor error such as reading
past end of file — is reported on stderr with its full cause chain and a
non-zero exit status, and stdout stays empty: rendering starts only after
the whole plan has executed successfully. An invalid configuration is
rejected before any filesystem access. The one boundary stdout cannot defend
is a write failure of the already-complete output (for example a pipe closed
mid-write): the run still exits non-zero with the failure on stderr, but
bytes the stream already accepted cannot be retracted.

The project is deliberately bounded. It is a Rust and Linux systems-learning
exercise, not a production storage engine or a general-purpose async runtime.

## Terminal `v0.1` scope

This is the complete scope of the project. The synchronous `pread` path and
the shared planning, scheduling, validation, and output foundations already
exist. They may change only where the remaining comparison needs a correctness
fix or the smallest backend-neutral measurement seam.

The only remaining work is:

1. Implement a Linux `io_uring` backend under the same typed-error, exact-byte,
   output-assembly, and hard in-flight-budget contracts as `pread`. Both
   backends must consume the same logical workloads and return identical bytes
   and checksums.
2. Run one bounded, predeclared comparison matrix covering multiple logical
   range sizes, multiple bounded concurrency or queue-depth settings, and both
   mostly sequential and scattered offsets. A common single-in-flight row must
   compare the backends directly; additional depths may isolate the effect of
   `io_uring` concurrency. Paired runs keep the data file, logical schedule,
   physical read size, byte budget, repetitions, and cache conditions fixed
   except for the named comparison axis.
3. Report throughput and latency, logical bytes requested, physical bytes
   actually read, physical operation count, and CPU cost when it can be
   measured reliably. Record the machine, OS, kernel, cache conditions, exact
   commands, and raw observations needed to audit the results.
4. Run one small coalescing or batching experiment over the same logical
   payload: compare separate small reads with fewer, larger physical reads, and
   report the trade-off in useful bytes, over-read bytes, operations, latency,
   and throughput. This is an experiment, not an adaptive policy or tuning
   framework.
5. Write one clear, machine-scoped conclusion describing when `pread`,
   `io_uring`, added concurrency, and coalescing help or hurt workloads that
   resemble tensor loading, including the limits of the evidence.

The terminal gate is backend correctness parity plus that reproducible report
and conclusion. Once it is satisfied, `range-replay` stops: do not add a later
milestone, another backend, an async runtime, an auto-tuner, or follow-on
optimizations to this repository. An interesting result may be documented as a
limitation, but it does not authorize more implementation here.

## Correctness boundary

The read plan, coalesced ranges, operation counts, byte counts, and output
checksums must be deterministic for equal inputs.

Elapsed time and throughput are physical measurements. They may vary between
runs and must record their machine and cache conditions rather than being
presented as deterministic or portable results.

## Non-goals

This repository will not include:

- macOS or Windows backends;
- a reusable async runtime;
- GPU execution or model inference;
- integration into `moe-sim`;
- multi-device or distributed I/O;
- `O_DIRECT` or portable cold-cache guarantees;
- claims of being the fastest possible reader.

## Learning workflow

Development will happen through small reviewable pull requests. Each slice
must have a hand-calculated example, focused tests, and an explanation of the
Rust ownership or safety invariant it introduces.

The project stops after the terminal gate in the scope above. There is no next
implementation milestone in this repository.
