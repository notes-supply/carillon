# Wire contracts

Rust consumers use [`carillon-protocol`](../../crates/carillon-protocol/src/lib.rs); TypeScript consumers use `@carillon/protocol`.

The shared model vocabulary owns availability, effective selections, inheritance and conditional updates. Providers own option meanings: Pi translates `thinking`; Discord decides how to present choices and recovery instructions. Discord's Wasm invocation protocol remains in [`contract.rs`](../../crates/carillon-discord-commands/src/contract.rs), and consumes these model types.

[`fixtures/models.json`](fixtures/models.json) is consumed by Rust and TypeScript conformance tests. Both must preserve its canonical requests and responses. Pi's model-selection tests also validate actual runtime output against the shared schema. These tests run in `mise run check`; no code generator is needed.

Installation configuration and durable filesystem utilities belong to `carillon-installation`, not the wire crate. A command module can depend on the wire crate without pulling host filesystem or credential discovery into Wasm.
