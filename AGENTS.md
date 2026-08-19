# AGENTS.md

Machine-facing development guidance for coding agents working in this
repository. Humans should start with [`README.md`](README.md); this file exists
to make the engineering and verification contracts explicit.

## Load order

1. This file — repository-wide agent rules.
2. [`README.md`](README.md) — product scope, correctness boundary, and non-goals.
3. Shared Rust skill **rust-strict** (v1.3.0+) before any Rust change, review,
   debugging, or verification claim. One canonical checkout; Claude/Grok paths
   are symlinks:

   | Tool | Path |
   | --- | --- |
   | Codex (canonical submodule) | [`.agents/skills/rust-strict/SKILL.md`](.agents/skills/rust-strict/SKILL.md) |
   | Claude Code | [`.claude/skills/rust-strict/SKILL.md`](.claude/skills/rust-strict/SKILL.md) → symlink |
   | Grok | [`.grok/skills/rust-strict/SKILL.md`](.grok/skills/rust-strict/SKILL.md) → symlink |

   Source: https://github.com/robinber/agent-skills-rust (pin tag, currently
   `v1.3.0`).
4. Rust policy files: [`Cargo.toml`](Cargo.toml),
   [`rust-toolchain.toml`](rust-toolchain.toml),
   [`.cargo/config.toml`](.cargo/config.toml),
   [`.rustfmt.toml`](.rustfmt.toml), [`clippy.toml`](clippy.toml), and
   [`deny.toml`](deny.toml).
5. Subsystem documentation next to the code being changed.
6. For multi-agent orchestration, use `kira-mux` (project id `range-replay`).
   Prefer `kira-mux examples` for CLI recipes instead of a repo-local Kira skill.

When these documents appear to disagree, stop and surface the conflict. Do not
silently choose the interpretation that permits more work.

## Current package facts

- Single Cargo package at the repository root: `range-replay`.
- Edition `2024`, Rust `1.97.0`, license MIT, not published by default.
- Layout is deliberately minimal:

  ```text
  src/
    lib.rs       library surface (planning, validation, backends)
    budget.rs    in-flight byte budget and runtime accounting (`ByteBudget`, `BudgetLimiter`)
    checksum.rs  deterministic per-range SHA-256 checksums
    completion.rs backend-neutral exact completion of one admitted physical read (`CompletedRead`)
    execution.rs read-size-derived physical planning (`ReadSize`, `ExecutionConfig`, `ExecutionPlan`)
    executor.rs  fail-closed synchronous pread execution over a private backend
                 session (`execute_pread`, `PreadExecutionError`); its tests and
                 scripted fake session live in executor/tests.rs; the Linux-only
                 bounded io_uring session and facade live in executor/uring.rs
                 (`execute_uring`, `UringQueueDepth`, `UringExecutionError`),
                 compiled only for `cfg(target_os = "linux")` together with the
                 target-specific `io-uring` dependency, and needing a Linux
                 kernel with io_uring read support (5.6+) at runtime
    output.rs    logical outputs assembled from physical completions (`RangeOutput`, `OutputAssembler`)
    plan.rs      pure planning over range collections (coalescing, `ReadPlan`)
    pread.rs     synchronous positioned-read reference backend (`read_plan`, `read_scheduled`)
    range.rs     validated file-range value types
    schedule.rs  textual schedule parsing (`offset,length` lines)
    scheduler.rs budget-aware greedy selection of pending physical reads
                 (`Scheduler`, `ScheduledRead`, `ScheduleDecision`, `OperationId`)
    test_support.rs shared cfg(test) test fixtures (ranges, plans, schedulers,
                 temporary files)
    main.rs      thin binary entrypoint
    bin/range-replay-measure/
                 Linux-only, purpose-built terminal comparison runner with a
                 fixed workload matrix, one fixed-payload coalescing
                 experiment, process CPU-tick accounting, raw TSV rendering,
                 and portable pure-logic tests
  ```

  The parentheticals name flagship items, not exhaustive export lists;
  `src/lib.rs` is the authoritative public surface.

- `Cargo.lock` is committed because this package builds an application.
- [`REPORT.md`](REPORT.md) is the accepted terminal machine-specific report;
  [`results/v0.1/`](results/v0.1/) contains its small intentional raw
  observations and GNU `time` captures.
- Do not introduce a multi-crate workspace until there is a demonstrated need
  (isolated dependencies, multiple binaries with different graphs, or a clear
  ownership boundary with more than one consumer).

Do not describe planned commands, modules, formats, or results as implemented.

## Working rules

- This repository is an **educational, bounded** project. Prefer a finished
  narrow slice over expanding scope. Stopping after the terminal `v0.1` gate
  is required, not merely allowed.
- Make the smallest change that satisfies the approved request.
- Work one bounded slice at a time and satisfy its gate before starting another.
- Do **not** auto-chain into the next milestone. After an intermediate gate,
  the operator chooses continue / side quest / pause; after the terminal gate,
  stop.
