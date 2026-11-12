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
  files, and copies of the files answers carry;
- the agents conversations belong to, each with the runtime its new
  conversations start on;
- conversations, each with the channel and title its first message gave it,
  the runtime of its latest turn, and a marker of its latest activity;
- turns (queued, running, completed, interrupted);
- the outcomes runtimes report for the turns they hold, and the turns they
  open themselves;
- delivery intents and their outcomes;
- each interface's read of its own conversations and their turns;
- relaying settings to the runtime that serves the agent, and command reads
  and turn controls to the conversation's runtime; and
- inspection, backup and restore.

SQLite is the only durable store, and network or model work never runs inside
its transactions. `Core` is platform-neutral: it receives agent, resource and
runtime facts and knows nothing of Discord, Pi or the installation's files.
The application around it authenticates HTTP, loads the registry and runs
the dispatchers. See the [core guide](../../crates/carillon-core/README.md).

An interface reads its own history from core, so it need store no
transcript. Core keeps each conversation's title, the first non-blank line
of its first message, and its activity marker, a number that rises in the
transaction that accepts a message of the conversation, opens a turn in it
or ends one of its turns. `GET /v1/conversations` lists the caller's
conversations by that marker, the latest active first, 50 to a page. Each
has its agent, audience, channel and title, when it started and was last
active, its marker and how many of its turns are at work.
`before=<activity>` reads the page below a marker, so a conversation that
becomes active between two reads moves to the first page and is on no
later one. `audience_id=<audience>` lists only that audience's
conversations, one for each agent it has talked to, so an interface that
knows a conversation by its audience alone finds its agent. The list reads no turn and no message text, so an interface may
poll it and compare each conversation's marker and count of turns at work.
While both are unchanged the conversation has no new turn and no new
answer, and its transcript need not be read again. A turn's state while it
is unfinished, and an external answer's delivery state, are not covered: an
interface that shows them reads the jobs of its unfinished turns.

`GET /v1/conversations/{agent_id}/{audience_id}` reads one conversation's
turns, its newest 50 first and oldest first within a page, and
`before=<turn>` reads the page before a turn. A turn is a person's
message, with its event ID, text and files, or a turn the runtime opened,
with its label. It has its state and times, an ended turn's final report,
and the answer core recorded, with its silence, files and delivery state.
Core scopes both reads to the authenticated interface before it reads a
row. Neither returns the context an interface added for the model, an
operator's error text or a snapshot.

### Dispatchers

Each enabled runtime gets one dispatcher. It hands the runtime every queued
message in acceptance order, without waiting for a conversation's earlier
turns, since the runtime orders its own, and waits for core to signal new
work rather than polling. No dispatcher waits for a turn's work.

One supervisor owns the dispatchers and follows the registry core serves: a
live change starts the dispatcher of a runtime now enabled and retires the
rest, and a retired dispatcher ends its hand-over before it ends.

### Turn lifecycle

1. An interface authenticates and admits an event.
2. Core records it as a queued turn, capturing the agent resources and any
   files, and returns a receipt.
3. The runtime's dispatcher hands it over, and the runtime holds it.
4. The runtime runs it and settles it. Model and tool side effects are not
   guaranteed to happen exactly once.
5. Core copies the files an answer names, then stores the outcome, those
   files and a delivery intent in one transaction.
6. The interface claims the delivery, sends it and records the outcome. An
   interface that retains its deliveries skips this step: core records its
   delivery as delivered in step 5.

