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
- conversations and the runtime each one is pinned to;
- turns (queued, running, completed, interrupted) and the observations core
  queues when a runtime says delegated work will report later;
- delivery intents and their outcomes;
- relaying model and command reads from an interface to the conversation's
  runtime; and
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
running, so a conversation keeps its order and two workers never share one.
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

A turn whose outcome can't be proved, or that its runtime reports as failed,
becomes interrupted. Core never replays it; the next message continues the
native session. An observation that got no answer is asked again instead,
since it only reads. Startup records turns still marked running as
interrupted. Delivery is a separate outbox, so a lost interface never reruns
the model, and a delivery claimed by a crashed interface becomes uncertain.

### Maintenance drain

`carillon restart` and `update` pause the workers through core
(`POST /v1/pause`, renewed with `PUT /v1/pause/{holder}` and ended with
`DELETE`). While the pause holds, the supervisor runs no worker: claimed
messages settle normally and observations are set aside to be asked again.
The barrier is the workers' actual end, not a row count. A report counts
workers only from an arrangement made under that very pause, and the pause
lapses on its own if its holder stops renewing it. Once no worker remains,
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
guild, user and route; downloads attachments after admission; creates and
adopts threads; and owns typing, reactions, mention-free delivery and slash
command registration. It also owns one card per job, drawn from core's
record of the job and the runtime's activity snapshot. Feedback is
best-effort and never decides delivery. A graceful stop records the gateway
session so the next start can resume it and have Discord replay what it
missed. See the [Discord guide](../../crates/carillon-discord/README.md).

### Replaceable Discord module

Slash commands, their controls and the cards come from one WebAssembly
module, which the provider loads at start and can replace while running.
Each invocation runs in a fresh instance with 16 MiB of linear memory and a
deadline; durable state stays native. The provider keeps the gateway,
admission, card records, every Discord and core call, and authority. An
operator-installed module lives in the provider's state directory and
survives updates; the release's module lives in `app/lib`. `install` and
`reset` take the installation's maintenance lock, validate the module
(including drawing sample cards), register commands if they changed, and
only then swap it in. See the [Wasm host](../../crates/carillon-wasm/README.md).

### Agent Discord operations

The Discord provider's operator API also runs typed operations
(`POST /v1/operations`) with the bot's own credential, within its guild and
Discord's permissions. The optional [`discord-pi`](../../packages/discord-pi/README.md)
extension and skill expose them to Pi, and `carillon discord call` to other
local agents. Uncertain writes are reported, not retried.

## Runtime providers

A runtime receives a claimed turn (agent, interface, audience, input,
captured resources, files by relative path and opaque interface metadata),
owns the conversation's native session, and returns nonblank output. An
empty or unusable answer interrupts the turn. A failure may carry an
`error` the runtime has judged safe to show; core and interfaces display it
as given.

Runtimes also answer:

- `GET /v1/turns/{id}/activity` with a bounded snapshot of the running turn:
  phase, recent and running calls, published reasoning and delegated work.
  Tool output is never included.
- `GET|PUT /v1/model` with the models they offer and the selection for the
  agent or a conversation.
- `GET /v1/commands` with the engine's own commands. Invoking one sends its
  text verbatim.
- `GET /v1/restart`: whether restarting now loses anything.

The shared [`@carillon/runtime`](../../packages/runtime/README.md) package
holds the HTTP adapter, the display boundary for explanations, tool
descriptions and registry loading.

**Pi** (`carillon-runtime-pi`) keeps a native Pi session per conversation,
runs turns one at a time, and owns models, tools, extensions, skills and
compaction. Its state and sign-in live in `CARILLON_HOME/pi`, apart from
`~/.pi`. It refuses follow-up turns, since it never delegates.
See the [Pi guide](../../packages/runtime-pi/README.md).

**T3** (`carillon-runtime-t3`) runs turns on a T3 Code server as another
client of the threads T3's own UI shows. It stores nothing durable: one
thread per conversation and one message per turn, named from Carillon's
identifiers, let a lost connection read the work back instead of repeating
it. Work delegated to T3's child agents reports later through observations.
Native work whose stop T3 couldn't confirm fences that thread in the
runtime's memory, and the runtime refuses a drained restart while any fence
stands. See the [T3 guide](../../packages/runtime-t3/README.md).

## Configuration

`carillon.toml` declares providers by stable ID and role, with each one's
address, credential hash, expected version and whether it's enabled. Each
provider owns `providers/ID.toml`. Both files reject unknown fields.

Services load their files at start. An explicit apply re-reads a file and
serves it whole when every difference applies live, and otherwise refuses
and names what needs a restart. Core applies runtime availability and
concurrency live; Discord applies admission, typing, reactions and card
markers live. Each admitted message keeps the settings it was admitted
with.

## Agent resources

The agent's resource root is an ordinary Pi-compatible directory:
`SYSTEM.md` (the system prompt), optional `AGENTS.md`, `settings.json`,
`extensions/` and `skills/`. Core captures a content-addressed copy with each
accepted turn, so later edits don't change work already queued. The copy is
write-protected against accidents, not against the owner. The root must
exist; its files are optional, and symlinks or special files are refused.
Model sign-in and engine settings are not resources: Pi keeps them in
`CARILLON_HOME/pi` and checks its default model is usable before it serves,
so incomplete setup leaves work queued. The runtime works in the configured
workspace (your home by default) with your authority.

## Packaging

`scripts/package-release.sh` builds one archive per Linux host architecture:
the Rust programs for the host target, the Discord module for
`wasm32-unknown-unknown`, the runtimes compiled with Bun, the systemd units,
and `licenses/` with Carillon's licence and notices for the Rust crates, npm
packages and Bun built into the programs. Every component reports the
workspace version. A `MANIFEST` lists every file and which must be
executable; the installer refuses anything else. It is a completeness check,
not a signature.

## Trust and authority

Remote admission is the security boundary: interfaces admit only configured
people and places before any runtime work. After admission, every service has
the installing user's ordinary authority, including files, groups and sudo.
Carillon adds no permission system of its own; runtime extensions can add
approval or confinement. The Wasm sandbox bounds the Discord module's memory
and time; it doesn't limit the owner.
