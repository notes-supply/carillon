# Carillon documentation

- [PRODUCT.md](PRODUCT.md): what Carillon does and its behaviour.
- [OPERATIONS.md](OPERATIONS.md): install, run, configure, update, back up.
- [ARCHITECTURE.md](ARCHITECTURE.md): components, ownership and boundaries.

## Components

- [Core](../../crates/carillon-core/README.md): conversations, turns and delivery.
- [Discord](../../crates/carillon-discord/README.md): admission, replies, cards and commands.
- [Pi](../../packages/runtime-pi/README.md) and [T3 Code](../../packages/runtime-t3/README.md) runtimes.
- [Runtime boundary](../../packages/runtime/README.md): the shared HTTP adapter.
- [Discord agent tools](../../packages/discord-pi/README.md): optional Pi extension and skill.
- [Wire contracts](../../packages/protocol/README.md): shared Rust/TypeScript types and fixtures.
- [Wasm host](../../crates/carillon-wasm/README.md): the Discord module's sandbox.

`mise run example:core` runs a conversation without a model account,
`example:pi` one with Pi credentials, and `example:discord` Discord admission
without a gateway.
