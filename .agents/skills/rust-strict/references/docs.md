# docs

## 1. Source-backed guidance
- Rustdoc emphasizes documenting public items, examples, and the API contract visible to users. See the rustdoc book on [how to write documentation](https://doc.rust-lang.org/rustdoc/how-to-write-documentation.html).
- Public-facing items should explain what they are for, how to use them, and the important caveats that affect callers.
- Examples should be short, runnable, and representative. If an example needs setup noise, hide only the unhelpful scaffolding, not the behavior the reader needs to see.
- For APIs that can panic, fail, or rely on unsafe assumptions, document `Panics`, `Errors`, and `Safety` sections where they apply.
- Rustdoc tests compile the examples you show, so docs are part of the correctness surface, not just prose.

## 2. Rustdoc gate
Repository policy requires `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps`. Once CI exists, it must preserve this gate. This means:

- Broken intra-doc links (`broken_intra_doc_links`) are build failures.
- Bare URLs (`bare_urls`) are build failures.
- Private intra-doc links (`private_intra_doc_links`) are build failures.
- Any rustdoc warning is a build failure.

When public docs or rustdoc examples change, verify locally with `RUSTDOCFLAGS="-D warnings" cargo doc-all` before finalizing.

## 3. Skill policy
- Document every public crate, module, type, function, trait, and method that is meant to be consumed by others.
- Keep docs focused on the contract, not an internal tour of the implementation.
- Explain examples in the same terms a caller would use, and prefer one good example over many weak ones.
- Make `Panics`, `Errors`, and `Safety` sections explicit when the API can trigger them; do not bury those details in prose.
- For private helpers and trivial internal glue, write only the comments needed to keep the code readable.

## 4. Allowed exceptions
- Trivial private code, generated code, and one-line helpers do not need exhaustive documentation.
- Internal modules that exist only to organize implementation details can stay lightly documented if they are not part of the API surface.
- A short omission is acceptable for obvious getters, constructors, and pass-through wrappers when the surrounding type docs already cover the contract.
- If a doc example would be misleading because it omits essential setup, keep the example minimal and hide only the boilerplate needed for compilation.
