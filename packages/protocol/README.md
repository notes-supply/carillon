# Wire contracts

Rust consumers use [`carillon-protocol`](../../crates/carillon-protocol/src/lib.rs); TypeScript consumers use `@carillon/protocol`.

The shared model vocabulary owns availability, effective selections, inheritance and conditional updates. Providers own option meanings: Pi translates `thinking`; Discord decides how to present choices and recovery instructions. Discord's Wasm invocation protocol remains in [`contract.rs`](../../crates/carillon-discord-commands/src/contract.rs), and consumes these model types.

[`fixtures/models.json`](fixtures/models.json) is consumed by Rust and TypeScript conformance tests. Both must preserve its canonical requests and responses and agree on negative fixtures. A reset is explicit `selection: null`; omitting the required field is invalid, not a reset. Optional non-nullable fields may be omitted but cannot be sent as null. Pi's model-selection tests also validate actual runtime output against the shared schema. These tests run in `mise run check`; no code generator is needed.

[`fixtures/activity.json`](fixtures/activity.json) holds valid and invalid running-turn activity snapshots, which runtimes produce and interfaces render; both languages check it the same way.

[`fixtures/turns.json`](fixtures/turns.json) holds core's runtime turn requests, with and without attachments, including files of earlier unanswered turns offered again. Rust round-trips the canonical requests core sends; the runtime's `RuntimeTurnRequestSchema` accepts them and refuses the invalid ones, such as attachment paths that are not a capture and ordinal.

Installation configuration and durable filesystem utilities belong to `carillon-installation`, not the wire crate. A command module can depend on the wire crate without pulling host filesystem or credential discovery into Wasm.
