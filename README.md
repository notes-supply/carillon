# Carillon

Carillon is a small self-hosted service that connects a messaging interface to an agent runtime.

The repository currently includes Discord and Pi providers. They are working implementations, not permanent choices: another interface or runtime can replace them without moving its platform- or engine-specific behavior into the core.

The point of the separation is practical. Carillon should be able to grow—and be worked on by agents—without becoming one large adapter full of assumptions about Discord, Pi, or whatever replaces them later.

## Current shape

```text
Discord <-> carillon-discord <-> carillon-core <-> carillon-runtime-pi <-> Pi
```

- **Core** admits events, preserves conversation identity, runs the operation lifecycle, and queues deliveries.
- **Interface providers** own platform authentication, routing, and delivery.
- **Runtime providers** own model execution, native sessions, tools, extensions, and skills.

The included installation runs those three parts as separate user services. Configuration can describe multiple interface and runtime provider instances, while each agent selects a runtime.

Carillon does not define its own command permissions or filesystem sandbox. Services run with the authority of the Unix user that installed them. Pi extensions and tools use Pi's native mechanisms.

## Operation lifecycle

Accepted events are stored before Carillon acknowledges them. A turn moves through:

```text
queued -> running -> completed
                  \-> uncertain
```

Only queued work is started. If Carillon loses track of a running operation, it marks the turn uncertain rather than silently running it again. Successful runtime output, its continuation reference, and the delivery intent are committed together. Interface delivery is retried separately.

An uncertain turn blocks later work in that conversation until an operator either retries or skips it:

```sh
carillon inspect --config ~/.carillon/carillon.toml TURN_ID
carillon resolve --config ~/.carillon/carillon.toml TURN_ID retry
carillon resolve --config ~/.carillon/carillon.toml TURN_ID skip
```

Retrying may repeat work whose outcome could not be observed. Skipping unblocks the conversation without running it again.

## Development

The repository uses Rust, TypeScript, `mise`, Cargo, and pnpm.

```sh
mise install
mise run check
```

`check` runs static checks and the Rust/TypeScript test suites. These include unit tests for parsing, validation, and routing, plus focused database, HTTP, and Pi-session integration tests.

For changes to service communication, startup, routing, or recovery, also run:

```sh
mise run test:integration
```

This starts local core, Discord, and Pi processes and tests admission, conversation continuity, deduplication, restarts, delivery recovery, diagnostics, and backup/restore. Discord input and model responses are simulated; it does not test live Discord or a model provider.

For packaging, installer, service-unit, or owner-environment changes—and before a release—run:

```sh
mise run test:install
```

This builds the package, installs it into a temporary home, and tests permissions, configuration, packaged service startup, continuation, and safe uninstallation. It validates generated systemd units but launches the test processes directly.

During iteration, run the tests affected by the change. Run `check` before submitting code changes; package/install verification is not required after every small edit.

## Installation

Build a release archive with:

```sh
mise run package
```

Extract it and run `install.sh` as the Unix user that should own the agent. The installer prints the remaining Discord-token, Pi-login, and service-start steps. It installs only:

- `carillon-core.service`
- `carillon-discord.service`
- `carillon-runtime-pi.service`
- `carillon.target`

See [`docs/product/OPERATIONS.md`](docs/product/OPERATIONS.md) for configuration, service operation, backup, and restore details.

## Status

Carillon is an early private beta. Configuration and state formats may change without migration support; fresh installations and cutovers are expected while the design is still moving.
