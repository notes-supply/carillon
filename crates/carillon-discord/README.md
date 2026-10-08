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
  Delivered content is sent as given, split at Discord's length limit, with mentions
  disabled. Its `Reactions` keep each message's reactions in order: the processing
  reaction is a request they own until Discord answers it, and the delivered, failed or
  uncertain reaction waits for that request before replacing it. Feedback beside the
  work, including the lanes' overload refusals, runs as their tasks, with at most 64
  outcomes under way (one beyond them is not shown, and that is logged), until the
  connection stops: it then gets two seconds to end, and the rest, like any outcome still
  awaited, is cancelled; nothing is sent after that.
- [`activity`](src/activity.rs) shows one work card per job (a message's turn and
  the observations core names by its origin) and types while its turns run. It is
  the only maker of cards: answers are plain text. It reads what core recorded of
  its jobs, every page of jobs at work and, by name, the jobs it still follows,
  and hears from the delivery loop when an answer's job is taken up and when its
  outcome is recorded. A job's card shows its status and work from core's record,
  stays as the job's summary when the job earned one, and is removed when it did
  not; a job that ended before its card appeared gets a card with the summary it
  earned. Every creation is durably suppressed before its POST. It records only what Discord alone knows: a creation known not
  sent and owed a retry, a card's message, or that one
  nobody can name may exist (a creation Discord did not confirm, or a card someone
  deleted), so the job gets no other card, and a restart goes on with recorded cards
  from core's record. An uncertain progress edit stops optional edits and retains
  the message for final-state retry at the next start. When the provider stops, the monitor reads, types and makes
  cards no more, and sends no typing, new card or progress edit still waiting for a
  rate-limit permit, though it still awaits one that may have left. Once the
  deliveries the stop lets finish are recorded, it reads its jobs once more and
  gives the cards they settle one final edit's bound to show their summaries before
  connect returns.
  Every Discord call is bounded, including rate-limiter waits and Twilight's 429
  retries. Its ordinary [`render` module](src/activity/render.rs) owns pure card embed rendering,
  using the concrete activity markers from [`config`](src/config.rs). Markers have
  configurable defaults, independently of reactions; see [activity card settings](../../docs/product/OPERATIONS.md#activity-cards).
- [`commands`](src/commands.rs) hosts replaceable command generations, registers
  them with Discord, and owns the installed module: `Commands::install`/`reset`
  validate, register, activate and save under the installation's maintenance lock,
  reporting activation, registration and save durability separately. Construct it
  with `CommandFiles` naming the release module, installed module, registration
  record and installation. Command presentation ([`/model`](../carillon-discord-commands/src/model.rs),
  [`/command`](../carillon-discord-commands/src/run.rs)) lives in the separate guest library;
  [`HostCalls`](../carillon-discord-commands/src/lib.rs) is the existing native-operation
  boundary, also used by the example. A submission from `/command` posts a silent marker
  message in the conversation and submits an event anchored on it, with the interaction's
  admission and `native.intent: "command"`. An interaction's native calls, such as a Run
  click's catalog lookup and the submission that follows it, end within seven of the
  module's ten seconds together: within them, at most four to post the marker, and core's
  attempts in the rest, beside the wait of at most two for the processing reaction and
  typing, which never holds them back; core's silence is reported as a possible
  acceptance, never as not sent, and a refusal's reaction is shown without the answer
  waiting for it. A marker Discord does not confirm in time may still have been posted,
  but without its ID nothing reaches core: the reply says the marker was not confirmed and
  the command was not sent to Carillon, and the marker's wording ("asked to run") claims
  nothing ran.
- [`inbound`](src/inbound.rs) settles one admitted message: it asks core whether it
  already holds the message, such as a replayed one, and if not prepares the native
  conversation, emits feedback (admission waits at most two seconds for it) and retries
  only ambiguous transport submission with the same event identity. Its [`lanes`](src/inbound/lanes.rs) run messages one at a
  time per conversation, in arrival order, with bounded queues, and give a `/command`
  submission its turn among them, so core accepts it after the messages before it; one
  core has not answered for by its reply is settled by the lane, under its own identity,
  before later messages go on, or, when the provider stops first, left unconfirmed once the
  attempt under way, or its first, has ended, with none after it, and shown as uncertain;
  a message with files
  is downloaded by [`attachments`](src/attachments.rs) into the provider's staging and
  submitted as a staged batch, accepted whole or refused with an explanation, within
  a preparation deadline. It receives its core client, configuration, participation
  store and downloads explicitly. Deterministic `/v1/simulate` admits a message the
  same way, stages its files with the same downloads and settles only its own
  turn's delivery, so a repeat settles an answer that arrived after its first request
  stopped waiting. It returns the delivery with the card the monitor shows of the
  message's job, from core's record, when the job earned one.
- [`operations`](src/operations.rs) owns authenticated assistant actions using the existing
  Discord client, independently of admission and the delivery outbox. The optional
  [Pi extension](../../packages/discord-pi/README.md) exposes them on demand.
- [`application`](src/application.rs) binds the operator server, connects the gateway,
  and assembles these pieces. [`cli`](src/cli.rs) supplies installation-derived paths
  and credentials. Importing the library starts none of these services.

For ordinary service use, keep using `carillon discord serve …`. Operators change the
command module with `carillon discord commands`, which asks the running provider.
Thread creation/reconciliation and delivery stay native. Pass the admitted plan to
`prepare_conversation(&participation, &discord_http, plan)`: it creates or reconciles
threads, persists participation, and returns the resolved event. Channel routes make
no Discord request. Both the service and external callers use this operation; callers
cannot bypass preparation by assigning an arbitrary target to an opaque plan.
Participation has one owning provider process; sharing its file between independent
writers is not supported.

## Change and test locally

Admission and identity tests live [beside conversations](src/conversations/tests.rs).
Change picker and command-menu layout in the [command library](../carillon-discord-commands/src/),
not in core or a runtime. Host, install and reset tests live in `commands.rs`; HTTP/gateway composition,
tests live in [application tests](src/application/tests.rs). Transport-failure tests
live [beside the core client](src/client/tests.rs), native delivery recovery tests
[beside delivery](src/delivery/tests.rs), activity lifecycle tests, several with Twilight's real
rate limiter, [beside activity](src/activity/tests.rs), literal card rendering tests
[beside the renderer](src/activity/render/tests.rs), and message retry/feedback tests
[beside inbound processing](src/inbound/tests.rs). The [external conversation tests](tests/conversations.rs)
exercise native thread preparation with a local Discord HTTP substitute, not a gateway.

```sh
mise exec -- cargo test -p carillon-discord --all-targets
mise exec -- cargo test -p carillon-discord-commands
mise run test:integration
```

The last task uses simulated Discord input/model responses with the real Wasm host.

To try a command change in an installation, build the module, check it against the
installed provider program, and install it into the running provider:

```sh
mise run build:commands
module=target/wasm32-unknown-unknown/wasm/carillon_discord_commands.wasm
carillon discord commands check "$module"
carillon discord commands install "$module"
carillon discord commands reset    # back to the release's module
```

`check` only reads the file. `install` serves the module at once, registers changed
definitions first, and keeps it across restarts and updates until `reset`.
It does not connect to Discord. Live operation and recovery belong in
[OPERATIONS.md](../../docs/product/OPERATIONS.md).
