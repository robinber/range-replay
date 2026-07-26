# drift-control

Use this reference when Rust work risks adding maintainability debt: large files,
active audit findings, duplicated logic, broad lint suppressions, public API growth,
command dispatch, runtime orchestration, config resolution, or missing tests.

## 1. No net-new debt

The default standard is not "tests pass"; it is "the touched surface is no worse
after the change." A passing build can still ship architectural drift.

Before editing, inventory the touched surface:

- file size with `wc -l <path>` or equivalent when useful;
- existing tests with a search for `#[cfg(test)]`, `#[test]`, and async test attributes in scope;
- similar logic with a search for domain terms, function stems, or error types;
- active audit/debt notes when referenced by the task, issue, plan, or a maintained tracker.

Dated one-off audit reports are historical context, not permanent live policy.
Read them only when they are explicitly referenced, have unresolved/active
status markers, or are cited by a maintained debt tracker. If an active finding
names the file or behavior you are touching, either reduce that finding, avoid
making it worse, or state the explicit trade-off.

## 2. Size and responsibility gates

| Condition | Required behavior |
|---|---|
| File > 1,000 LOC | Bugfix/test-only additions may be minimal. Feature work should extract or split before adding another responsibility. |
| File > 800 LOC | Treat as a pressure zone. Keep additions narrow and prefer focused helper modules. |
| Function > 80 LOC or deeply nested | Do not add another branch without first considering extraction. |
| Module owns unrelated responsibilities | New behavior should land in the narrower responsibility, not the broad module. |

Do not refactor unrelated code just to satisfy a number. The rule is about
stopping additional drift on the surface you are already touching.

## 3. API shape gates

- Skill gate: a change must not push a function past **six** parameters without
  a request, context, options, or builder type (unless a documented exception
  already exists). That means the resulting arity after the change must be ≤ 6.
- Clippy's `too-many-arguments-threshold` is a looser mechanical backstop, not a
  license to ignore the skill gate.
- Avoid boolean parameters in public APIs unless the name at the call site is
  self-evident. Prefer a small enum for policy choices.
- New public types must describe ownership, fallibility, and caller-visible
  invariants in rustdoc.
- If a function's arguments cluster naturally, name the cluster and make it a
  type. Do not make every caller remember positional meaning.

## 4. Duplication gates

Search before adding logic for repeated concerns such as:

- parsing, validation, and schema or format conversion;
- identifier and key validation;
- size, capacity, offset, and budget accounting;
- error mapping and status/exit-code translation;
- report, checksum, and provenance rendering;
- path, timestamp, retry, and configuration resolution;
- adapter or backend request/response mapping.

Two copies can be transitional. A third copy is a design decision and needs a
shared helper or an explicit divergence reason.

## 5. Suppression gates

New `#[allow]` attributes require:

- the narrowest possible scope;
- `reason = "..."` for non-test code;
- no broad lint-group suppression unless the user approves it or a migration
  note already exists;
- a cleanup path for temporary suppressions.

Prefer fixing the API shape over silencing `too_many_arguments`, fixing the
ownership shape over silencing clone-related lints, and documenting the public
contract over silencing docs lints.

## 6. Test gates

When touching critical logic, add or update focused tests for the behavior you
changed. Critical surfaces typically include:

- input parsing and validation;
- planning, scheduling, and policy decisions;
- capacity, budget, and resource accounting;
- deterministic reports, metrics, and checksums;
- backend adapters and I/O boundary error handling;
- command dispatch and exit-code behavior;
- any module that owns safety invariants or `unsafe` encapsulation.

If tests are impractical in the current turn, state the remaining gap and run
the narrowest command that still exercises the touched path.

## 7. Final report checklist

For non-trivial Rust changes, report:

- files touched that were over 800 or 1,000 LOC;
- whether any active audit/debt finding was affected;
- whether new duplication, parameters, or lint suppressions were introduced;
- tests or verification commands run;
- remaining drift, if any, as explicit follow-up.
