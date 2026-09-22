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
- [`inbound`](src/inbound.rs) prepares native conversations, emits feedback and retries
  only ambiguous transport submission with the same event identity. It receives its
  core client, configuration and participation store explicitly.
- [`application`](src/application.rs) binds the operator server, connects the gateway,
  and assembles these pieces. [`cli`](src/cli.rs) supplies installation-derived paths
  and credentials. Importing the library starts none of these services.

For ordinary service use, keep using `carillon discord serve …`; no new CLI is required.
Thread creation/reconciliation and delivery stay native. Pass the admitted plan to
`prepare_conversation(&participation, &discord_http, plan)`: it creates or reconciles
threads, persists participation, and returns the resolved event. Channel routes make
no Discord request. Both the service and external callers use this operation; callers
cannot bypass preparation by assigning an arbitrary target to an opaque plan.
Participation has one owning provider process; sharing its file between independent
writers is not supported.

## Change and test locally

Admission and identity tests live [beside conversations](src/conversations/tests.rs).
Change picker layout in the [command library](../carillon-discord-commands/src/model.rs),
not in core or Pi. Host/reload tests live in `commands.rs`; HTTP/gateway composition,
tests live in [application tests](src/application/tests.rs). Transport-failure tests
live [beside the core client](src/client/tests.rs), native delivery recovery tests
[beside delivery](src/delivery/tests.rs), and message retry/feedback tests
[beside inbound processing](src/inbound/tests.rs). The [external conversation tests](tests/conversations.rs)
exercise native thread preparation with a local Discord HTTP substitute, not a gateway.

```sh
mise exec -- cargo test -p carillon-discord --all-targets
mise exec -- cargo test -p carillon-discord-commands
mise run test:integration
```

The last task uses simulated Discord input/model responses with the real Wasm host.
It does not connect to Discord. Live operation and recovery belong in
[OPERATIONS.md](../../docs/product/OPERATIONS.md).
