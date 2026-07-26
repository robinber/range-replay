# errors

## 1. Source-backed guidance
- The Rust Book treats `Result<T, E>` as the default for recoverable errors and `panic!` as the tool for unrecoverable failures or bugs.
- Prefer typed, composable library errors when the caller may want to match, enrich, or recover from them.
- Use `?` as the normal propagation path in library and application code.
- Document error behavior on public functions, including panic conditions when they exist.
- For constructors that can fail validation, prefer `build`/`try_new`-style APIs or `TryFrom` over a fallible `new`.
- At binary boundaries, `anyhow::Result` or `Box<dyn Error>` is acceptable when the goal is to report a user-facing failure and exit cleanly.

## 2. Skill policy
- Library code should usually return a concrete error enum or another composable error type, not erase errors too early.
- Do not use `unwrap`, `expect`, `panic!`, `todo!`, or `unimplemented!` in production code (non-test, non-example, non-bench). Use typed errors and `?` propagation to surface fallibility.
- Make fallibility obvious in the API name and docs.
- Prefer `?` over manual propagation unless you need to attach context or translate the error.
- Keep validation at the boundary: construct valid values with `build`/`try_new`, then let the rest of the code assume invariants hold.

## 3. Allowed exceptions
- Tests, examples, and invariant-checking code may use `panic!` or `expect` when failure should stop immediately.
- A CLI or TUI `main` may use `anyhow` or `Box<dyn Error>` to collapse diverse failures into one exit path.
- In `moe-sim`, do not introduce `panic!` in non-test code unless an existing,
  explicit repository deviation already covers that boundary or the operator
  specifically approves the deviation.
- A fallible `new` can be tolerated in existing code, but it should not be the preferred shape for new APIs.
