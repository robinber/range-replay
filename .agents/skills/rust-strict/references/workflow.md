# workflow

## 1. Source-backed guidance
- Start from the workspace policy files: `Cargo.toml` (`[workspace.package]`, `[workspace.lints]`), `rust-toolchain.toml`, `.rustfmt.toml`, `clippy.toml`, `.cargo/config.toml`, and `deny.toml`. CI workflow files join the effective contract once they exist.
- Treat `rust-version` in `[workspace.package]` as the MSRV declaration. Update it deliberately and verify it against CI and the selected toolchain.
- Treat `rust-toolchain.toml` as the default toolchain contract. Do not infer nightly by default; use `+nightly` only for commands that explicitly require it, such as this repo's rustfmt configuration.
- Run `rustfmt` before broad verification so the diff reflects behavior, not formatting drift. Use the repository's `.rustfmt.toml` (edition 2024 baseline with import grouping).
- Check `.github/workflows/ci.yml` before widening scope when it exists. Until
  then, use `AGENTS.md` and the workspace policy files as the canonical gates
  and do not claim CI coverage.
- Verify from the smallest relevant scope first: one package, one test target, one feature set. Escalate to `cargo check`, then `cargo test`, then workspace-wide or feature-complete commands only when the change affects shared code, feature gates, build scripts, or cross-crate behavior.

## 2. Local verification baseline
Default verification is impact-scoped. The full static baseline is:

```bash
cargo +nightly fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo deny check advisories licenses sources
```

Cargo aliases (`.cargo/config.toml`) provide shorthand: `lint`, `lint-app`,
`lint-pedantic`, `doc-all`, `deny-all`, and `test-all`.

Tests remain impact-scoped for lint, documentation, and workspace-policy
changes. Run `cargo test --workspace --all-features` only when the operator
explicitly asks for it or a cross-cutting runtime/shared-contract change cannot
be covered by narrower tests.

No CI workflow exists yet. When one is added, keep it aligned with
`AGENTS.md`, the workspace policy files, and the roadmap's pinned-runner gates.

When reporting verification, copy the exact command shape and scope: package, workspace/member selection, target (`--lib`, `--bin`, tests), feature set, and whether doctests or dependency-policy checks were included.

## 3. Skill policy
- Always do an anchor pass over workspace policy files and existing CI before editing.
- Prefer the narrowest command that can fail for the change you made.
- Escalate in this order when needed: package scope, `--all-targets`, `--all-features`, then `--workspace`.
- Treat the full static baseline as mandatory when editing shared crates, root
  manifests, workspace dependencies, dependency policy, lint/toolchain policy,
  or feature plumbing. Keep tests impact-scoped unless the shared runtime
  behavior or operator request requires workspace-wide coverage.
- Keep MSRV, lint policy, deny policy, and CI expectations aligned; if one changes, check the others.

## 4. Allowed exceptions
- If the change is manifest-only or formatting-only, a focused manifest check plus `rustfmt` is enough unless CI policy says otherwise.
- If the workspace is very large, first verify the affected package and direct dependents, then widen only if the change crosses crate boundaries.
- For documentation-only edits, you may skip full test execution unless doctests or public API examples changed.
- If CI is the authoritative gate for a slow target, a local narrower check is acceptable as long as you clearly note the remaining gap.
