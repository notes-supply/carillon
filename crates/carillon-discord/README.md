# Discord interface

[The gateway-free example](examples/settings.rs) admits a message and a
`/settings` interaction in a thread and runs the real command handler
against a read-only settings page, with no gateway, credentials or
inference:

```sh
mise run example:discord
```

It prints the routed channel's page as Discord shows it, then the
components JSON: the line picker, the picked line's choices with the
configured model badges, and their custom IDs.

## Components

- [`setup`](src/setup.rs) routes each chosen channel to an agent and asks
  when that agent answers. Agents with the same display name include their
  IDs in the choices, questions, and route summary. Unique names stay
  as configured. Core setup uses the same labels when choosing agents' models.
- [`config`](src/config.rs): the provider's settings file and the settings it
  serves; `Settings::apply` serves a re-read file whole, the module it
  declares included, when every difference applies live. Each admitted
  message keeps the snapshot it was admitted with.
- [`conversations`](src/conversations.rs): admission, activation, thread
  participation and conversation identity. `prepare_conversation` creates or
  adopts threads; one provider process owns the participation file.
- [`client`](src/client.rs): authenticated requests to core, with bounded
  waits.
- [`channels`](src/channels.rs): a routed channel's settings page. It joins
  the route's own lines, read-only for now, with the runtime's lines read
  and changed through core for a channel subject the provider builds from
  the route. `/settings` draws it titled `<#channel>`, and the operator reads
  and changes it on `GET /v1/channels` and `GET|PUT
  /v1/channels/{channel_id}/settings`, titled `#name`.
- [`inbound`](src/inbound.rs): settles each admitted message. It asks core
  whether it already holds the message, downloads files
  ([`attachments`](src/attachments.rs)), shows feedback and submits with
  the same identity on retry. [`lanes`](src/inbound/lanes.rs) keep each
  conversation's messages and `/command` submissions in arrival order, with
  bounded queues.
- [`delivery`](src/delivery.rs): sends answers (split at Discord's limit, with
  mentions disabled) from core's outbox, and owns the bot's reactions as
  best-effort feedback: a message's processing and outcome reactions and the
  controls it offers. From a message's admission, or its first offer of
  controls, until its outcome, it records the control emoji the message may
  show, every one it added and has not seen removed, and removes them before
  the outcome shows; a message it neither admitted nor offered controls may
  show every configured control, so those are removed once. A steered or stopped turn sends nothing
  and shows its reaction; a turn without a message behind it, which its
  runtime opened, gets none.
- [`controls`](src/controls.rs): admits a control reaction as a message is
  admitted, by guild and user, maps its emoji to steer, interrupt or stop,
  and has core relay it to the runtime holding the message's turn.
- [`activity`](src/activity.rs): one card per job, drawn by the module from
  core's record of the job, kept as a summary or removed when the job ends,
  and recorded in `activity-cards.json` so a restart carries on with the same
  card. Every Discord call is bounded, including rate-limit waits. It types
  while a job's turn runs, not while it waits in its runtime, and offers the
  controls the runtime's snapshot lists, in any phase. Offering the first
  control on a message this process did not admit removes the others an
  earlier process may have left; a failed change is tried again at the next
  read, and new control emoji replace the previous ones on messages showing
  controls.
- [`module`](src/module.rs): hosts the replaceable Discord module (commands,
  controls and cards) and switches to the one the settings declare under the
  maintenance lock. The guest library lives in
  [`carillon-discord-module`](../carillon-discord-module/src/); its `export!`
  macro gives a custom module, such as the
  [card colours example](../../examples/card-colours/README.md), the same
  entry points.
- [`gateway`](src/gateway.rs): the gateway session, including the record a
  graceful stop leaves so the next start can resume and have Discord replay
  what it missed.
- [`operations`](src/operations.rs): typed actions for agents, with the bot's
  credential, independent of admission and delivery.
- [`application`](src/application.rs) assembles the operator server and the
  gateway; [`cli`](src/cli.rs) supplies installation paths and credentials.
  Importing the library starts nothing.

## Test

```sh
mise run build:module
mise exec -- cargo test -p carillon-discord --all-targets
mise exec -- cargo test -p carillon-discord-module
mise run test:integration
```

Tests that draw cards run the built modules through the Wasm host; build
them first, or point `CARILLON_DISCORD_MODULE` and `CARILLON_EXAMPLE_MODULE`
at other files. Tests sit beside
their modules; [`tests/conversations.rs`](tests/conversations.rs) prepares
threads against a local Discord HTTP stand-in.

## Try a module change

Command menus and card layout live in the module library. To try a change in
a running installation:

```sh
mise run build:module
module=$PWD/target/wasm32-unknown-unknown/wasm/carillon_discord_bundled.wasm
carillon discord module check "$module"
carillon discord module enable "$module"
# after the next build:
carillon discord settings apply
carillon discord module disable    # back to the bundled module
```