The dispatcher POSTs the turn to the runtime's `/v1/inbox`. On 202 the turn
is running, which means the runtime holds it, and its files count as
carried. A 5xx, 408, 429
or no answer leaves it queued, to be handed over again in order; the runtime
takes a turn it already holds as the same one. So does an answer that shows
an installation fault rather than the turn's (400, 401, 403, 404, 405 or another
2xx), which core logs as an error. The dispatcher backs off from 1 to 60
seconds meanwhile. Any other answer interrupts the turn; nothing ran, so its
notice is the runtime's explanation alone. The runtime later settles the turn with
its own credential: answered, failed with an explanation it judged safe to
show, steered into an answer that another message carries, on Pi the newest
of those that joined it, or stopped. Steered and
stopped deliver a silence the interface shows on the message instead of a
reply. The outcome and its delivery commit together, once; the same outcome
again is recorded, another is refused. A turn still queued is settled too,
since the runtime may hold a turn whose 202 core never recorded. A runtime
may also open a running turn in a conversation whose agent runs on
it, for a schedule or a subagent's report, idempotent by its key, which no
other conversation may reuse; such a turn has no person or message behind it.
It records the resources of the conversation's latest turn. `GET /v1/runtime/turns`
lists the turns core records running on the runtime, each opened turn with
its key, so a runtime that restarts can take up every turn it holds.
Interfaces may not use event IDs that begin `runtime:`.

Agent assignments are fixed for core's lifetime. If an agent moves to
another runtime or is removed at restart, further messages in its old
conversations are accepted directly as interrupted turns. The receipt,
interruption and explanation's delivery intent commit together. For a moved
agent, that settled turn records the current runtime as owner. The next message
in the same conversation starts a fresh native conversation there. Removed
agents have no new runtime, so further messages remain stranded. Neither
admission nor delivery needs the former runtime enabled or configured.
Command queries return the explanation as a conflict without changing
ownership. Startup settlement also preserves the former owner.
New conversations use the agent's current runtime.

A turn whose outcome can't be proved, or that its runtime reports as failed,
becomes interrupted. Core never replays it; the next message continues the
native session. At startup, the running turns core handed a runtime are
queued again, to be handed over in order, so the runtime is handed every
turn it holds again; it takes a turn it accepted before as the same one.
Turns it opened stay running. The running turns of a runtime no longer
configured are interrupted. Startup then interrupts queued turns whose agent
moved to another runtime or was removed, without needing their former
runtime's dispatcher. Delivery is a separate outbox, so a lost
interface never reruns the model, and a delivery claimed by a crashed
interface becomes uncertain. A silence sends no message, so it is delivered
again instead.

An interface whose registry entry says `delivery = "retained"` shows its
people core's own record, so nothing is sent. Core writes each of its
deliveries as delivered, in the transaction that ends the turn, whichever
way the turn ends: an outcome, a refused hand-over, a stranded
conversation's notice or a startup interruption. No claim returns such a
delivery, no reconciliation touches it, it is never uncertain, and no drain
waits for it. The mode is the registry's when the turn ends, and changes
only with a restart. A turn in flight across that restart ends under the
new mode. A delivery written before it keeps the state it was written in,
so one left pending or claimed still waits for its interface.

### Maintenance drain

`carillon restart` and `update` pause the dispatchers through core
(`POST /v1/pause`, renewed with `PUT /v1/pause/{holder}` and ended with
`DELETE`). While the pause holds, the supervisor runs no dispatcher. The
barrier is the dispatchers' actual end, not a row count. A report counts
dispatchers only from an arrangement made under that very pause, and the
pause lapses on its own if its holder stops renewing it. Running turns hold
no dispatcher, so the drain waits only for a hand-over in progress, and
runtimes still settle their turns while the pause holds. Once no dispatcher
remains, core asks every runtime whether a restart would lose something only its
process knows (`GET /v1/restart`). A runtime that refuses the connection is
not running and holds nothing; any other lack of a valid answer counts as not
ready. The
command then stops the target together with its member services, so it
returns only when their processes have ended, and reports whether core
confirmed the pause until then.

## Interface providers

An interface owns its platform credential and protocol: it authenticates
ingress, maps platform conversations to Carillon audiences and submits
events. With external delivery it also claims deliveries and sends them.
Interfaces fail closed before any runtime work.

What every interface does with core is in the
[shared interface boundary](../../crates/carillon-interface/README.md): the
client of core, submission, staging and the start. An interface the registry
does not list ends at once, as a disabled one does.

