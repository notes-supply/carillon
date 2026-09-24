# Carillon

Carillon is a small self-hosted service that connects a messaging interface to an agent runtime.

The repository currently includes Discord and Pi providers. They are working implementations, not permanent choices: another interface or runtime can replace them without moving its platform- or engine-specific behavior into the core.

The point of the separation is practical. Carillon should be able to grow—and be worked on by agents—without becoming one large adapter full of assumptions about Discord, Pi, or whatever replaces them later.

## Current shape

```text
Discord <-> carillon-discord <-> carillon <-> carillon-runtime-pi <-> Pi
                 |
                 `- carillon_discord_commands.wasm
```

- **Core** (`carillon`) admits events, preserves conversation identity, runs the operation lifecycle, queues deliveries, and relays model-selection calls to the runtime that owns a conversation.
- **Interface providers** own platform authentication, admission, routing, platform commands, and delivery.
- **Runtime providers** own model execution, model selection, native sessions, tools, extensions, and skills.

Core is platform-neutral. Each provider is a separate program installed beside it and launched through the one command line: `carillon discord serve …` runs `carillon-discord`. A provider keeps its own settings in `providers/<id>.toml`; `carillon.toml` is the registry of provider identities, roles, and loopback credentials. The included installation runs the three parts as separate user services, and configuration can describe multiple interface and runtime provider instances while each agent selects a runtime.

Discord's slash commands—currently `/model`, which shows and changes the model and engine settings for a conversation or the agent default—live in a WebAssembly module the provider loads and can replace while running (`carillon discord reload`), without reconnecting its gateway. The shared `carillon-wasm` crate hosts such modules with bounded memory and time; the module can only ask the provider for the native operations it exposes.

Carillon does not define its own command permissions or filesystem sandbox. Services run with the authority of the Unix user that installed them. Pi extensions and tools use Pi's native mechanisms.

## Building blocks

The service and direct callers use the same components. Start with the component
that owns the behavior you want to change:

- **[Durable core](crates/carillon-core/README.md)** — `Core::create` / `Core::open`,
  admission, execution and delivery over SQLite.
  [Run a complete conversation](crates/carillon-core/examples/conversation.rs)
  without HTTP or a model account: `mise run example:core`.
- **[Pi runtime](packages/runtime-pi/README.md)** — `createPiRuntime`, native
  sessions, model selection and explicit shutdown.
  [Run a native session](packages/runtime-pi/examples/session.ts) without Discord
  or Carillon HTTP: `mise run example:pi -- …` (configured credentials required).
- **[Discord interface](crates/carillon-discord/README.md)** — admission,
  conversation mapping, native delivery and command hosting.
  [Render the model picker](crates/carillon-discord/examples/model_picker.rs)
  without a gateway: `mise run example:discord`.
- **[Wire contracts](packages/protocol/README.md)** — identities and requests in
  Rust; shared model vocabulary in Rust/TypeScript with common conformance fixtures.
  No installation or credential discovery.
- **[Wasm host](crates/carillon-wasm/README.md)** — bounded invocation of a compiled
  module with explicit native operations. An optional leaf, not an application framework.

Application assembly supplies installation paths, credentials, listeners and process
lifetimes, and operates installed services and updates. Find it in
[core application](crates/carillon-core/src/application/mod.rs),
[Discord application](crates/carillon-discord/src/application.rs), and
[Pi CLI](packages/runtime-pi/src/cli.ts). Registry, installation-location and
filesystem utilities live in [`carillon-installation`](crates/carillon-installation/src/lib.rs);
bundled provider choices belong to the applications, not the durable engine or wire contracts.

Constructors do not parse CLI arguments or start hidden services. Use ordinary defaults
for component-owned concerns; supply explicit collaborators when replacing them.
The examples are exercised by `check`, including the native Pi session adapter with a
controlled model stream. They are not separate demonstration implementations.

## Operation lifecycle

Accepted events are stored before Carillon acknowledges them. A turn moves through:

```text
queued -> running -> completed
                  \-> interrupted
```

Only queued work is started. Interrupted execution is never automatically replayed and does not block the conversation. Send another message to continue in the runtime's saved session. Separately queued user messages proceed in order. Successful output and its delivery intent are committed together; delivery is separate from execution.

```sh
carillon inspect TURN_ID
```

## Development

The repository uses Rust, TypeScript, `mise`, Cargo, and pnpm. Rust components live in `crates/`, TypeScript packages in `packages/`. The Discord command library also builds for `wasm32-unknown-unknown`; `mise install` adds that target to the toolchain. The component guides above link their public APIs, examples and relevant tests.

```sh
mise install
mise run check
```

`check` runs static checks and the Rust/TypeScript test suites. These include unit tests for parsing, validation, and routing, plus focused database, HTTP, and Pi-session integration tests.

For changes to service communication, startup, routing, or recovery, also run:

```sh
mise run test:integration
```

This builds the command module and starts local core, Discord, and Pi processes to test admission, conversation continuity, deduplication, restarts, delivery recovery, diagnostics, backup/restore, and the `/model` picker end to end through the Wasm module. Discord input and model responses are simulated; it does not test live Discord or a model provider.

For packaging, installer, service-unit, or owner-environment changes—and before a release—run:

```sh
mise run test:install
```

This builds the package, installs it into a temporary home, and tests permissions, configuration, packaged service startup, continuation, and safe uninstallation. It validates generated systemd units but launches the test processes directly.

For filesystem publication or persistence-barrier changes—and before a release—run:

```sh
mise run test:persistence
```

This selects the extended Linux `strace` syscall-injection suite for initialization, restore, configuration writes, resource snapshots, and Discord thread participation. It checks sync ordering and error/retry behavior at real publication boundaries, not physical power cuts. Post-rename failures may leave a complete new tree selected. The install task separately retains representative maintenance failure/retry tests and requires `strace` for its uninstall interruption checks. All of these tests use temporary installations.

During iteration, run the tests affected by the change. Run `check` before submitting code changes; package/install verification is not required after every small edit.

## Installation

Build a release archive with:

```sh
mise run package
```

Extract it and run `install.sh` as the Unix user that should own the agent. The installer writes the registry and the Discord provider's settings, links `~/.local/bin/carillon`, then prints the remaining Discord-token, Pi-login, and service-start steps. It installs only:

- `carillon-core.service`
- `carillon-discord.service`
- `carillon-runtime-pi.service`
- `carillon.target`

After setup, operate the installation with `carillon status`, `start`, `stop`, `restart`, `logs` and `discord reload`. Update the software from a later archive with `carillon update carillon-VERSION-linux-ARCH.tar.gz`; this keeps configuration, credentials and state, and returns the services to their previous running or stopped state.

See [`docs/product/OPERATIONS.md`](docs/product/OPERATIONS.md) for configuration, service operation, backup, and restore details.

## Status

Carillon is an early private beta. Configuration and state formats may change without migration support; fresh installations and cutovers are expected while the design is still moving.
