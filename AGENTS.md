# Working agreement

Build the product described in `docs/product/` as the smallest coherent production-shaped system.

Write language-native Rust and TypeScript. Code that translates Python architecture or idioms into Rust or TypeScript syntax fails review even when it compiles. Prefer Rust ownership, enums, newtypes, explicit errors, and traits only at real seams; prefer TypeScript inference, discriminated unions, exhaustive narrowing, and validation at untrusted boundaries.

Keep native runtime and platform semantics lossless behind their owning adapters. Do not introduce speculative universal abstractions, duplicate state ownership, broad compatibility layers, or wrapper hierarchies without a present requirement.

Use mature ecosystem APIs through their public interfaces. Keep `mise run check` green and test observable behavior rather than implementation trivia.