The Discord provider (`carillon-discord`) admits messages and interactions by
guild, user and route, each route a channel, by ID, and the agent that
answers there. Allowlisted people's direct messages to the bot have a route
of their own, under the channel `dm`, when the settings name its agent; a
person's direct messages are one conversation, and commands are not
offered there. The provider downloads attachments after admission; uploads
an answer's files with it, from core's copies; creates and adopts threads;
and owns typing, reactions, mention-free delivery and slash command
registration. A message that replies to or forwards another reaches core
with a short quote of that message, from what Discord sent with it, as the
event's context, apart from its own text; a forward's files are downloaded
with it. The provider
records, with its thread participation, which agent had the bot send each
of its latest messages, answers with their files and tool sends alike, and
what agents sent each person directly since that person's last direct
message. A quote names the agent from that record, and a person's direct
message opens with what they were sent. The gateway session record names
the intents the session was opened with, so a release that asks for other
events starts a new session. It also owns one card per job, drawn from core's
record of the job and the runtime's activity snapshot. Feedback is
best-effort and never decides delivery. While a message's turn is in a
runtime that offers controls, it offers them as reactions (steer,
interrupt, stop) and relays an allowlisted person's control to core
(`POST /v1/events/{event_id}/control`), which relays it to the runtime and
keeps nothing of it. A steered or stopped turn sends nothing; its message shows
a reaction instead. A graceful stop records the gateway
session so the next start can resume it and have Discord replay what it
missed. See the [Discord guide](../../crates/carillon-discord/README.md).

The web provider (`carillon-web`) serves a chat page to the installing
user's own browser on the same machine. Setup registers it as `web`, with
`delivery = "retained"` and `max_upload_mib = 1024`. It refuses to serve an
entry with any other delivery, whose answers would wait for a claim it
never makes.
It keeps no conversation. The page is plain files with no build step, read
from disk on each request. It lists conversations and reads turns
through the provider from core's history, polls the list while it is
visible, and asks for the live state of the unfinished turns on screen. In
the browser it keeps its token and each conversation's unsent text. A conversation is one UUID
the page mints, which is its audience, its channel and its delivery target
in core, and its agent is chosen with its first message and never changes.
A message's event ID is the page's too, so a retry is the first message
again. Uploads stream into a staging batch of the request's own. Answers
are rendered to HTML by the provider, and files are read by signed links
that expire. The provider's only durable file is the browser key, which it
reads again for every request that presents a credential. `carillon web
open` reads the key itself and signs a single-use code that expires after
two minutes. A browser trades the code for a bearer token made from the
key. `carillon web sign-out` replaces the key file. Neither command sends
the provider a request. Every request must name the provider in `Host` and
come from its own origin. A failure of core reaches the page as one
retryable status, and what core said is logged.
See the [web guide](../../crates/carillon-web/README.md).

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
Discord's permissions; message operations also reach allowlisted people's
direct messages. The [`discord-pi`](../../packages/discord-pi/README.md)
tools expose them to the Pi runtime's agents, and its skill and
`carillon discord call` to other local agents. The tools are a default only
when a provider with `program = "discord"` is enabled, and they reach that
provider, never the conversation's own interface. A Discord conversation
uses its own provider and knows its message's origin. A conversation on
another interface uses the only Discord provider and has no origin, so the
agent names the channel. Uncertain writes are
reported, not retried.

## Runtime providers

