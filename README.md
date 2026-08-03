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

The byte budget limits the physical read buffers simultaneously in flight,
not the final logical output buffers or total process memory. No `io_uring`
backend, backend selector, comparison report, measurement report, or
displayed checksum exists yet.

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

## Planned `v0.1`

This list is the full `v0.1` scope; the Status section above tracks which
items already exist.

- An explicit file-range schedule format.
- Deterministic validation and coalescing of overlapping or adjacent ranges.
- A synchronous `pread` reference backend.
- A fail-closed library executor driving the budget-aware physical plan
  (`execute_pread`).
- A minimal synchronous command line exposing the pipeline.
- An `io_uring` backend with a strict in-flight byte budget.
- Typed errors for invalid ranges, overflow, EOF, partial reads, and I/O
  failures.
- Checksums proving that both backends returned the same bytes.
- Deterministic reports for plans, byte counts, and operation counts.
- One machine-specific measurement report from an NVIDIA DGX Spark.

## Correctness boundary

The read plan, coalesced ranges, operation counts, byte counts, and output
checksums must be deterministic for equal inputs.

Elapsed time and throughput are physical measurements. They may vary between
runs and must record their machine and cache conditions rather than being
presented as deterministic or portable results.

## Non-goals

The first version will not include:

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

The project stops after the two backends agree on correctness, the in-flight
budget is enforced, and one reproducible Linux report is documented. Further
optimization requires a new explicit project decision.
