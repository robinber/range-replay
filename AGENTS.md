# AGENTS.md

Machine-facing development guidance for coding agents working in this
repository. Humans should start with [`README.md`](README.md); this file exists
to make the engineering and verification contracts explicit.

## Load order

1. This file — repository-wide agent rules.
2. [`README.md`](README.md) — product scope, correctness boundary, and non-goals.
3. [`.agents/skills/rust-strict/SKILL.md`](.agents/skills/rust-strict/SKILL.md)
   — required before changing, reviewing, debugging, or claiming verification
   for Rust or Cargo work.
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
    lib.rs     library surface (planning, validation, backends later)
    main.rs    thin binary entrypoint
  ```

- `Cargo.lock` is committed because this package builds an application.
- Do not introduce a multi-crate workspace until there is a demonstrated need
  (isolated dependencies, multiple binaries with different graphs, or a clear
  ownership boundary with more than one consumer).

Do not describe planned commands, modules, formats, or results as implemented.

## Working rules

- This repository is an **educational, bounded** project. Prefer a finished
  narrow slice over expanding scope. Stopping after `v0.1` is a valid success.
- Make the smallest change that satisfies the approved request.
- Work one bounded slice at a time and satisfy its gate before starting another.
- Do **not** auto-chain into the next milestone. After a gate, the operator
  chooses continue / side quest / pause.
- Follow existing module boundaries before introducing new abstractions.
- Work test-first when practical. For bugs, reproduce or localize the root
  cause before editing.
- Match the style of the file being edited.
- Mention unrelated drift and leave it untouched.
- Do not commit secrets, gated datasets, large generated results,
  machine-specific profiles, or private agent/runtime state.
- Self-check every design: if a senior engineer would call it overcomplicated,
  simplify it before claiming completion.

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

`io_uring` and other Linux-specific backends will eventually need carefully
scoped `unsafe`. That is an explicit later decision: introduce it only behind a
documented safety boundary, with the smallest possible surface, and never as a
blanket policy relaxation.

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

The package is still taking shape. Until modules exist, prefer:

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

## Deferred surfaces

Until an explicit project decision opens them, do not add:

- macOS or Windows backends;
- a reusable async runtime;
- GPU execution or model inference;
- integration into `moe-sim`;
- multi-device or distributed I/O;
- `O_DIRECT` or portable cold-cache guarantees;
- claims of being the fastest possible reader.

These are deliberate non-goals for the first release, not missing scaffolding
to create in advance.

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
   action. Default after a closed gate: **pause and re-decide**.

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
- no deferred surface or unnecessary crate split was introduced;
- correctness invariants still hold;
- deterministic and provenance requirements are covered;
- public items and behavior changes are documented;
- relevant format, lint, test, rustdoc, and dependency-policy checks were run;
- exact verification evidence and gaps are reported;
- unrelated worktree changes were left untouched;
- the next gate, not merely the implementation task, is explicit.
