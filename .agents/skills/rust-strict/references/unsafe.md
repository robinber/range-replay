# unsafe

Use this reference whenever adding, reviewing, refactoring, or documenting `unsafe` code, raw pointers, FFI, or safe abstractions over kernel/system interfaces.

## 1. Source-backed guidance

- Prefer safe Rust first. `unsafe` is a boundary where the compiler stops proving memory safety; the author must prove it instead.
- Read the [Rustonomicon](https://doc.rust-lang.org/nomicon/) and the [Unsafe Code Guidelines](https://rust-lang.github.io/unsafe-code-guidelines/) when unsure.
- Standard library policy: each `unsafe` block should have a `SAFETY` comment explaining why the block is sound and which invariants must hold. See the [safety comments policy](https://std-dev-guide.rust-lang.org/policy/safety-comments.html).
- Public `unsafe fn` items need a rustdoc `# Safety` section describing caller obligations.
- Safe wrappers should encapsulate invariants so ordinary callers cannot cause undefined behavior.

## 2. Skill policy

Hard rules:

1. Do not introduce `unsafe` unless the slice requires it and no safe API is adequate.
2. Keep `unsafe` blocks as small as possible. Push checks and setup into safe code around them.
3. Every `unsafe` block must include a `// SAFETY:` comment that states the local proof, not a vague claim that "this is fine".
4. Every public `unsafe fn` must document `# Safety` preconditions.
5. Prefer a small safe API over exporting raw unsafe operations.
6. Do not dilute repo-wide `unsafe_code = "deny"` casually. If the package must use `unsafe`, relax the lint at the smallest module or item scope with `reason = "..."`, or adopt an explicit package policy documented in `AGENTS.md`.
7. When changing `unsafe`, add or update tests for the safe abstraction's guarantees. Run Miri on supported targets when practical:

```bash
cargo +nightly miri test -p <package> -- <test-filter>
```

### Good SAFETY comments

State:

- which pointers/handles are valid, aligned, initialized, and non-aliasing as required;
- which lifetimes or ownership transfers justify the operation;
- which external contracts (kernel, FFI, hardware) are assumed;
- why concurrent access is safe if shared state is involved.

### Safe abstraction checklist

| Question | Expected answer |
|---|---|
| What invariant does the safe type maintain? | Documented on the type |
| Can a safe caller break that invariant? | No |
| Is the `unsafe` region minimal? | Yes |
| Are error paths free of partial UB? | Yes |
| Are drop/cleanup paths sound? | Yes |

## 3. Allowed exceptions

- Generated bindings and vendor stubs may contain bulk `unsafe`; still isolate them and document the trust boundary.
- Performance-motivated `unsafe` needs a measured reason and a safe baseline comparison when practical; "might be faster" is not enough.
- If Miri cannot run (device I/O, unsupported syscalls), document the gap and compensate with focused tests, assertions at boundaries, and careful review.
