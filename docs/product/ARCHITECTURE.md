# Carillon architecture

```text
platform <-> interface provider <-> core <-> runtime provider <-> agent engine
```

Discord, Pi and T3 Code are the included providers. Each is its own program
on loopback HTTP; the `carillon` command forwards `carillon <provider> …` to
`carillon-<provider>`, found beside it or on `PATH`.

## Core

`carillon-core` (the `carillon` executable) owns what interfaces and runtimes
share:

- accepted events, with duplicate suppression and immutable copies of their
  files;
- the agents conversations belong to, each with the runtime its new
  conversations start on;
- conversations and the runtime of each one's latest turn;
- turns (queued, running, completed, interrupted) and the observations core
  queues when a runtime says delegated work will report later;
- the outcomes durable runtimes report for the turns they hold, and the
  turns they open themselves;
- delivery intents and their outcomes;
- relaying model and command reads, and turn controls, from an interface to
  the conversation's runtime; and
- inspection, backup and restore.

SQLite is the only durable store, and network or model work never runs inside
its transactions. `Core` is platform-neutral: it receives agent, resource and
runtime facts and knows nothing of Discord, Pi or the installation's files.
The application around it authenticates HTTP, loads the registry and runs
the workers. See the [core guide](../../crates/carillon-core/README.md).

### Workers

Each enabled runtime gets message workers (one, or the registry's
`parallel_turns`) and four observation workers. A message claim takes the
oldest queued message of a conversation with nothing earlier queued or
running on the same runtime, so a conversation keeps its order and two
workers never share one. A durable runtime instead gets one dispatcher and no
observation workers: it hands the runtime every queued message in acceptance
order, without waiting for a conversation's earlier turns, since the runtime
orders its own.
Observations only read native records and run beside messages, never in
their place. Workers wait for core to signal new work rather than polling.

One supervisor owns the workers and follows the registry core serves: a live
change starts workers a runtime now wants and retires the rest, and a
retired worker settles its turn before it ends.

### Turn lifecycle

1. An interface authenticates and admits an event.
2. Core records it as a queued turn, capturing the agent resources and any
   files, and returns a receipt.
3. A worker claims the turn and marks it running.
4. The runtime executes it. Model and tool side effects are not guaranteed
   to happen exactly once.
5. Core stores the output and a delivery intent in one transaction.
6. The interface claims the delivery, sends it and records the outcome.

A durable runtime's turn takes another path from step 3. The dispatcher
POSTs it to the runtime's `/v1/inbox`. On 202 the turn is running, which
means the runtime holds it, and its files count as carried. A 5xx, 408, 429
or no answer leaves it queued, to be handed over again in order; the runtime
takes a turn it already holds as the same one. So does an answer that shows
an installation fault rather than the turn's (401, 403, 404, 405 or another
2xx), which core logs as an error. The dispatcher backs off from 1 to 60
seconds meanwhile. Any other answer interrupts the turn with the runtime's
explanation. The runtime later settles the turn with
its own credential: answered, failed with an explanation it judged safe to
show, steered into the turn running before it, or stopped. Steered and
stopped deliver a silence the interface shows on the message instead of a
reply. The outcome and its delivery commit together, once; the same outcome
again is recorded, another is refused. A turn still queued is settled too,
since the runtime may hold a turn whose 202 core never recorded. A durable
runtime may also open a running turn in a conversation whose agent runs on
it, for a schedule or a subagent's report, idempotent by its key, which no
other conversation may reuse; such a turn has no person or message behind it. Interfaces may not use event IDs that
begin `runtime:`.

Agent assignments are fixed for core's lifetime. If an agent moves to
another runtime or is removed at restart, further messages in its old
conversations are accepted directly as interrupted turns. The receipt,
interruption and explanation's delivery intent commit together. For a moved
agent, that settled turn records the current runtime as owner. The next message
in the same conversation starts a fresh native conversation there. Removed
agents have no new runtime, so further messages remain stranded. Neither
admission nor delivery needs the former runtime enabled or configured.
Model and command queries return the explanation as a conflict without
changing ownership. Startup settlement also preserves the former owner.
New conversations use the agent's current runtime.

