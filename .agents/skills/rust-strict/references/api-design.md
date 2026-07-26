# api-design

## 1. Source-backed guidance
- Follow Rust naming conventions from the API Guidelines: `snake_case` for functions/modules, `UpperCamelCase` for types/traits, and `new` for the primary constructor when it is a natural fit.
- Use common traits when they are semantically correct: `Debug`, `Display`, `Clone`, `Eq`, `Hash`, `Default`, and the standard conversion traits (`From`, `TryFrom`, `AsRef`, `AsMut`).
- Prefer newtypes to encode meaningful distinctions, builders for many construction knobs, and `From`/`TryFrom` for explicit conversions instead of ad hoc APIs.
- Document every public item with examples; for fallible APIs, document errors and panics explicitly.
- Avoid ambiguous `bool` parameters in public APIs when a named type or enum would communicate intent better.
- Keep internal binaries from becoming accidental public libraries unless reuse, testing, or API stability really justify a library split.

## 2. Skill policy
- Make names read like Rust, not like a generic OO API.
- Expose types that carry meaning; do not encode domain states as loose primitives when a newtype or enum is clearer.
- Default to the smallest public surface that still supports the use case.
- Use a builder when construction has optional knobs, validation, or order-sensitive setup; use a simple `new` only when that truly stays simple.
- Write rustdoc for public entry points as if the example will be copied into another crate.

## 3. Allowed exceptions
- Private helpers may use `bool`, ad hoc naming, or direct primitive parameters when the scope is local and the intent is obvious.
- Domain-specific constructor names like `open`, `bind`, or `connect` are fine when they match the resource being created.
- An internal binary can stay binary-only if it is not meant to be reused, documented as an API, or depended on as a library.
- Skip a builder or newtype when it adds ceremony without meaningful clarity or validation benefit.
