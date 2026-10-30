# Working agreement

Solve the actual product problem with the smallest coherent design.

Understand the existing code, tests, and documentation. They are evidence, not immutable requirements; change the architecture when that simplifies the system or corrects ownership. [Current architecture](docs/product/ARCHITECTURE.md) describes what is implemented today.

Give each fact one authoritative owner. Prefer direct behavior and state over mirrored models and unnecessary boundaries.

Use mature runtime, platform, and library facilities where they fit; do not reproduce them merely for uniformity. Build abstractions from concrete needs. Prefer ordinary data, direct control flow, deletion, and replaceable boundaries over generalized frameworks, compatibility layers, and new moving parts.

Carillon is the installing user's general-purpose agent. Preserve that user's native operating-system authority; do not add a parallel permission system. Keep authenticated remote ingress fail-closed before runtime work.

Provider-authored emotes and adornments should be configurable defaults in that provider's configuration. Keep this scope narrow: prose, punctuation and safety deadlines do not each need a user setting.

## Code and verification

Write idiomatic Rust and TypeScript. If it looks like a Python developer wrote it, it is bad Rust or TypeScript.

In Rust, use ownership, structs, and enums to express the design. Shared ownership, cloning, locks, trait objects, and stringly state need a real domain reason.

In TypeScript, preserve useful library types, validate unknown input at its boundary, and prefer discriminated unions and ordinary functions over wrappers and repeated casts.

Test observable behavior, state transitions, failure boundaries, and recovery rather than implementation trivia. Passing tests do not justify preserving a bug or obsolete requirement. Update tests with intentional contract changes, without weakening safety merely to make them pass. Verification tasks are described in [README.md](README.md#development).

Carillon is an early beta. Prefer clean cutovers to compatibility scaffolding. Update code, tests, and documentation together, removing what the new design displaces.
