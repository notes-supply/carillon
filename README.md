# Carillon

A self-hosted AI agent for Discord, powered by Pi or T3 Code.

Chat in channels or threads, attach files and images, choose models with `/model`,
and run the runtime's commands with `/command`. Work cards show tool calls and
background tasks. Conversations and answer delivery survive service restarts.

Carillon runs on Linux as user-level systemd services, with the installing user's
permissions. It is a private beta; configuration and state formats may change
without migrations.

## Install

Build a release:

```sh
mise install
mise run package
```

Extract the archive in `dist/` and follow the [setup guide](docs/product/OPERATIONS.md).
Pi is the default runtime. [T3 Code setup](packages/runtime-t3/README.md) is optional.

## Development

Rust lives in `crates/`, TypeScript in `packages/`. Mise manages tools and tasks:

```sh
mise run format
mise run check
```

`check` runs formatting, linting, type checking and tests. Use `check:rust` or
`check:ts` for one language. For service changes, also run `mise run test:integration`.
Before a release, run `mise run test:install` and `mise run test:persistence`.

See the [architecture](docs/product/ARCHITECTURE.md) and
[component guides](docs/product/README.md) for the internals.