A runtime receives a turn (agent, interface, audience, channel, the person's
input, captured resources, files by relative path and interface metadata)
and owns the conversation's native session. The metadata is opaque apart
from `intent: "command"`, for a command the person invoked, and `context`,
text the interface adds for the model to read before the input, such as a
quote. It keeps each turn core hands it at
`POST /v1/inbox`, answering 202 once kept, and settles it later
through core's `POST /v1/runtime/turns/{id}/outcome`. An answer may name
files by absolute path. Before it records the answer, core copies each into
its attachment store with its digest, within its attachment limits and the
largest file the conversation's interface uploads, and adds a line to the
delivered text for each file it cannot keep. The same outcome again copies
nothing; while the files are being copied, it is refused as unavailable, to
be asked again. It opens turns of its
own through `POST /v1/runtime/turns`, has core capture an agent's resources
for a run it starts itself through `POST /v1/runtime/resources`, reads back the turns core records
running on it through `GET /v1/runtime/turns`, and takes a person's controls at
`POST /v1/turns/{id}/control`. A turn's activity snapshot lists the
controls it offers now, and says `queued` while it waits behind another. A failure
may carry an explanation the runtime has judged safe
to show; core and interfaces display it as given.

Runtimes also answer:

- `GET /v1/turns/{id}/activity` with a bounded snapshot of the running turn:
  phase, recent and running calls, published reasoning and delegated work.
  Tool output is never included.
- `POST /v1/settings` with their settings for an agent or a channel, after
  applying a change when the request carries one.
- `GET /v1/commands` with the engine's own commands: T3's composer
  commands, or a Pi agent's skills as `skill:<name>`. Invoking one sends its
  text verbatim.
- `GET /v1/restart`: whether restarting now loses anything.

The shared [`@carillon/runtime`](../../packages/runtime/README.md) package
holds the HTTP adapter, a runtime's client of core, the display boundary for explanations, tool descriptions and registry
loading.

**Pi** (`carillon-runtime-pi`) is a runtime on pi-durable, Pi's
durable agent harness. Each agent's conversations live in one SQLite file in
its component directory, where model calls, tool calls and waiting messages
survive restarts. A message that arrives while the agent answers in its
conversation steers that answer, waits for it or interrupts it, as the
agent's settings choose, and people steer, interrupt or stop a waiting
message. Steers placed into one answer settle with it: the newest carries
it. An agent's settings may select schedules, subagents and the MCP servers
of `CARILLON_HOME/pi/mcp.json` as built-in extensions; a schedule's run and
a background subagent's report come in turns Pi opens, in the channel core
recorded for the conversation. The runtime's own `attach` tool records a file
with the run that attached it. That run's answer takes the file, and a run
that ends without an answer drops it. A subagent is not offered the tool. Pi's sign-in and catalogue live in
`CARILLON_HOME/pi`, apart from `~/.pi`.
See the [Pi guide](../../packages/runtime-pi/README.md).

**T3** (`carillon-runtime-t3`) is a runtime that runs turns on a T3
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
message runs in that thread. Commands are refused without creating anything
until the project-scoped thread exists, then use it.
Returning to the original project resumes the canonical thread. Conversations
without a T3 thread yet start in the current project. Native work already
running in another project is left alone. Delegated work reports in a turn
the runtime opens, keyed by T3's records, before the reply that started the
work settles; that turn offers no controls, since the work is T3's. Native
work whose stop T3 couldn't confirm is reported as such, not as stopped; it
fences that thread in the runtime's memory, and the runtime refuses a
drained restart while any fence stands or a person's stop waits for T3.

Its agents are plain T3 Code, given the person's text as written, their
threads titled by T3; an agent with resources is refused. Its answers are
text: T3 Code records its assistant messages without files. A turn takes the
model of its channel once, before its first dispatch, and every dispatch of
that turn names it, a reconnect's included. A channel change therefore
reaches each of the channel's threads at its next turn. A model picked in
T3's own UI for a Carillon thread lasts until Carillon's next turn there.
See the [T3 guide](../../packages/runtime-t3/README.md).

## Settings

Each setting has one owner, which declares it, offers its choices,
validates a change, stores it and computes its revision. A renderer draws
what the owners return and defines no setting of its own.

- The interface owns a channel's route: its agent, whether the agent
  answers every message or only mentions, and whether it replies in threads
  or in the channel. Direct messages' route has only its agent. The Discord provider keeps routes in its settings file,
  and while it runs it is that file's only writer: route changes, setup and
  module changes reach it, and one at a time it reads the file, refuses it
  when it differs from what serves, checks the change's revision, validates,
  writes atomically and serves the result. A stopped provider's file is
  changed in place under the installation's maintenance lock.
