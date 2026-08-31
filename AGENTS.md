# Working agreement

Solve the actual product problem with the smallest coherent design.

Understand the existing code, tests, and relevant documentation before changing them. They describe decisions and current behavior, not unchangeable law. Change architecture when doing so materially simplifies the system, corrects ownership, or removes a false assumption.

Prefer one authoritative owner for each fact. Put behavior and state where they can be expressed most directly, and move ownership when the existing boundary is wrong.

Use mature runtime, platform, and library facilities when they fit. Replace or bypass them when they do not; do not reproduce them merely for uniformity.

Build abstractions from concrete current requirements. Prefer ordinary data, direct control flow, deletion, and replaceable boundaries over generalized frameworks, mirrored state, compatibility layers, and new moving parts.

Carillon is the installing user's general-purpose agent. Preserve that user's native operating-system authority and Pi's native tool and extension semantics; do not add a parallel permission system. Keep authenticated remote ingress fail-closed before runtime work.

Write idiomatic Rust and TypeScript. If it looks like a Python developer wrote it, it is bad Rust or TypeScript.

In Rust, use ownership, structs, and enums to express the design. Shared ownership, cloning, locks, trait objects, and stringly state need a real domain reason.

In TypeScript, preserve useful library types, validate unknown input at its boundary, and prefer discriminated unions and ordinary functions over wrappers and repeated casts.

Test observable behavior, state transitions, failure boundaries, and recovery. Avoid tests coupled only to source shape or implementation trivia.

Treat `mise.toml` as the authoritative quality and release gate.

Before finishing, read the complete net diff as a maintainer. Remove displaced code, obsolete tests and state, speculative fallbacks, and architecture that the new design has made unnecessary.
