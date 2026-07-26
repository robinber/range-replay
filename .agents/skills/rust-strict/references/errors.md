# errors

## 1. Source-backed guidance

- The Rust Book treats `Result<T, E>` as the default for recoverable errors and `panic!` as the tool for unrecoverable failures or bugs.
- Prefer typed, composable library errors when the caller may want to match, enrich, or recover from them.
- Use `?` as the normal propagation path in library and application code.
- Document error behavior on public functions, including panic conditions when they exist.
- At binary boundaries, `anyhow::Result` or `Box<dyn Error + Send + Sync>` is acceptable when the goal is to report a user-facing failure and exit cleanly.

## 2. Skill policy

- Library code should usually return a concrete error enum or another composable error type (`thiserror` is the default preference), not erase errors too early.
- In non-test production code, do not use `unwrap`, `expect`, `panic!`, `todo!`, or `unimplemented!`. Use typed errors and `?`.
- Prefer `build` / `try_new` / `TryFrom` for fallible construction over a fallible `new` when designing new APIs (house style; std sometimes uses fallible `new`).
- Make fallibility obvious in the API name and docs.
- Prefer `?` over manual propagation unless you need to attach context or translate the error.
- Keep validation at the boundary: construct valid values with fallible constructors, then let the rest of the code assume invariants hold.
- Preserve `source` / `#[from]` relationships so diagnostics keep the underlying cause.
- Map errors to actionable messages and stable exit codes at the CLI boundary.

### Panic contract

- Prefer making illegal states unrepresentable over runtime panics.
- When repository policy bans panics in non-test code (as this repo's `AGENTS.md` does), that ban wins over general Rust culture. Do not introduce invariant panics without an explicit operator-approved exception.
- If an approved exception allows an internal invariant panic, document `# Panics`, keep the surface small, and treat it as a bug path—not control flow.
- Do not use panics for user input, I/O failure, parse failure, or policy rejection.
- Prefer enabling `clippy::panic` (and related unwrap/expect lints) so the ban is mechanical, not review-only.
- Test-only panics/unwraps require Clippy test knobs or scoped expects; see `references/lints.md`.

## 3. Allowed exceptions

- Tests may use `panic!` or `expect` when failure should stop immediately **and** package lint knobs allow it.
- A CLI or TUI `main` may use `anyhow` or `Box<dyn Error>` to collapse diverse failures into one exit path.
- A fallible `new` can be tolerated in existing code, but it should not be the preferred shape for new APIs.
