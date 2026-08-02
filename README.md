# range-replay

`range-replay` is a small educational Rust project for planning and executing
file-range read schedules on Linux.

> Given a file-range schedule and an in-flight memory budget, can a synchronous
> `pread` reference backend and an `io_uring` backend return exactly the same
> bytes while reporting the physical work honestly?

## Status

**Early implementation.** The validated `ReadRange` value type, deterministic
coalescing of overlapping or adjacent ranges, the textual `offset,length`
schedule format parsed by `parse_schedule`, the validated `ReadPlan` boundary
type owning the canonical coalesced ranges, the synchronous positioned-read
(`pread`) reference backend executing a `ReadPlan` against an open file, a
minimal synchronous command line exposing that pipeline, deterministic
per-range SHA-256 checksums over completed range outputs (`checksum`,
library-only: the CLI neither renders nor compares checksums), and validated
execution configuration with compact deterministic physical planning
(`ReadSize` bounds one physical read, `ByteBudget` bounds the total bytes in
flight, `ExecutionConfig` validates `read_size <= byte_budget` by
construction, and the derived `ExecutionPlan` splits with the read size only,
stores one planned entry per logical range, and computes each physical read
on demand instead of materializing them; library-only: the CLI takes no
configuration arguments), a single-threaded `BudgetLimiter` enforcing the
in-flight byte budget through uniquely owned RAII `Reservation` guards (the
accounting primitive: several planned reads can be admitted together when
the budget can hold their combined lengths), and a compact greedy
`Scheduler` incrementally selecting pending physical reads from an owned
`ExecutionPlan` — greatest fitting length first, equal lengths in plan
order, no combination search — reserving their exact bytes through an
internal limiter before returning uniquely owned `ScheduledRead` handles
that pair a stable `OperationId` with the reservation (scheduling primitive
only: temporary `WaitingForBudget` backpressure stays distinct from plan
exhaustion, and exhaustion is not execution completion; library-only: the
CLI is unchanged), and a synchronous one-operation adapter
(`read_scheduled`) executing exactly one admitted `ScheduledRead` through
the `pread` exact-read loop into a backend-neutral `CompletedRead` that
owns the exact physical bytes and keeps the reservation live until the
completion is destroyed (the physical buffer is dropped before the
reservation releases, every error releases the admitted bytes and exposes
no partial output, and the file cursor never moves; library-only: the CLI
is unchanged) exist. No executor loop, logical output assembly from
physical completions, backend selection, or `io_uring` backend does yet.

## Usage

```text
range-replay <DATA_FILE> <SCHEDULE_FILE>
```

The schedule file is UTF-8 text with one `offset,length` line per requested
range. The schedule is parsed, coalesced into the canonical plan, and executed
against the data file with the synchronous `pread` backend. On success, stdout
holds exactly one line per canonical range, ordered by ascending offset:

```text
offset,length,hex
```

`hex` is the complete range payload, each byte rendered as exactly two
lowercase, zero-padded hexadecimal characters; payload bytes are never
interpreted as text. For a data file containing `0123456789abcdef` and a
schedule containing:

```text
10,4
2,3
```

the exact output is:

```text
2,3,323334
10,4,61626364
```

Any failure — an unreadable or invalid schedule, an empty plan, an unopenable
data file, or a backend error such as reading past end of file — is reported
on stderr with its full cause chain and a non-zero exit status, and stdout
stays empty: no partial output is ever rendered for a failed run.

The project is deliberately bounded. It is a Rust and Linux systems-learning
exercise, not a production storage engine or a general-purpose async runtime.

## Planned `v0.1`

This list is the full `v0.1` scope; the Status section above tracks which
items already exist.

- An explicit file-range schedule format.
- Deterministic validation and coalescing of overlapping or adjacent ranges.
- A synchronous `pread` reference backend.
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
