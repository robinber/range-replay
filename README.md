# range-replay

`range-replay` is a small educational Rust project for planning and executing
file-range read schedules on Linux.

> Given a file-range schedule and an in-flight memory budget, can a synchronous
> `pread` reference backend and an `io_uring` backend return exactly the same
> bytes while reporting the physical work honestly?

## Status

**Planning only. No implementation exists yet.**

The project is deliberately bounded. It is a Rust and Linux systems-learning
exercise, not a production storage engine or a general-purpose async runtime.

## Planned `v0.1`

- An explicit file-range schedule format.
- Deterministic validation and coalescing of overlapping or adjacent ranges.
- A synchronous `pread` reference backend.
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
