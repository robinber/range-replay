# api-design

## 1. Source-backed guidance

- Follow the [Rust API Guidelines checklist](https://rust-lang.github.io/api-guidelines/checklist.html).
- Naming: `snake_case` for functions/modules, `UpperCamelCase` for types/traits, conversion prefixes `as_` / `to_` / `into_`, getters without a useless `get_` prefix unless needed for disambiguation.
- Use common traits when they are semantically correct: `Debug`, `Display`, `Clone`, `Eq`, `Hash`, `Default`, and standard conversion traits (`From`, `TryFrom`, `AsRef`, `AsMut`).
- Prefer newtypes to encode meaningful distinctions, builders for many construction knobs, and `From`/`TryFrom` for explicit conversions.
- Avoid ambiguous `bool` parameters in public APIs when a named type or enum would communicate intent better.
- Keep public fields private when future layout changes should not break callers; expose accessors or constructors instead.
- For fallible construction, prefer `try_new` / `build` / `TryFrom` over a fallible `new`.
- Seal traits that are not meant for downstream implementation.

## 2. Review checklist

Before exposing or changing a public item, confirm:

| Check | Question |
|---|---|
| Naming | Does it read like idiomatic Rust, with consistent word order? |
| Meaning | Would a newtype or enum remove a class of misuse? |
| Fallibility | Is failure visible in the type system when recoverable? |
| Traits | Is `Debug` present? Are `Send`/`Sync` intentional where concurrency matters? |
| Errors | Is the error type meaningful, and are `Errors`/`Panics`/`Safety` documented? |
| Surface | Is this the smallest public surface that still supports the use case? |
| Stability | Would a private field or sealed trait preserve future flexibility? |
| Examples | Would a short rustdoc example prevent the most likely misuse? |

## 3. Skill policy

- Make names read like Rust, not like a generic OO API.
- Expose types that carry meaning; do not encode domain states as loose primitives when a newtype or enum is clearer.
- Default to the smallest public surface that still supports the use case.
- Use a builder when construction has optional knobs, validation, or order-sensitive setup; use a simple `new` only when that truly stays simple.
- Write rustdoc for public entry points as if the example will be copied into another crate.
- Prefer validating at construction so the rest of the code can rely on invariants.

## 4. Allowed exceptions

- Private helpers may use `bool`, ad hoc naming, or direct primitive parameters when the scope is local and the intent is obvious.
- Domain-specific constructor names like `open`, `bind`, or `connect` are fine when they match the resource being created.
- An internal binary can stay binary-only if it is not meant to be reused or depended on as a library.
- Skip a builder or newtype when it adds ceremony without meaningful clarity or validation benefit.
