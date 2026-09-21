# Discord interface

Start with [the gateway-free example](examples/model_picker.rs):

```sh
mise run example:discord
```

It constructs ordinary configuration and temporary participation state, admits a
message and `/model` interaction, and calls the production command handler with a
controlled, read-only model-state collaborator. No gateway, credentials, HTTP server,
or inference is needed. The example is also a Cargo test; service integration tests
retain the actual Wasm path rather than substituting this direct native call.

## Components

- [`conversations`](src/conversations.rs) owns message/interaction admission,
  activation, thread participation and conversation identity. Supply `DiscordConfig`
  and a `Participation` store; `admit_message` returns an activated plan or ignores
  the message, while `admit_interaction` supplies the command context. The caller
  supplies authenticated platform identity, not user-provided identity claims.
- [`CoreClient`](src/client.rs) owns authenticated core requests and bounded network
  waits. Construct it with the core URL, interface provider ID and credential. Clones
  share one delivery-instance identity; construct a fresh client for a new exclusive
  provider instance. Reconciliation requires exclusive ownership of that provider.
- [`delivery`](src/delivery.rs) implements native delivery and feedback using the
  concrete core and Discord clients. It does not own a second delivery queue.
- [`commands`](src/commands.rs) hosts replaceable command generations and registers
  them with Discord. [Command presentation](../carillon-discord-commands/src/model.rs)
  lives in the separate guest library; [`HostCalls`](../carillon-discord-commands/src/lib.rs)
  is the existing native-operation boundary, also used by the example.
- [`application`](src/application.rs) binds the operator server, connects the gateway,
  and assembles these pieces. [`cli`](src/cli.rs) supplies installation-derived paths
  and credentials. Importing the library starts none of these services.

For ordinary service use, keep using `carillon discord serve …`; no new CLI is required.
Thread creation/reconciliation and delivery stay native. `MessagePlan::resolve` maps
an admitted message to a **confirmed** channel/thread target; it does not create a
thread or record adoption. The native service performs those steps before resolving.
Participation has one owning provider process; sharing its file between independent
writers is not supported.

## Change and test locally

Admission and identity tests live [beside conversations](src/conversations/tests.rs).
Change picker layout in the [command library](../carillon-discord-commands/src/model.rs),
not in core or Pi. Host/reload tests live in `commands.rs`; HTTP/gateway composition,
transport-failure and delivery tests live in [application tests](src/application/tests.rs).

```sh
mise exec -- cargo test -p carillon-discord --all-targets
mise exec -- cargo test -p carillon-discord-commands
mise run test:integration
```

The last task uses simulated Discord input/model responses with the real Wasm host.
It does not connect to Discord. Live operation and recovery belong in
[OPERATIONS.md](../../docs/product/OPERATIONS.md).
