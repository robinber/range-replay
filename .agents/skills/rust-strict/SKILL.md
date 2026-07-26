---
name: rust-strict
description: Use when the task involves Rust codebases, Cargo manifests, Cargo workspaces, clippy/rustfmt/rustdoc/test workflows, Rust API design, error handling, technical debt drift, or release-quality verification for Rust projects.
---

# Rust Strict

Use this skill for Rust code that should be changed, reviewed, verified, or documented with release-quality discipline.

## Scope

- Rust crates, workspaces, binaries, libraries, proc-macros, build scripts, examples, benches, and tests.
- Cargo manifests, feature flags, workspace layout, toolchain policy, MSRV, CI gates, lint policy, docs, and release checks.
- Public API design, error types, panic boundaries, safety contracts, and runtime architecture.

## Activation checklist

Before any Rust action, complete the anchor pass and make the effective contract explicit:

1. Read `AGENTS.md` and this skill before editing, reviewing, debugging, or claiming verification.
2. Read the workspace policy files listed below, then identify the exact package, target, feature set, and crate boundary affected.
3. State the effective toolchain. In this repo, `rust-toolchain.toml` pins stable Rust; nightly is used only where an explicit command such as `cargo +nightly fmt` requires it.
4. Classify the change as implementation, review, docs/rustdoc, dependency/supply-chain, workspace policy, or release verification.
5. Choose the narrowest verification command that can fail for the touched behavior, and record what it does not cover.

Do not start from memory or generic Rust habits when the repository policy files answer the question.

## Repository anchoring

Before acting, inspect the workspace policy files that define the effective contract:

- `Cargo.toml` — workspace package metadata (`[workspace.package]`), workspace lint policy (`[workspace.lints]`), and crate-level `[lints] workspace = true` inheritance.
- `rust-toolchain.toml` — pinned toolchain channel, required components, and profile.
- `.rustfmt.toml` — edition 2024 formatting baseline with import grouping.
- `clippy.toml` — MSRV, doc-valid-idents, and threshold knobs.
- `.cargo/config.toml` — canonical cargo aliases.
- `deny.toml` — dependency advisory, license, and source policy.
- `.github/workflows/ci.yml` — the CI lane: one job per reference quality
  gate (fmt, clippy, test, doc, deny), with `--locked` builds, on pull
  requests and pushes to `main`.

Anchor on those files first, then before editing:

1. Identify the crate or crates actually involved.
2. Determine whether the change is public API, runtime behavior, tests/examples, or private glue.
3. Confirm MSRV and toolchain constraints.
4. Find the repo's lint baseline and CI commands.
5. Decide which verification command is the cheapest meaningful check.

If the request is ambiguous, resolve it by reading the current code and manifests instead of guessing the intended architecture.

## Normative sources

Treat the official Rust ecosystem guidance as the default technical baseline:

- Cargo semantics and workspace behavior.
- rustfmt formatting rules.
- Clippy diagnostics and lint intent.
- rustdoc documentation conventions.
- Rust API Guidelines for public design.

Repository policy, lint configuration, CI, and release process define the effective project contract. Follow them unless they conflict with the user's requested outcome or would normalize a correctness, safety, or compatibility regression.

If a Cargo, Clippy, rustfmt, rustdoc, or public-API fact is uncertain or version-sensitive, verify it against official documentation instead of relying on memory.

## Strictness profile

- Be strict on runtime code, public APIs, error semantics, docs, safety contracts, dependency changes, and final verification.
- Treat `-D warnings`, rustdoc warnings, `cargo deny`, workspace lint inheritance, and no hidden panics in production code as the quality floor.
- Be pragmatic on tests, examples, benches, and private glue when extra ceremony would not improve signal.
- Do not impose a generic Rust preference where the repository already has a documented and enforced policy.
- Do not make nightly the default toolchain unless the repository owner deliberately changes `rust-toolchain.toml`; use `+nightly` only for commands that explicitly need it.

## Operating model

- Prefer the smallest change that satisfies the request.
- Separate discovery, implementation, and verification.
- Keep iteration scope narrow until there is evidence the change needs to widen.
- When the change touches shared behavior, verify the behavior directly instead of relying on inference.
- For final confirmation, widen verification only as far as the change scope justifies.

## Drift control gates

Rust work must leave the touched surface no worse than it was.

Before editing non-trivial runtime code:

1. Check target file size and local module shape.
2. Search for existing helpers, duplicate logic, and related tests before adding new code.
3. If the task, issue, plan, or an active debt tracker references audit findings for the touched scope, read only the relevant findings and do not expand known debt without an explicit reason.

Hard gates:

- Do not grow a file already over 1,000 lines for feature work unless the change is a minimal bug fix, test-only addition, or an approved transitional step. Extract or split first when the requested change would add another responsibility.
- Treat files over 800 lines as pressure zones: keep additions narrow, avoid new responsibilities, and prefer moving cohesive helpers into focused modules.
- Do not add parameters to functions that already have 6 or more parameters; introduce a request, context, or options type unless there is a documented reason not to.
- Do not add a third copy of parsing, formatting, validation, config, path, timestamp, retry, or error-mapping logic. Extract a shared helper or justify why the behaviors must diverge.
- Do not add broad `#[allow]` attributes. Every new allowance needs the smallest scope, `reason = "..."`, and a cleanup path if it is temporary.
- If touching untested critical logic, add a focused test or state why the gap remains and which command gives the best available coverage.

For more detail, load `references/drift-control.md` when a task touches large files, active audit findings, duplicated logic, broad suppressions, public APIs, command dispatch, runtime orchestration, config resolution, or test gaps.

## Verification policy

Default verification is impact-scoped. Use the narrowest command that can fail for the change, then widen only when the touched surface justifies it. The full static baseline is reserved for releases, workspace policy changes, dependency-policy changes, broad public API changes, cross-crate behavior, or explicit operator request:

```bash
cargo +nightly fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo deny check advisories licenses sources
```

Tests remain impact-scoped for lint, documentation, and workspace-policy changes, including before a push. Run `cargo test --workspace --all-features` only when the operator explicitly asks for it or when a cross-cutting runtime/shared-contract change cannot be covered by narrower tests.

Repository CI may split these gates into narrower jobs, so check the workflow files before matching a CI lane exactly.

During iteration, prefer the narrowest useful command: `cargo check -p crate`, a focused `cargo test`, a single package, or a targeted doc build. Use cargo aliases (`lint-app`, `lint`, `doc-all`, `test-all`) for convenience. Do not report an ambiguous `cargo test` result as broad proof; state the exact package/workspace selection, target, feature set, and remaining gap.

- Use `cargo +nightly fmt --all --check` when formatting is part of the change or when the repo expects formatting gates.
- Use `cargo clippy` when the change affects logic, API shape, unsafe code, concurrency, or lint-sensitive code.
- Use `RUSTDOCFLAGS="-D warnings" cargo doc-all` when public docs or rustdoc examples changed.
- Use `cargo test` when behavior changed or the request explicitly involves tests.
- In workspaces, remember that `cargo test` from the root may only cover default members. Use `-p`, `--workspace`, or explicit package selection when coverage needs to include non-default members.
- Treat doctests as required when public docs or public examples changed.
- Before finalizing, run the smallest set of commands that convincingly covers the touched surface.
- Escalate to `--workspace --all-targets --all-features` only when the change spans shared crates, workspace policy, lint/toolchain config, feature plumbing, public APIs shared across crates, or release-critical paths.
- Final reports must name every verification command actually run, whether it passed, and what was intentionally not run.

## Lint policy

Lints are managed at the workspace level in `Cargo.toml` `[workspace.lints]` and inherited by crates via `[lints] workspace = true`. Additional Clippy configuration lives in `clippy.toml`.

- Treat `cargo clippy ... -- -D warnings`, rustdoc `-D warnings`, and the
  repository's explicit workspace lint denials as the safe strict baseline.
- Every new workspace member must inherit `[workspace.lints]` with `[lints] workspace = true` unless the operator approves a crate-local exception.
- This repository explicitly enables `clippy::pedantic` workspace-wide. Do not
  weaken it, and do not enable `clippy::nursery` or `clippy::restriction` as a
  group. Trial additional restriction lints individually before ratcheting
  them into workspace policy.
- Read the repo's manifest lint policy, `clippy.toml`, and existing CI before introducing new lint rules.
- Respect repo-specific lint settings when present, but do not let local history override a reasonable Rust baseline.
- Fix the root cause of a lint instead of suppressing it unless the suppression is narrowly justified and documented.
- Avoid broad `allow` attributes. New non-test suppressions need the smallest scope, `reason = "..."`, and a cleanup path if temporary; touching an existing suppression must not broaden it silently.

Lint ratchet: lints are tightened deliberately at the workspace level. Enforce new lint policy in runtime code first, then widen only when the resulting signal is understood.

## Deviation handling

Repository-specific deviations from the shared baseline must be explicit and documented. When a deviation exists, justify the reason and whether it is temporary or permanent. Deviations not documented are not permitted by omission.

## Public API rules

- Keep public items stable in shape, naming, and semantics unless the task explicitly asks for a breaking change.
- Prefer explicit types, explicit ownership boundaries, and predictable trait bounds.
- Make fallibility visible in the type system when the error is recoverable.
- Avoid leaking implementation details through public signatures.
- If a change alters a public contract, update the docs and tests that describe that contract.

## Public trait guidance

For public traits in libraries and shared crates, do not default to public `async fn`. Prefer:

- methods returning `impl Future<Output = T> + Send`,
- a concrete stream alias such as `BoxStream<'static, T>`,
- object-safe traits only when dynamic dispatch is genuinely needed.

This keeps `Send` explicit at the boundary, avoids surprise auto-trait issues, and keeps port and trait layers stable for callers.