- Follow existing module boundaries before introducing new abstractions.
- Work test-first when practical. For bugs, reproduce or localize the root
  cause before editing.
- Match the style of the file being edited.
- Mention unrelated drift and leave it untouched.
- Do not commit secrets, gated datasets, large generated results,
  machine-specific profiles, or private agent/runtime state.
- Self-check every design: if a senior engineer would call it overcomplicated,
  simplify it before claiming completion.

## Hard project ceiling

The synchronous `pread` path is complete. Remaining feature and research work
in this repository is limited to these five deliverables:

1. A Linux `io_uring` backend with the same correctness, typed-error, output,
   and hard in-flight-byte-budget contracts as `pread`. (Delivered; see the
   module inventory above and the README status.)
2. One bounded, predeclared workload matrix shared at the logical level by both
   backends, covering multiple range sizes, a common single-in-flight baseline,
   multiple bounded concurrency or queue-depth settings, and mostly sequential
   versus scattered access. Measurement support must stay purpose-built; do not
   introduce a reusable runtime or general benchmark framework.
3. One reproducible, machine-specific comparison reporting throughput,
   latency, logical requested bytes, physical bytes read, operation count, and
   CPU cost when reliable, with the controls and raw observations required for
   audit.
4. One small experiment comparing separate small reads with fewer, larger
   physical reads for the same logical payload, including over-read and
   operation-count trade-offs. Do not turn it into an adaptive coalescer,
   batching subsystem, or auto-tuner.
5. One final conclusion stating when `pread`, `io_uring`, concurrency, and
   coalescing help or hurt tensor-loading-like workloads, with explicit limits.

All five deliverables now have implementation, clean-commit VPS captures, raw
observations, and a conclusion in [`REPORT.md`](REPORT.md). The operator
accepted the terminal report on 2026-08-19. The terminal stop applies and
feature development has ended.

The acceptance details live in the README's terminal scope. After correctness
parity and the final reproducible report are accepted, stop feature development
in this repository. Do not open or implement a post-`v0.1` milestone, even when
the measurements suggest an interesting follow-up. Such a follow-up requires a
separate operator decision outside this project's scope and must not be added
to this repository's roadmap.

## Rust contract

Denied in non-test Rust code:

- `unsafe_code`;
- `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, and `dbg!`;
- undocumented public items, unless the package has an explicit narrower
  contract.

The Cargo lint tables enforce most of this policy, including `clippy::panic`,
`unwrap_used`, and `expect_used`. Tests may use stronger assertions and
intentional panics when they improve diagnostics; `clippy.toml` allows
unwrap/expect/panic in tests only.

The `io_uring` backend carries the one approved `unsafe` deviation (issue
#40): a single `unsafe` block containing the single SQ push in
`executor/uring.rs`, scoped with one `#[expect(unsafe_code, reason = ...)]`
and a `SAFETY` proof of buffer, descriptor, token, and reservation
ownership. The package-wide `unsafe_code = "deny"` policy stays in force;
any additional or widened `unsafe` needs its own explicit decision under
`rust-strict`, and never a blanket policy relaxation.

Preferred patterns:

- Use `thiserror` for library errors. Use `anyhow` only at the binary boundary.
- Return typed, actionable errors for invalid ranges, overflow, EOF, partial
  reads, and I/O failures.
- Keep `main.rs` thin; put behavior in the library so it stays testable.
- Keep planning and validation synchronous and deterministic. Add async only at
  a measured I/O boundary that establishes a real need.
- Add no dependency without a current use and a license/source-policy check.
- Keep intentional lint exceptions local with
  `#[expect(..., reason = "...")]`; do not add broad allowances.
- `clippy::pedantic` is enabled package-wide.
  `clippy::restriction` is not enabled as a group; only the selected
  low-noise rules in `Cargo.toml` apply.
- Read secrets from environment variables or untracked local configuration and
  redact them from logs, reports, diagnostics, and provenance.

## Architecture notes

The module boundaries above are established. Preserve them:

- pure planning / validation / coalescing logic separate from I/O backends;
- typed errors at domain boundaries;
- thin CLI or binary glue that only parses, dispatches, and renders.

Do not invent crates or plugin layers early. Split only when a real need is
demonstrated.

## Correctness invariants

These rules are correctness requirements for `v0.1`, not implementation
suggestions:

- Invalid ranges, overflows, and empty schedules are rejected with typed errors
  before any backend runs.
- One physical read is bounded to `ReadSize::MAX_BYTES` (1 GiB), a fixed
  backend-neutral ceiling kept below Linux's per-read transfer cap. Larger
  read sizes are rejected with a typed error before any backend runs; larger
  logical ranges stay valid and split deterministically.
- Coalescing of overlapping or adjacent ranges is deterministic for equal
  inputs.
- The in-flight byte budget is a hard limit. Never admit work that would exceed
  it.
