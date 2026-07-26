# workflow

## 1. Source-backed guidance

- Start from the policy files that exist: `Cargo.toml` (package and/or workspace tables), `rust-toolchain.toml`, `.rustfmt.toml`, `clippy.toml`, `.cargo/config.toml`, and `deny.toml`. CI workflow files join the effective contract once they exist.
- Treat `rust-version` in package or `[workspace.package]` metadata as the MSRV declaration. Update it deliberately and verify it against the selected toolchain.
- Treat `rust-toolchain.toml` as the default toolchain contract. Do not infer nightly by default; use `+nightly` only for commands that explicitly require it (for example rustfmt options that need nightly).
- Run `rustfmt` before broad verification so the diff reflects behavior, not formatting drift.
- Check `.github/workflows/*` before widening scope when CI exists. Until then, use `AGENTS.md` and local policy files as the canonical gates and do not claim CI coverage.
- Verify from the smallest relevant scope first: one package, one test target, one feature set. Escalate only when the change affects shared code, feature gates, build scripts, or cross-package behavior.

## 2. Local verification baseline

Default verification is impact-scoped. The full static baseline is:

```bash
cargo +nightly fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
cargo deny check advisories licenses sources
```

In a workspace, add `--workspace` (or explicit `-p` selection) when the change spans members or shared policy. During iteration, package-scoped commands are preferred.

Cargo aliases in `.cargo/config.toml`, when present, provide shorthand such as `lint`, `lint-app`, `lint-pedantic`, `doc-all`, `deny-all`, and `test-all`.

Tests remain impact-scoped for lint, documentation, and policy-only changes. For a behavior change claimed complete, run tests that exercise the touched behavior. Run the full package or workspace suite when the operator asks or when a cross-cutting contract cannot be covered narrowly.

When reporting verification, copy the exact command shape and scope: package, workspace/member selection, target (`--lib`, `--bin`, tests), feature set, and whether doctests or dependency-policy checks were included.

## 3. Manifest edit classes

Not every `Cargo.toml` edit is the same. Classify before choosing gates:

| Edit class | Examples | Minimum verification |
|---|---|---|
| Metadata-only | description, readme, authors, keywords | `cargo metadata --no-deps --format-version 1 --locked` (or equivalent parse) + `cargo +nightly fmt --all --check` if any Rust/fmt files also changed |
| Lint / toolchain / feature policy | `[lints]`, features, `rust-version`, profile policy | full static baseline for the affected package/workspace selection |
| Dependencies / supply chain | new deps, version bumps, `deny.toml` | full static baseline + `cargo deny check advisories licenses sources` (and `bans` when relevant) |
| Code-adjacent package wiring | new targets, `[[bin]]`, path deps | clippy + tests for the affected targets |

When in doubt between metadata-only and policy, use the stricter class.

## 4. Skill policy

- Always do an anchor pass over policy files and existing CI before editing.
- Prefer the narrowest command that can fail for the change you made.
- Escalate in this order when needed: package scope, `--all-targets`, `--all-features`, then workspace-wide selection.
- Treat the full static baseline as mandatory for lint, feature, toolchain, dependency-policy, and shared-package changes — not for pure metadata renames.
- Keep MSRV, lint policy, deny policy, and CI expectations aligned; if one changes, check the others.

## 5. Allowed exceptions

- If the change is pure formatting, `rustfmt --check` is enough unless CI policy says otherwise.
- If the workspace is very large, first verify the affected package and direct dependents, then widen only if the change crosses package boundaries.
- For documentation-only edits, you may skip full test execution unless doctests or public API examples changed.
- If CI is the authoritative gate for a slow target, a local narrower check is acceptable as long as you clearly note the remaining gap.
- If no `deny.toml` exists, skip `cargo deny` and note the gap rather than inventing policy.
