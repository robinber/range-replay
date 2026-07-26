# testing

## 1. Source-backed guidance
- The Rust Book distinguishes unit tests, integration tests, and doctests; each catches different failure modes. See [Testing](https://doc.rust-lang.org/book/ch11-00-testing.html) and [Test Organization](https://doc.rust-lang.org/book/ch11-03-test-organization.html).
- `cargo test` builds and runs tests for the selected package or workspace selection; doctests are included for library docs and public examples. See the Cargo Book on [`cargo test`](https://doc.rust-lang.org/cargo/commands/cargo-test.html).
- In a workspace, `cargo test` without explicit `-p`, `--workspace`, target, or feature flags can be narrower than expected. Verification evidence must state the exact selection used.
- Keep logic out of `main` when it matters for correctness. Move behavior into functions or modules so unit tests can call it directly.
- Put cross-module behavior in integration tests, and keep doctest examples in sync with the public API they describe.
- When public docs or examples change, rerun doctests intentionally; examples that compile in the docs should still compile after the edit.

## 2. Skill policy
- Use unit tests for small, isolated behavior inside one crate.
- Use integration tests for public APIs, crate boundaries, and end-to-end behavior inside a package.
- Use doctests for user-facing examples, API usage, and invariants that should stay visible in docs.
- Prefer testing behavior through stable functions and modules, not through `main` or ad hoc setup code.
- When scope is unclear, start with the smallest test that exercises the behavior, then widen only if the bug can escape that boundary.
- Prefer explicit commands such as
  `cargo test -p moe-sim-core <test-filter> --all-features`; avoid saying
  "tests pass" without package, target, feature, and doctest scope.
- Prefer deterministic tests. Avoid `sleep` when coordination primitives (channels, notifications, atomic flags) are available.
- For adapter code, mock the transport layer or isolate request/response mapping logic rather than hitting live services.
- Keep the suite `cargo nextest`-friendly: no shared mutable global state, no ordering dependencies between tests.

## 3. Allowed exceptions
- Generated code, thin binary entrypoints, and trivial glue may not need dedicated tests if they are fully covered by the code they delegate to.
- A doctest can be intentionally skipped or hidden only when the example is about setup rather than the public teaching point.
- Some integration scenarios belong in a higher-level harness instead of `cargo test` if they require external services or slow fixtures.
- If a package has intentionally non-testable platform glue, document the limitation rather than forcing awkward test hooks.
- If a focused command is chosen instead of the full workspace suite, state the uncovered surface explicitly in the final report.