- The runtime that serves an agent owns its model and the model's options.
  It keeps them in `settings.json` in its component directory: at most one
  model unit per agent and one per channel: the model and the options
  chosen for it. A channel without a unit inherits its agent's, which is
  the only agent default. An option the channel did not choose takes the
  agent's choice while the channel runs the agent's model, else the
  model's default. Choosing a model replaces the unit with that model
  alone, so an option value never outlives its model.
- The Pi runtime also owns an agent's busy mode and extensions, which only
  the agent's page shows. They stay in the `settings.json` of the agent's
  own resource root, which a change rewrites under the model ledger's lease;
  the next turn's captured copy carries it.

A revision digests everything that decides a setting's meaning at its
subject: the subject's interface, channel and agent, its own unit, its
agent's unit while that applies there, and the catalogue entry of the
model that applies.
A change names the revision it was drawn from. The owner refuses it with a
conflict and writes nothing when the revision is another now, so a card
drawn before a re-route or a model switch cannot write.

The Discord provider composes a channel's page from its own route lines and
the runtime's lines, which it reads through core's `POST /v1/settings`.
Core relays only for the interface that owns the channel, or for the
operator, who may also name an agent. The provider serves the same pages to
`carillon settings` on its operator API. The operator names a channel's
interface, or has only one enabled, and `carillon settings` asks that
interface alone: a channel ID means what its interface says, and no other
interface has to answer. The listing asks each enabled interface for its
channels. A runtime that does not answer
leaves the route lines on the page, still editable, so a channel can move
off a broken runtime. A re-route shows the new agent's runtime lines. In a
channel of the guild with no route, the page has only the Agent line and no
subject; choosing an agent creates the route, and no runtime is asked
before it exists. Direct messages' page, for the operator only, works the
same way under the channel `dm`.

A web chat has no page: it is its own channel in core, sets nothing there
and so runs its agent's model.

A turn takes its channel's unit, else its agent's, when the runtime admits
it. Core records each conversation's channel with its first admitted
message and hands it to the runtime with every turn of the conversation,
the turns a runtime opens included.

## Configuration

`carillon.toml` declares providers by stable ID and role, with each one's
program, address, credential hash, expected version and whether it's
enabled, with the largest file an interface uploads, and the agents: each one's ID, name,
runtime, workspace and, for Carillon's assistant on Pi, resources. Each
provider owns `providers/ID.toml`, where a runtime keeps its own settings for
its agents. Both files reject unknown fields. Setup, init, update and the
provider commands change `carillon.toml` in place under the maintenance lock,
as the Discord provider changes its own file: one editor in
`carillon-installation` writes only the values that differ, matching the
tables of a list by ID, so the owner's comments, order, quoting and line
endings stay.

`program` names the program that serves a provider, `carillon-<program>`,
and every entry has one. A file without it is refused with the lines to
add, and nothing migrates it. `Crossing` in `carillon-installation` owns
that refusal's text and the one for a core database in another format: the
steps that cross to this release in place. `Bundled` in `carillon-installation` is the
one list of the programs a release ships: `web`, `discord`, `runtime-pi` and
`runtime-t3`. It owns each one's standard ID, role, version and port above
core's (Discord +1, Pi +2, T3 +3, web +4), and what an interface's entry
says of delivery and of the largest file it takes. Initialization, setup,
version stamping on update and each interface program's choice of its
instance all read it. Setup offers the interfaces in its order, the first
one preselected. Pi chooses its Discord provider by the `program`
the registry gives each entry. Registering a
program never changes an address the file already has, and it refuses when
another provider holds the program's port. An entry whose program the
release doesn't bundle keeps the version its owner wrote.