## Ownership and borrowing

- Take ownership when the function needs ownership.
- Borrow when it does not.
- Do not take `&T` and immediately clone unless there is a documented reason.
- Avoid gratuitous `Arc<Mutex<_>>`. First ask whether ownership transfer or message passing is cleaner.

## Error handling rules

- Use typed errors where the caller can act on them.
- Keep error enums small and domain-oriented.
- Preserve source errors when context matters.
- Do not use `unwrap`, `expect`, `panic!`, `todo!`, or `unimplemented!` in production code (non-test, non-example, non-bench). Use typed errors and `?` propagation to surface fallibility.
- In binaries and top-level orchestration, convert errors at the boundary and emit actionable context.

## Documentation rules

- Public items should have rustdoc. In libraries and shared crates, treat undocumented public API as a bug unless the repository has explicitly chosen a narrower documentation policy.
- Repository policy requires `RUSTDOCFLAGS="-D warnings"`, and the CI `doc`
  job enforces it. Broken intra-doc links, bare URLs, and other rustdoc
  warnings are build failures under the canonical documentation gate.
- Include examples when the example clarifies usage or edge cases.
- Add `Errors`, `Panics`, and `Safety` sections when they are relevant.
- Document invariants, preconditions, feature flags, and lifetimes when callers need them.
- Keep examples compileable and aligned with the current API.
- For internal glue, keep docs brief unless the code is subtle.

## Runtime architecture rules

- Keep `main.rs` thin. Put logic in testable modules.
- Move branching behavior, parsing, and business logic out of the entrypoint.
- Keep shared state explicit and bounded.
- Prefer small, composable modules with narrow responsibilities.
- Make concurrency, blocking, and allocation decisions visible near the boundary where they matter.
- Separate pure logic from I/O so tests stay focused and cheap.

## Async rules

- Keep async at the boundaries that actually need it.
- Avoid blocking calls inside async tasks.
- Make cancellation behavior explicit.
- Treat spawned tasks as owned resources. Someone must supervise them.
- Prefer bounded queues and explicit backpressure over unbounded buffering.
- Add tracing spans to long-running or externally visible async work.

## Logging and observability

- Use `tracing`, not ad hoc `println!` or `eprintln!`.
- Prefer structured fields over interpolated strings.
- Log state transitions and external calls.
- Never log secrets, raw tokens, or full unredacted configs.
- For streaming systems, capture timing at step boundaries.

## Tests, examples, and private glue

- Be pragmatic here. Use the least ceremony that gives confidence.
- Tests and examples may trade strictness for clarity when the public contract is already covered elsewhere.
- Private glue can be simpler and more direct, but it still should not become unmaintainable or hide failures.
- Avoid over-architecting helper code that is only used in tests.
- Still keep failure messages and assertions useful.

## Rust-specific checks

- Confirm feature flags, default features, and workspace member selection when they affect the result.
- Watch for edition differences, MSRV-sensitive APIs, and `no_std` or `alloc` boundaries.
- Check doctests when public examples are added or changed.
- Check proc-macro and build-script behavior separately from the main library or binary when needed.

## Dependency rules

- Treat `Cargo.toml`, `Cargo.lock`, and `deny.toml` edits as supply-chain changes.
- Prefer workspace dependency declarations and explain whether a new dependency is direct, transitive-only, build-time, runtime, or test-only.
- Keep the dependency graph lean; avoid large framework additions for small problems.
- Avoid wildcard versions, unreviewed git dependencies, unnecessary default features, and broad feature enables.
- Do not relax `deny.toml` policy without an explicit rationale and a narrower alternative considered first.
- Run `cargo deny check advisories licenses sources` after dependency or dependency-policy changes; include `bans` too when version duplication or wildcard policy is part of the change.

## Review checklist

Before considering a Rust change complete, confirm:

- Are the public docs still accurate?
- Are trait boundaries still clean?
- Is the new code free of hidden panics?
- Is ownership clearer, not muddier?
- Are errors typed and contextual?
- Are logs structured and secret-safe?
- Does every new crate inherit workspace lints and workspace package policy?
- Is the verification evidence exact about package, target, feature set, and gaps?

## Reference files

Load these when you need more detail than this skill should carry:

- `references/workflow.md` for the anchor pass, verification scoping, and CI alignment.
- `references/testing.md` for unit, integration, and doctest expectations.
- `references/lints.md` for manifest lint policy, Clippy groups, and suppressions.
- `references/docs.md` for rustdoc expectations and public examples.
- `references/api-design.md` for public API shape, constructors, and naming.
- `references/errors.md` for recoverable errors, panic boundaries, and fallible construction.
- `references/cli-systems.md` for thin `main.rs` boundaries, exit codes, and CLI/runtime separation.
- `references/drift-control.md` for no-net-new-debt gates, large-file pressure, duplication checks, and audit-aware edits.

Keep this file short. Put deep, stable reference material in the files above rather than expanding this skill body.