- For equal inputs, both backends must return identical bytes and matching
  checksums.
- Plans, coalesced ranges, operation counts, byte counts, and output checksums
  are deterministic. Elapsed time and throughput are physical measurements and
  must record machine and cache conditions.
- Partial reads, short EOF, and I/O failures are typed errors, never silent
  success with truncated data.

Any change to these invariants is a shared-contract change. It requires an
explicit design decision, updated documentation, adversarial fixtures, and
independent review.

## Data and reproducibility

- Large datasets and generated benchmark trees stay outside source control.
- Small deterministic fixtures may be committed when necessary for tests and
  documentation.
- A machine-specific measurement report must record hardware, OS, kernel,
  cache conditions, and the exact commands used.
- Do not present one machine's timings as portable defaults.
- Preserve raw observations needed to audit a reported result.

## Excluded surfaces

Do not add these surfaces to this repository:

- macOS or Windows backends;
- a reusable async runtime;
- GPU execution or model inference;
- integration into `moe-sim`;
- multi-device or distributed I/O;
- `O_DIRECT` or portable cold-cache guarantees;
- claims of being the fastest possible reader.

These are terminal non-goals for `range-replay`, not missing scaffolding or a
post-`v0.1` roadmap. Exploring one requires a separate project decision outside
this repository.

## Commands

Reference quality gates:

```bash
cargo +nightly fmt --all
cargo +nightly fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
cargo deny check advisories licenses sources
```

Cargo aliases in `.cargo/config.toml`: `lint`, `lint-app`, `lint-pedantic`,
`test-all`, `doc-all`, and `deny-all`.

Prefer narrow verification commands scoped to what changed.
Do not claim source or test coverage for code that does not exist.

## Verification baseline

Default verification is impact-scoped. Run the narrowest checks that exercise
what changed, then widen when evidence is insufficient.

Widen static checks when:

- public APIs or correctness invariants change;
- package policy, dependencies, or report contracts change;
- preparing a release;
- narrow checks cannot demonstrate the slice gate.

Only claim a command passed if it was run and its output was checked. Record
the exact commands, results, and any unverified gaps. A passing command is
evidence for its actual scope, not for unrelated roadmap criteria.

## Feature and bug workflow

For a feature:

1. Identify the current slice and its gate.
2. Write or approve a bounded plan.
3. Add the smallest failing test or hand-calculated fixture that expresses the
   contract when practical.
4. Implement only enough to satisfy the slice.
5. Run impact-scoped verification.
6. Review correctness, reproducibility, and documentation separately.
7. Demonstrate the gate before expanding scope.

For a bug:

1. Reproduce or localize the root cause.
2. Add a regression test or fixture when practical.
3. Apply the smallest fix.
4. Verify the original failure and relevant neighboring invariants.

## Working in slices

Stay small. Prefer one bounded slice with an explicit stop condition:

1. State the slice and its stop condition before coding.
2. Implement the smallest change that meets the gate.
3. Prove correctness and reproducibility with tests or fixtures when useful.
4. Record what ran, what passed, and what remains open.
5. Pause before scope expansion, merge, publication, or any irreversible
   action. After an intermediate gate, **pause and re-decide**; after the
   terminal report gate, stop with no next slice.

Close a slice only from reviewable evidence. Prefer several small changes over
one long march through the roadmap.

## Multi-agent (Kira)

This repo is registered with `kira-mux` as project id `range-replay`
(machine config: `~/.config/kira-mux/projects/range-replay.toml`). Default
agents: `claude`, `codex`, `grok` (allow-all).

Do **not** maintain a repo-local Kira skill. CLI recipes live in the tool:

```bash
kira-mux examples
kira-mux status range-replay   # or `.` from this repo
kira-mux agents list range-replay
kira-mux send range-replay codex "…"
kira-mux capture range-replay codex --lines 80
```

When coordinating work through Kira:

1. One bounded, operator-approved slice at a time.
2. Independent review axes when using multiple agents.
3. Capture evidence; do not invent results from pane liveness alone.
4. Operator remains the gate for merge, publish, and irreversible actions.
5. All Rust work still loads `.agents/skills/rust-strict/SKILL.md`.

## Research and public claims

- State the objective, applicability, hardware, and limitations of every
  comparative result.
- Do not turn estimates into measurements through presentation wording.
- Keep generated benchmark results out of source control unless they are small,
  intentional, reproducible release artifacts.

## Completion checklist

Before claiming a change complete, confirm that:

- it belongs to the active slice (not an unrequested later exploration);
- no excluded surface or unnecessary crate split was introduced;
- correctness invariants still hold;
- deterministic and provenance requirements are covered;
- public items and behavior changes are documented;
- relevant format, lint, test, rustdoc, and dependency-policy checks were run;
- exact verification evidence and gaps are reported;
- unrelated worktree changes were left untouched;
- the next gate is explicit, or the terminal gate is recorded with no next
  slice.
