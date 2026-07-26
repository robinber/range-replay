# testing

## 1. Source-backed guidance

- The Rust Book distinguishes unit tests, integration tests, and doctests; each catches different failure modes. See [Testing](https://doc.rust-lang.org/book/ch11-00-testing.html) and [Test Organization](https://doc.rust-lang.org/book/ch11-03-test-organization.html).
- `cargo test` builds and runs tests for the selected package or workspace selection; doctests are included for library docs and public examples. See the Cargo Book on [`cargo test`](https://doc.rust-lang.org/cargo/commands/cargo-test.html).
- In a workspace, `cargo test` without explicit `-p`, `--workspace`, target, or feature flags can be narrower than expected. Verification evidence must state the exact selection used.
- Keep logic out of `main` when it matters for correctness. Move behavior into functions or modules so unit tests can call it directly.
- Prefer deterministic tests. Avoid `sleep` when coordination primitives (channels, notifications, atomic flags) are available.
- Keep the suite `cargo nextest`-friendly: no shared mutable global state, no ordering dependencies between tests.

## 2. Skill policy

- Use unit tests for small, isolated behavior inside one module or package.
- Use integration tests for public APIs and end-to-end behavior inside a package.
- Use doctests for user-facing examples and invariants that should stay visible in docs.
- Prefer testing behavior through stable functions and modules, not through `main` or ad hoc process setup, unless the CLI boundary is the subject under test.
- Prefer explicit commands such as `cargo test -p <package> <test-filter> --all-features`; avoid saying "tests pass" without package, target, feature, and doctest scope.

### Fixtures and oracle style

- Prefer hand-calculated examples for domain invariants (plans, coalescing, budgets, checksums).
- Commit small deterministic fixtures when they document the contract; keep large datasets out of source control.
- Name fixtures after the behavior they prove, including invalid and adversarial cases.

### Property tests

- Use property testing (`proptest` or similar) for pure transformations with a large input space when a few examples are not enough: parsers, coalescing, arithmetic bounds, round-trips.
- Keep generators realistic; assert properties, not a second copy of the implementation.
- Combine property tests with a few fixed regression cases for known edge bugs.

### When to widen testing tools

| Situation | Tooling to consider |
|---|---|
| Pure logic with many edge combinations | table-driven tests, then proptest |
| `unsafe` or raw pointer logic that Miri supports | `cargo +nightly miri test` on the focused target |
| Suspected weak assertions | mutation testing (`cargo mutants`) as an occasional audit, not a default gate |
| Slow or flaky suite | nextest isolation and explicit timeouts |

Do not add heavy testing infrastructure by default. Escalate when the risk of the touched surface justifies it.

## 3. Allowed exceptions

- Generated code, thin binary entrypoints, and trivial glue may not need dedicated tests if they are fully covered by the code they delegate to.
- A doctest can be intentionally skipped or hidden only when the example is about setup rather than the public teaching point.
- Some integration scenarios belong in a higher-level harness instead of `cargo test` if they require external services, privileged devices, or slow machine-specific fixtures.
- If a package has intentionally non-testable platform glue, document the limitation rather than forcing awkward test hooks.
- If a focused command is chosen instead of the full suite, state the uncovered surface explicitly in the final report.
