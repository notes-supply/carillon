# Carillon product documents

- [PRODUCT.md](PRODUCT.md) describes the product and current release behavior.
- [ARCHITECTURE.md](ARCHITECTURE.md) describes the implemented ownership, boundaries, and packaging.
- [AGENT_DIRECTORY.md](AGENT_DIRECTORY.md) describes common and Pi-native agent resources.
- [OPERATIONS.md](OPERATIONS.md) describes installation, services, failure handling, backup, and restore.

## Components

- [Core](../../crates/carillon-core/README.md): durable conversations, execution and delivery.
- [Discord](../../crates/carillon-discord/README.md): routing, replies, cards and commands.
- [Pi](../../packages/runtime-pi/README.md) and [T3 Code](../../packages/runtime-t3/README.md): native agent execution.
- [Runtime boundary](../../packages/runtime/README.md): shared HTTP adapter and registry loading.
- [Discord agent tools](../../packages/discord-pi/README.md): optional Pi extension and skill.
- [Wire contracts](../../packages/protocol/README.md): shared Rust/TypeScript vocabulary and fixtures.
- [Wasm host](../../crates/carillon-wasm/README.md): replaceable guest modules and native operations.

Try `mise run example:core` without a model account, `mise run example:pi` with
Pi credentials, or `mise run example:discord` without a gateway.
