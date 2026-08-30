# Working agreement

Build the product described in `docs/product/` as the smallest coherent production-shaped system.

Write idiomatic Rust and TypeScript. If it looks like a Python developer wrote it, it is bad Rust or TypeScript.

Keep native runtime and platform semantics lossless behind their owning adapters. Do not introduce speculative universal abstractions, duplicate state ownership, broad compatibility layers, or wrapper hierarchies without a present requirement.

Use mature ecosystem APIs through their public interfaces. Keep `mise run check` green and test observable behavior rather than implementation trivia.
