# lints

## 1. Source-backed guidance

- Cargo supports lint configuration in manifests, including workspace-level lint policy via `[workspace.lints]` inherited by packages with `[lints] workspace = true`. See the Cargo Book on [lints](https://doc.rust-lang.org/cargo/reference/lints.html).
- Single-package repositories may put the same policy under package-level `[lints]` without a workspace.
- Clippy documents lint groups and expects teams to choose stricter groups deliberately. See the Clippy docs on [lint groups](https://doc.rust-lang.org/stable/clippy/lints.html).
- `clippy::correctness` is deny-by-default for a reason; do not casually allow it.
- `clippy::pedantic` is for power users and can have intentional false positives; enabling it means accepting local `allow`/`expect` where justified.
- The `clippy::restriction` group must **not** be enabled as a whole. It contains mutually exclusive lints and trips `clippy::blanket_clippy_restriction_lints`. Cherry-pick only.
- `clippy::nursery` is experimental; cherry-pick only.

## 2. Lint structure

Layered architecture:

- **Manifest-level** (`[lints]` or `[workspace.lints]`): source of truth for lint policy.
- **Inheritance** (`[lints] workspace = true`): required for every new workspace member unless an explicit exception is approved.
- **Verification/CI-level** (`cargo clippy ... -- -D warnings`, `RUSTDOCFLAGS=-D warnings`): promotes warnings to hard failures in canonical verification and must be preserved by CI once it exists.
- **`clippy.toml`**: Clippy configuration knobs (MSRV, threshold overrides, doc-valid-idents, `allow-*-in-tests`).

### Lint rings

Lints are tightened in phases (the "ratchet" strategy):

1. **Phase 1**: Core `rust` and `rustdoc` lints (`unsafe_code` when desired, `elided_lifetimes_in_paths`, `rust_2018_idioms`, `unused_*`, intra-doc link lints).
2. **Phase 2**: Production-code Clippy denials that match stated hard rules:
   - `dbg_macro`, `expect_used`, `todo`, `unimplemented`, `unwrap_used`, `panic`
   - when `unsafe` is in scope: `undocumented_unsafe_blocks`, `multiple_unsafe_ops_per_block`
3. **Phase 3**: Tighten selected warning-level lints to deny only after real signal and a cleanup path.
4. **Phase 4**: Trial individual nursery/restriction lints in focused modules before any repo-wide ratchet. Never enable those groups wholesale.

Enforce in runtime code first, then widen to tests once the runtime surface is clean.

### Test exceptions

Package lints apply to all targets unless Clippy knobs say otherwise. For intentional test-only panics/unwraps, set in `clippy.toml` as needed:

```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
allow-panic-in-tests = true
```

Do not claim tests are exempt without checking these knobs and the package lint table.

## 3. Skill policy

- Default to Clippy's standard groups plus the repository's explicit denials unless the project already has a stricter, documented standard.
- Tighten lint levels incrementally, one lint or one module at a time, based on real signal.
- In workspaces, require `[lints] workspace = true` on every new member; absence of lint inheritance is a blocker, not a style nit.
- Prefer mechanical enforcement of hard rules stated in the skill (`panic`, `unwrap_used`, SAFETY comments) over review-only hope.
- Treat lint changes as part of the maintenance contract: if a lint is raised to deny, make sure canonical verification (and future CI) enforce it and the migration cost is known.

## 4. Allowed exceptions

- A global `pedantic` setting is acceptable as deliberate repository policy.
- Global `nursery` or `restriction` groups are **not** acceptable. Cherry-pick individual lints only, with operator-approved policy and measured signal.
- A temporary `allow`/`expect` is acceptable for an upstream false positive, a compiler or Clippy limitation, or a known migration path, with justification and cleanup plan.
- Library APIs that intentionally expose patterns Clippy dislikes may need targeted suppression, but correctness or safety lints should not be weakened casually.