A turn whose outcome can't be proved, or that its runtime reports as failed,
becomes interrupted. Core never replays it; the next message continues the
native session. An observation that got no answer is asked again instead,
since it only reads. Startup interrupts running messages and requeues running
observations. A durable runtime's running messages core handed over are
queued again instead, to be handed over in order: a turn started on the
synchronous path before its runtime became durable would otherwise stay
running, and the runtime is handed every turn it holds again. It takes a
turn it accepted before as the same one. Turns it opened stay
running, and its observations are interrupted, since it answers none.
Startup then interrupts queued turns whose agent moved to another runtime or
was removed, without needing their former runtime's workers. Observations
also warn that work
already started there will not be reported here and that actions already
taken may have had effects. Delivery is a separate outbox, so a lost
interface never reruns the model, and a delivery claimed by a crashed
interface becomes uncertain. A silence sends no message, so it is delivered
again instead.

### Maintenance drain

`carillon restart` and `update` pause the workers through core
(`POST /v1/pause`, renewed with `PUT /v1/pause/{holder}` and ended with
`DELETE`). While the pause holds, the supervisor runs no worker: claimed
messages settle normally and observations are set aside to be asked again.
The barrier is the workers' actual end, not a row count. A report counts
workers only from an arrangement made under that very pause, and the pause
lapses on its own if its holder stops renewing it. A durable runtime's
running turns hold no worker, so the drain waits only for its dispatcher's
hand-over, and the runtime still settles its turns while the pause holds.
Once no worker remains,
core asks every runtime whether a restart would lose something only its
process knows (`GET /v1/restart`); no valid answer counts as not ready. The
command then stops the target together with its member services, so it
returns only when their processes have ended, and reports whether core
confirmed the pause until then.

## Interface providers

An interface owns its platform credential and protocol: it authenticates
ingress, maps platform conversations to Carillon audiences, submits events,
claims deliveries and sends them. Interfaces fail closed before any runtime
work.

The Discord provider (`carillon-discord`) admits messages and interactions by
guild, user and route, each route a channel, by ID, and the agent that
answers there; downloads attachments after admission; creates and
adopts threads; and owns typing, reactions, mention-free delivery and slash
command registration. It also owns one card per job, drawn from core's
record of the job and the runtime's activity snapshot. Feedback is
best-effort and never decides delivery. While a message's turn is in a
durable runtime that offers controls, it offers them as reactions (steer,
interrupt, stop) and relays an allowlisted person's control to core
(`POST /v1/events/{event_id}/control`), which relays it to the runtime and
keeps nothing of it. A steered or stopped turn sends nothing; its message shows
a reaction instead. A graceful stop records the gateway
session so the next start can resume it and have Discord replay what it
missed. See the [Discord guide](../../crates/carillon-discord/README.md).

### Replaceable Discord module

Slash commands, their controls and the cards come from one WebAssembly
module, which the provider loads at start and can replace while running.
Each invocation runs in a fresh instance with 16 MiB of linear memory and a
deadline; durable state stays native. The provider keeps the gateway,
admission, card records, every Discord and core call, and authority. The
provider's settings choose the module: a custom file they enable, otherwise
the release's bundled module in `app/lib`. Applying the settings rereads that
file; when the module changed, it takes the installation's maintenance lock,
validates the module (including drawing sample cards), registers commands if
they changed, and only then swaps it in, with the rest of the settings. See
the [Wasm host](../../crates/carillon-wasm/README.md).

Module contract 5 carries the provider's configured markers with both card
requests and admitted interactions. Interaction replies can include embeds
and controls. The host validates reply and card embeds against Discord's
limits and suppresses mentions when it sends them.

### Agent Discord operations

