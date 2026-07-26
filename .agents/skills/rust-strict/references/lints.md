# lints

## 1. Source-backed guidance
- Cargo supports lint configuration in manifests, including workspace-level lint policy via `[workspace.lints]` inherited by crates with `[lints] workspace = true`. See the Cargo Book on [lints](https://doc.rust-lang.org/cargo/reference/lints.html).
- Cargo's `missing_lints_inheritance` lint exists because workspace lint policy is easy to forget; new workspace members must opt in explicitly with `[lints] workspace = true`.
- Clippy documents lint groups and expects teams to choose stricter groups deliberately, not by habit. See the Clippy docs on [lint groups](https://doc.rust-lang.org/stable/clippy/lints.html).
- `clippy::all` is the safest broad default for application and library code that wants Clippy signal without a large policy jump.
- `pedantic`, `nursery`, and `restriction` are stronger policy sets. This
  repository has deliberately enabled `pedantic`; `nursery` and `restriction`
  remain opt-in experiments whose signal and maintenance cost must be measured.
- Suppressions should be local, justified, and reviewed. Prefer `#[expect(...)]` or a narrow `#[allow(...)]` on the smallest item that needs it, with a reason that explains why the lint is acceptable here.

## 2. Workspace lint structure
The repository uses a layered lint architecture:

- **Workspace-level** (`Cargo.toml` `[workspace.lints.rust]`, `[workspace.lints.rustdoc]`, `[workspace.lints.clippy]`): the source of truth for lint policy.
- **Crate-level** (`[lints] workspace = true`): inherits workspace policy. Crate-specific overrides are exceptions, not the norm.
- **Verification/CI-level** (`cargo clippy ... -- -D warnings`,
  `RUSTDOCFLAGS=-D warnings`): promotes warnings to hard failures in canonical
  local verification and must be preserved by CI once it exists.
- **`clippy.toml`**: Clippy configuration knobs (MSRV, threshold overrides, doc-valid-idents).

### Lint rings
Lints are tightened in phases (the "ratchet" strategy):

1. **Phase 1**: Core `rust` and `rustdoc` lints (`unsafe_code`, `elided_lifetimes_in_paths`, `rust_2018_idioms`, `unused_*`, `bare_urls`, `broken_intra_doc_links`, `private_intra_doc_links`).
2. **Phase 2**: Core `clippy` denials (`dbg_macro`, `expect_used`, `todo`, `unimplemented`, `unwrap_used`).
3. **Phase 3**: tighten warning-level lints to deny only after the workspace has real signal and a cleanup path.
4. **Phase 4**: trial stricter Clippy families or individual lints in focused modules before any workspace-wide ratchet.

Enforce in runtime code first, then widen to tests once the runtime surface is clean.

## 3. Skill policy
- Default to `clippy::all` plus the repository's explicit denials for new Rust code unless the project already has a stricter, documented standard.
- Tighten lint levels incrementally, one lint or one module at a time, based on real signal.
- Require `[lints] workspace = true` in every new workspace member; absence of lint inheritance is a blocker, not a style nit.
- Keep manifest lint policy consistent across the workspace so crate-level exceptions do not drift into policy.
- Treat lint changes as part of the public maintenance contract: if a lint is
  raised to deny, make sure canonical verification and future CI enforce it and
  the migration cost is known.

## 4. Allowed exceptions
- The existing global `pedantic` setting is repository policy. A global
  `nursery` or `restriction` setting is acceptable only after an explicit
  policy decision, measured signal, and a funded migration plan.
- A temporary `allow` is acceptable for an upstream false positive, a compiler or Clippy limitation, or a known migration path, but it should carry a justification and a cleanup plan.
- Library APIs that intentionally expose patterns Clippy dislikes may need targeted suppression, but correctness or safety lints should not be weakened casually.
- Temporary allowances are acceptable only when they are local, justified, and paired with a cleanup path.
