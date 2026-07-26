# lints

## 1. Source-backed guidance

- Cargo supports lint configuration in manifests, including workspace-level lint policy via `[workspace.lints]` inherited by packages with `[lints] workspace = true`. See the Cargo Book on [lints](https://doc.rust-lang.org/cargo/reference/lints.html).
- Single-package repositories may put the same policy under package-level `[lints]` without a workspace.
- Clippy documents lint groups and expects teams to choose stricter groups deliberately. See the Clippy docs on [lint groups](https://doc.rust-lang.org/stable/clippy/lints.html).
- `clippy::correctness` is deny-by-default for a reason; do not casually allow it.
- `clippy::pedantic` is for power users and can have intentional false positives; enabling it means accepting local `allow`/`expect` where justified.
- The `clippy::restriction` group should **not** be enabled as a whole. Cherry-pick individual restriction lints after measuring signal. Clippy itself warns against blanket restriction group enables.
- `clippy::nursery` is experimental; cherry-pick only.

## 2. Lint structure

Layered architecture:

- **Manifest-level** (`[lints]` or `[workspace.lints]`): source of truth for lint policy.
- **Inheritance** (`[lints] workspace = true`): required for every new workspace member unless an explicit exception is approved.
- **Verification/CI-level** (`cargo clippy ... -- -D warnings`, `RUSTDOCFLAGS=-D warnings`): promotes warnings to hard failures in canonical verification and must be preserved by CI once it exists.
- **`clippy.toml`**: Clippy configuration knobs (MSRV, threshold overrides, doc-valid-idents).

### Lint rings

Lints are tightened in phases (the "ratchet" strategy):

1. **Phase 1**: Core `rust` and `rustdoc` lints (`unsafe_code` when desired, `elided_lifetimes_in_paths`, `rust_2018_idioms`, `unused_*`, intra-doc link lints).
2. **Phase 2**: Core Clippy denials useful for production code (`dbg_macro`, `expect_used`, `todo`, `unimplemented`, `unwrap_used`).
3. **Phase 3**: Tighten selected warning-level lints to deny only after real signal and a cleanup path.
4. **Phase 4**: Trial stricter Clippy families or individual lints in focused modules before any repo-wide ratchet.

Enforce in runtime code first, then widen to tests once the runtime surface is clean.

## 3. Skill policy

- Default to Clippy's standard groups plus the repository's explicit denials unless the project already has a stricter, documented standard.
- Tighten lint levels incrementally, one lint or one module at a time, based on real signal.
- In workspaces, require `[lints] workspace = true` on every new member; absence of lint inheritance is a blocker, not a style nit.
- Treat lint changes as part of the maintenance contract: if a lint is raised to deny, make sure canonical verification (and future CI) enforce it and the migration cost is known.

## 4. Allowed exceptions

- A global `pedantic` setting is acceptable as deliberate repository policy.
- A global `nursery` or `restriction` setting is acceptable only after an explicit policy decision, measured signal, and a funded migration plan. Prefer cherry-picking.
- A temporary `allow`/`expect` is acceptable for an upstream false positive, a compiler or Clippy limitation, or a known migration path, with justification and cleanup plan.
- Library APIs that intentionally expose patterns Clippy dislikes may need targeted suppression, but correctness or safety lints should not be weakened casually.