The Discord provider's operator API also runs typed operations
(`POST /v1/operations`) with the bot's own credential, within its guild and
Discord's permissions. The [`discord-pi`](../../packages/discord-pi/README.md)
tools expose them to the Pi runtime's agents, and its skill and
`carillon discord call` to other local agents. Uncertain writes are
reported, not retried.

## Runtime providers

A runtime receives a turn (agent, interface, audience, input, captured
resources, files by relative path and opaque interface metadata) and owns
the conversation's native session. It speaks one of two turn contracts.

A durable runtime, marked `durable` in the registry, keeps each turn core
hands it at `POST /v1/inbox`, answering 202 once kept, and settles it later
through core's `POST /v1/runtime/turns/{id}/outcome`. It opens turns of its
own through `POST /v1/runtime/turns`, reads back the turns core records
running on it through `GET /v1/runtime/turns`, and takes a person's controls at
`POST /v1/turns/{id}/control`. A turn's activity snapshot lists the
controls it offers now, and says `queued` while it waits behind another.

Under the synchronous contract, which no bundled runtime speaks, the runtime
runs a claimed turn while core waits and returns nonblank output. An empty
or unusable answer interrupts the turn.

Either way, a failure may carry an explanation the runtime has judged safe
to show; core and interfaces display it as given.

Runtimes also answer:

- `GET /v1/turns/{id}/activity` with a bounded snapshot of the running turn:
  phase, recent and running calls, published reasoning and delegated work.
  Tool output is never included.
- `GET|PUT /v1/model` with the models they offer, provider display names,
  named option choices, and the selection for the agent or a conversation.
  Selections hold provider, model and choice IDs.
- `GET /v1/commands` with the engine's own commands. Invoking one sends its
  text verbatim.
- `GET /v1/restart`: whether restarting now loses anything.

The shared [`@carillon/runtime`](../../packages/runtime/README.md) package
holds the HTTP adapter for both contracts, a durable runtime's client of
core, the display boundary for explanations, tool descriptions and registry
loading.

**Pi** (`carillon-runtime-pi`) is a durable runtime on pi-durable, Pi's
durable agent harness. Each agent's conversations live in one SQLite file in
its component directory, where model calls, tool calls and waiting messages
survive restarts. A message that arrives while the agent answers in its
conversation steers that answer, waits for it or interrupts it, as the
agent's settings choose, and people steer, interrupt or stop a waiting
message. Steers placed into one answer settle with it: the newest carries
it. An agent's settings may select schedules, subagents and the MCP servers
of `CARILLON_HOME/pi/mcp.json` as built-in extensions; a schedule's run and
a background subagent's report come in turns Pi opens. Pi's sign-in,
catalogue and default model live in `CARILLON_HOME/pi`, apart from `~/.pi`.
See the [Pi guide](../../packages/runtime-pi/README.md).

**T3** (`carillon-runtime-t3`) is a durable runtime that runs turns on a T3
Code server as another client of the threads T3's own UI shows. It stores
nothing durable of a conversation: one thread per conversation in a project
and one message per turn, named from Carillon's identifiers, let a lost
connection, or a turn taken up again after a restart, read the work back
instead of repeating it. Core keeps the turns it holds; restarted, it reads
them back from core and follows each again. It runs a conversation's
messages in order, and a person may stop one, waiting or running. A stop
core has not recorded yet is kept in the runtime's component directory, so
a restart settles it instead of running the message, and a hand-over of a
turn it settled lately runs nothing. The canonical thread ID derives from the interface, agent,
and audience. The runtime uses it when absent or in the agent's current project;
otherwise it uses an ID that also includes the current project. T3's projection
owns each thread's project. After a project change, the first message opens an
empty project-scoped thread and receives an explanation; none of it runs. The next
message runs in that thread. Model requests and commands are refused without
creating anything until the project-scoped thread exists, then use it.
Returning to the original project resumes the canonical thread. Conversations
without a T3 thread yet start in the current project. Native work already
running in another project is left alone. Delegated work reports in a turn
the runtime opens, keyed by T3's records, before the reply that started the
work settles; that turn offers no controls, since the work is T3's. Native
work whose stop T3 couldn't confirm is reported as such, not as stopped; it
fences that thread in the runtime's memory, and the runtime refuses a
drained restart while any fence stands or a person's stop waits for T3.

