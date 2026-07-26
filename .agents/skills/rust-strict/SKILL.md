---
name: rust-strict
description: Use when the task involves Rust codebases, Cargo manifests, Cargo workspaces, clippy/rustfmt/rustdoc/test workflows, Rust API design, error handling, unsafe boundaries, technical debt drift, or release-quality verification for Rust projects.
---

# Rust Strict

Use this skill for Rust code that should be changed, reviewed, verified, or documented with release-quality discipline.

## Scope

- Rust packages, workspaces, binaries, libraries, proc-macros, build scripts, examples, benches, and tests.
- Cargo manifests, feature flags, layout, toolchain policy, MSRV, lint policy, docs, and release checks.
- Public API design, error types, panic boundaries, `unsafe` safety contracts, and runtime architecture.

## Activation checklist

Before any Rust action, complete the anchor pass and make the effective contract explicit:

1. Read `AGENTS.md` (if present) and this skill before editing, reviewing, debugging, or claiming verification.
2. Read the package or workspace policy files listed below, then identify the exact package, target, feature set, and boundary affected.
3. State the effective toolchain. Prefer `rust-toolchain.toml` when present; use `+nightly` only for commands that explicitly need it (for example this repo's rustfmt options).
4. Classify the change as implementation, review, docs/rustdoc, dependency/supply-chain, package/workspace policy, or release verification.
5. Choose the narrowest verification command that can fail for the touched behavior, and record what it does not cover.

Do not start from memory or generic Rust habits when the repository policy files answer the question.

## Repository anchoring

Before acting, inspect the policy files that exist and define the effective contract:

- `Cargo.toml` — package metadata and/or `[workspace.package]` / `[workspace.lints]`, plus local `[lints]` tables.
- `rust-toolchain.toml` — pinned toolchain channel, required components, and profile.
- `.rustfmt.toml` — formatting baseline.
- `clippy.toml` — MSRV, doc-valid-idents, test-allow knobs, and thresholds.
- `.cargo/config.toml` — cargo aliases when present.
- `deny.toml` — dependency advisory, license, and source policy.
- `.github/workflows/*` — CI gates when present. If absent, use `AGENTS.md` and local policy files as the canonical gates and do not claim CI coverage.

Anchor on those files first, then before editing:

1. Identify the package(s) actually involved (single-package repo or workspace members).
2. Determine whether the change is public API, runtime behavior, tests/examples, or private glue.
3. Confirm MSRV and toolchain constraints.
4. Find the repo's lint baseline and verification commands.
5. Decide which verification command is the cheapest meaningful check.

If the request is ambiguous, resolve it by reading the current code and manifests instead of guessing the intended architecture.

## Normative sources

Treat official and high-quality Rust ecosystem guidance as the default technical baseline:

- Cargo semantics and workspace behavior.
- rustfmt, Clippy, and rustdoc documentation.
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/checklist.html) for public design.
- [The Rustonomicon](https://doc.rust-lang.org/nomicon/) and the [Unsafe Code Guidelines](https://rust-lang.github.io/unsafe-code-guidelines/) when writing or reviewing `unsafe`.
- [Microsoft Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/) for scalable application/library design when the change is non-trivial.

Repository policy (`AGENTS.md`, manifests, lint config, CI) is the effective project contract. If it conflicts with a user request, surface the conflict and get an explicit decision. Do not silently prefer either side, and do not normalize a correctness, safety, or compatibility regression.

If a Cargo, Clippy, rustfmt, rustdoc, public-API, or safety fact is uncertain or version-sensitive, verify it against official documentation instead of relying on memory.

## Strictness profile

- Be strict on runtime code, public APIs, error semantics, docs, safety contracts, dependency changes, and final verification.
- Treat `-D warnings`, rustdoc warnings, `cargo deny` (when configured), lint policy inheritance, and no hidden panics in non-test code as the quality floor.
- Be pragmatic on tests, examples, benches, and private glue when extra ceremony would not improve signal.
- Do not impose a generic Rust preference where the repository already has a documented and enforced policy.
- Do not make nightly the default toolchain unless the owner deliberately changes `rust-toolchain.toml`.

## Operating model

- Prefer the smallest change that satisfies the request.
- Separate discovery, implementation, and verification.
- Keep iteration scope narrow until there is evidence the change needs to widen.
- When the change touches shared behavior, verify the behavior directly instead of relying on inference.
- For final confirmation, widen verification only as far as the change scope justifies.
- Do not introduce multi-crate workspaces, plugins, or heavy frameworks without a demonstrated need.

## Drift control gates

Rust work must leave the touched surface no worse than it was.

Before editing non-trivial runtime code:

1. Check target file size and local module shape.
2. Search for existing helpers, duplicate logic, and related tests before adding new code.
3. If the task, issue, plan, or an active debt tracker references audit findings for the touched scope, read only the relevant findings and do not expand known debt without an explicit reason.

Hard gates:

- Do not grow a file already over 1,000 lines for feature work unless the change is a minimal bug fix, test-only addition, or an approved transitional step. Extract or split first when the requested change would add another responsibility.
- Treat files over 800 lines as pressure zones: keep additions narrow, avoid new responsibilities, and prefer moving cohesive helpers into focused modules.
- A change must not push a function past **six** parameters without introducing a request, context, or options type (unless a documented exception already exists). The Clippy `too-many-arguments` threshold is a looser mechanical backstop, not the skill gate.
- Do not add a third copy of parsing, formatting, validation, config, path, timestamp, retry, or error-mapping logic. Extract a shared helper or justify why the behaviors must diverge.
- Do not add broad `#[allow]` attributes. Every new allowance needs the smallest scope, `reason = "..."`, and a cleanup path if it is temporary.
- If touching untested critical logic, add a focused test or state why the gap remains and which command gives the best available coverage.

## Verification policy

Default verification is impact-scoped. Use the narrowest command that can fail for the change, then widen only when the touched surface justifies it.

Full static baseline (adapt `--workspace` only when a workspace exists):

```bash
cargo +nightly fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
cargo deny check advisories licenses sources
```

In a workspace, prefer the package-scoped form during iteration and the workspace form for shared policy, cross-package, or release verification.

Tests remain impact-scoped for lint, documentation, and policy-only changes. For a behavior change claimed complete, run tests that exercise the touched behavior. Run the full package or workspace suite when the operator asks or when a cross-cutting contract cannot be covered narrowly.

When reporting verification, name every command actually run, whether it passed, the exact package/target/feature scope, and intentional gaps.

## Lint policy

Lints live in `Cargo.toml` (`[lints]` on a package, and/or `[workspace.lints]` inherited via `[lints] workspace = true`). Additional Clippy configuration lives in `clippy.toml` when present.

- Treat `cargo clippy ... -- -D warnings`, rustdoc `-D warnings`, and the repository's explicit lint denials as the safe strict baseline.
- Every new workspace member must inherit workspace lints with `[lints] workspace = true` unless the operator approves a package-local exception.
- Prefer enabling `clippy::pedantic` deliberately when the repo chooses it. Do **not** enable `clippy::nursery` or `clippy::restriction` as a group (they contain mutually exclusive or unstable lints). Cherry-pick individual nursery/restriction lints only after measuring signal.
- Prefer mechanical enforcement of stated hard rules when Clippy has a lint for them (for example `unwrap_used`, `expect_used`, `panic`, `undocumented_unsafe_blocks`, `multiple_unsafe_ops_per_block`).
- Fix the root cause of a lint instead of suppressing it unless the suppression is narrowly justified and documented.
- Avoid broad `allow` attributes. New non-test suppressions need the smallest scope, `reason = "..."`, and a cleanup path if temporary.

Lint ratchet: tighten deliberately. Enforce new lint policy in runtime code first, then widen when the signal is understood.

## Public API rules

- Keep public items stable in shape, naming, and semantics unless the task explicitly asks for a breaking change.
- Prefer explicit types, explicit ownership boundaries, and predictable trait bounds.
- Make fallibility visible in the type system when the error is recoverable.
- Avoid leaking implementation details through public signatures.
- Prefer newtypes and enums over ambiguous `bool` or magic integers at public boundaries.
- Implement `Debug` on public types; implement common traits when they are semantically correct.
- If a change alters a public contract, update the docs and tests that describe that contract.

For public traits in libraries, do not default to public `async fn`. Prefer `impl Future<Output = T> + Send` (or a concrete stream type) so `Send` stays explicit.

## Ownership and borrowing

- Take ownership when the function needs ownership.
- Borrow when it does not.
- Do not take `&T` and immediately clone unless there is a documented reason.
- Avoid gratuitous `Arc<Mutex<_>>`. First ask whether ownership transfer or message passing is cleaner.

## Arithmetic and boundary validation

For sizes, offsets, lengths, capacities, and budgets:

- Validate at the boundary; keep internal code on established invariants.
- Prefer `checked_*` arithmetic and `TryFrom` conversions over silent truncation or narrowing `as` casts.
- Use `saturating_*` only when the saturation policy is intentional and documented (budget hard-limits must not silently clamp).
- Reject overflow, underflow, empty invalid inputs, and out-of-range values with typed errors.
- Prefer newtypes when raw integers would mix distinct units (for example file offset vs length vs budget bytes).
- Remember debug builds often panic on overflow while release may wrap unless `overflow-checks` is enabled. Do not treat a green debug test as proof that release arithmetic is safe; prefer checked math at boundaries either way.

## Error handling rules

- Use typed errors where the caller can act on them (`thiserror` in libraries).
- Keep error enums small and domain-oriented.
- Preserve source errors when context matters.
- In non-test production code, do not use `unwrap`, `expect`, `panic!`, `todo!`, or `unimplemented!`. Prefer typed errors and `?`.
- When repository policy (for example `AGENTS.md`) bans panics in non-test code, that ban wins. Do not introduce invariant panics unless the operator explicitly approves a documented exception.
- Prefer making illegal states unrepresentable over runtime panics.
- In binaries and top-level orchestration, convert errors at the boundary and emit actionable context (`anyhow` is fine at that boundary only).
- Test-only `unwrap`/`expect`/`panic!` is allowed only when Clippy/repo knobs permit it (`allow-*-in-tests` in `clippy.toml`, or scoped expects). Do not assume test code is exempt from package lints by default.

## Unsafe and safety contracts

Default to safe Rust. Introduce `unsafe` only when required and approved by the slice.

- Keep `unsafe` blocks as small as possible behind a safe abstraction.
- Every `unsafe` block needs a `// SAFETY:` comment stating the invariants that make it sound.
- Document safety preconditions on `unsafe fn` with a `# Safety` rustdoc section.
- Prefer safe encapsulation so callers cannot violate invariants.
- Prefer lints that enforce this contract when available (`undocumented_unsafe_blocks`, `multiple_unsafe_ops_per_block`).
- When `unsafe` changes, run the strongest practical checks: focused tests; Miri when the code is Miri-compatible; otherwise sanitizers / careful review for syscall or device I/O paths.

## Documentation rules

- Public items should have rustdoc. Treat undocumented public API as a bug unless the repository chose a narrower policy (`missing_docs` may make omission a build failure).
- When the repo uses `RUSTDOCFLAGS="-D warnings"`, broken intra-doc links and other rustdoc warnings are build failures.
- Include examples when they clarify usage or edge cases.
- Add `Errors`, `Panics`, and `Safety` sections when relevant.
- Keep examples compilable and aligned with the current API.

## Runtime architecture rules

- Keep `main.rs` thin. Put logic in testable modules or the library crate.
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

- Prefer `tracing` with structured fields for long-running or multi-step runtimes.
- For simple CLIs, clear messages on `stderr` (errors) and `stdout` (success output) are acceptable; do not force a tracing stack without need.
- Never log secrets, raw tokens, or full unredacted configs.

## Tests, examples, and private glue

- Be pragmatic. Use the least ceremony that gives confidence.
- Prefer deterministic, hand-calculated fixtures for domain invariants.
- Use property tests (`proptest` or similar) for pure transformations with a large input space when a few examples are not enough.
- Keep the suite free of shared mutable global state and ordering dependencies (nextest-friendly).
- Tests and examples may trade strictness for clarity when the public contract is already covered.
- Avoid over-architecting helper code that is only used in tests.

## Dependency rules

- Treat `Cargo.toml`, `Cargo.lock`, and `deny.toml` edits as supply-chain changes.
- Prefer workspace dependency declarations in multi-package repos.
- Keep the dependency graph lean; avoid large framework additions for small problems.
- Avoid wildcard versions, unreviewed git dependencies, unnecessary default features, and broad feature enables.
- Do not relax `deny.toml` policy without an explicit rationale.
- Run `cargo deny check advisories licenses sources` after dependency or dependency-policy changes; include `bans` when version duplication or wildcards matter.
- Escalate to stronger supply-chain tools (`cargo-vet`, audit notes, SBOM) only when the project opts in or the risk justifies it.

## Review checklist

Before considering a Rust change complete, confirm:

- Are the public docs still accurate?
- Are trait and ownership boundaries still clean?
- Is the new code free of hidden panics and silent truncation?
- Are errors typed and contextual?
- If `unsafe` changed, are `SAFETY` comments and tests adequate?
- Are logs or CLI messages secret-safe?
- For workspaces, does every new package inherit lint policy?
- Is the verification evidence exact about package, target, feature set, and gaps?

## Reference files

Load a reference only when the task needs that detail. Do not load every reference by default.

| Load when… | File |
|---|---|
| verification scoping, manifests, CI alignment | `references/workflow.md` |
| unit/integration/doctest, fixtures, property tests | `references/testing.md` |
| Clippy groups, suppressions, lint ratchet | `references/lints.md` |
| rustdoc gates and public examples | `references/docs.md` |
| public API shape, constructors, naming checklist | `references/api-design.md` |
| recoverable errors, panic boundaries, construction | `references/errors.md` |
| writing or reviewing `unsafe` | `references/unsafe.md` |
| thin `main`, exit codes, stdout/stderr | `references/cli-systems.md` |
| large files, duplication, debt-sensitive surfaces | `references/drift-control.md` |

Keep this file short. Put deep, stable reference material in the files above rather than expanding this skill body.