Setup registers the interfaces and the runtimes chosen. An installation
needs at least one interface, and none has to be Discord. Pi runs
Carillon's assistant whenever it is chosen. Each provider's own program
sets up its part, and setup runs `carillon-<program> setup --provider <id>`
for every interface entry: Pi signs in through its own interface, and
setup then asks the model of each Pi agent that has none; the T3
runtime makes an agent for each T3 project T3 Code channels work in, and
their settings; the Discord provider learns the bot from its token and
offers servers, channels and people by name, asking who answers in each
channel; and the web provider asks nothing, checks its entry and says how
to open the page. Told only `--interface` on an existing installation,
setup registers that interface, runs its program and nothing else, and
restarts a running Carillon once core has drained, because core takes a
new provider only when it starts. It writes nothing but the new entry, and
refuses a port another program of the machine listens on before it writes
that. When the interface's program ends without finishing, setup takes the
entry out again. Carillon counts as stopped only when core's address
refuses the connection: any other failure to reach core fails the command. Setup reruns retain a plain T3 agent's ID by its saved T3 project
ID. New projects receive IDs that avoid both existing agents and saved
project bindings, so keeping a Discord route cannot silently move it to a
different project. Adding Pi explicitly to plain T3 Code channels plans its
assistant while keeping T3's settings and honours `--workspace`. A
registered runtime may have no agents, such as T3 beside Pi with no project
chosen. Core reports that runtime as idle, and its service exits
successfully without serving.
The user target wants every bundled program's unit; one the registry doesn't
list ends at once, as a disabled one does.

Services load their files at start. An explicit apply re-reads a file and
serves it whole when every difference applies live, and otherwise refuses
and names what needs a restart. Core applies runtime availability
live; Discord applies admission, typing, reactions, card
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
latest one it admitted for the agent that loads, and tells the agent why a
newer one did not. Before a run it starts itself, a schedule's or a
subagent's, it asks core for a fresh copy, so the agent can verify its own
edit without a person's message. An edit applies to the agent's next
model request, including those for messages already waiting, and its
`settings.json` selects the agent's extensions and busy mode, which
`carillon settings --agent` also changes. T3 takes no
resources, and refuses an agent that has them. Model sign-in and engine settings are not
resources: Pi keeps them in `CARILLON_HOME/pi`. A runtime
works in its agent's workspace (your home by default for Pi) with your
authority.

## Packaging

`scripts/package-release.sh` builds one archive per Linux host architecture:
the Rust programs for the host target, the web chat's page as the files it
is written in (`lib/web`), the Discord module and the
card-colours example module for `wasm32-unknown-unknown`, the runtimes
compiled with Bun, the systemd units,
and `licenses/` with Carillon's licence and notices for the Rust crates, npm
packages and Bun built into the programs, and the licence of the page's
typeface. Every component reports the
workspace version. A `MANIFEST` lists every file and which must be
executable; the installer refuses anything else. It is a completeness check,
not a signature. Registration publishes every unit the archive ships, the
services first and the target last. The uninstaller keeps no list of units:
when core's unit names its installation, it removes the target and each
service that names that installation too. It looks at the units only while
the application directory is there. Once that is gone it reports so and
leaves the units as they are.

## Trust and authority

Remote admission is the security boundary: interfaces admit only configured
people and places before any runtime work. After admission, every service
has the installing user's ordinary authority, including files, groups and
sudo. Carillon adds no permission system of its own; runtime extensions can
add approval or confinement. Runtimes call core with their own registry
credential, the one core presents to them. A runtime settles its own turns, even once disabled, since it accepted them, and while
enabled opens turns in its own conversations. The
Wasm sandbox bounds the Discord module's memory and time; it doesn't limit
the owner.

The web chat's address belongs to whichever local program holds the web
provider's port. While the provider is stopped, another program on the
machine can take the port and serve a page at the chat's own origin, which
can read what the real page stored there. Carillon trusts the machine's
local programs here, as its other providers do, and remote admission is the
boundary. `carillon web sign-out` voids what such a page could have taken.
`carillon web open` hands the sign-in link, with its two-minute code, to the
browser opener as a process argument, which other users of the machine can
read while it runs.