Its agents are plain T3 Code, given the person's text as written, their
threads titled by T3; an agent with resources is refused. Each message
names the selection its run takes: the thread's, else the agent default.
See the [T3 guide](../../packages/runtime-t3/README.md).

## Configuration

`carillon.toml` declares providers by stable ID and role, with each one's
address, credential hash, expected version and whether it's enabled, and the
agents: each one's ID, name, runtime, workspace and, for Carillon's
assistant on Pi, resources. Each provider owns `providers/ID.toml`, where a
runtime keeps its own settings for its agents. Both files reject unknown
fields.

Setup registers the Discord provider and the runtimes chosen, Pi, T3 or
both. Pi runs Carillon's assistant whenever it is chosen. Each provider's
own program sets up its part: Pi signs in through its own interface; the T3
runtime makes an agent for each T3 project T3 Code channels work in, and
their settings; and the Discord provider learns the bot from its token and
offers servers, channels and people by name, asking who answers in each
channel. Setup reruns retain a plain T3 agent's ID by its saved T3 project
ID. New projects receive IDs that avoid both existing agents and saved
project bindings, so keeping a Discord route cannot silently move it to a
different project. Adding Pi explicitly to plain T3 Code channels plans its
assistant while keeping T3's settings and honours `--workspace`. A
registered runtime may have no agents, such as T3 beside Pi with no project
chosen. Core reports that runtime as idle, and its service exits
successfully without serving.
The user target wants every bundled runtime's unit; one the registry doesn't
list ends at once, as a disabled one does.

Services load their files at start. An explicit apply re-reads a file and
serves it whole when every difference applies live, and otherwise refuses
and names what needs a restart. Core applies runtime availability and
concurrency live; Discord applies admission, typing, reactions, card
markers and its module live. Each admitted message keeps the settings it was admitted
with.

## Agent resources

An agent's resource root is a directory of `SYSTEM.md` (the system prompt),
optional `AGENTS.md`, `settings.json`, `extensions/` and `skills/`, which each
runtime reads in its own way. Core captures a content-addressed copy with
each accepted turn. The copy is write-protected against accidents, not
against the owner. The root must exist; its files are optional, and symlinks
or special files are refused. An agent without resources captures none.

A runtime decides which captured copy a turn uses. The Pi runtime uses the
latest one it admitted for the agent: an edit applies to the agent's next
model request, including those for messages already waiting, and its
`settings.json` selects the agent's extensions and busy mode. T3 takes no
resources, and refuses an agent that has them. Model sign-in and engine settings are not
resources: Pi keeps them in `CARILLON_HOME/pi` and checks its default model
is usable before it serves, so incomplete setup leaves work queued. A runtime
works in its agent's workspace (your home by default for Pi) with your
authority.

## Packaging

`scripts/package-release.sh` builds one archive per Linux host architecture:
the Rust programs for the host target, the Discord module and the
card-colours example module for `wasm32-unknown-unknown`, the runtimes
compiled with Bun, the systemd units,
and `licenses/` with Carillon's licence and notices for the Rust crates, npm
packages and Bun built into the programs. Every component reports the
workspace version. A `MANIFEST` lists every file and which must be
executable; the installer refuses anything else. It is a completeness check,
not a signature.

## Trust and authority

Remote admission is the security boundary: interfaces admit only configured
people and places before any runtime work. After admission, every service
has the installing user's ordinary authority, including files, groups and
sudo. Carillon adds no permission system of its own; runtime extensions can
add approval or confinement. Runtimes call core with their own registry
credential, the one core presents to them: only a durable runtime may. It
settles its own turns, even once disabled, since it accepted them, and while
enabled opens turns in its own conversations. The
Wasm sandbox bounds the Discord module's memory and time; it doesn't limit
the owner.
