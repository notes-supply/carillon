# T3 runtime

Runs Carillon turns on a [T3 Code](https://github.com/pingdotgg/t3code) server
instead of Pi, as another client of the threads T3's own UI shows. It serves
the same runtime boundary as Pi through [`@carillon/runtime`](../runtime/README.md).
T3 keeps the conversations; the runtime keeps only the models its agents
and channels chose, in the settings ledger `settings.json` in its component
directory, and the stops core has not recorded yet, in `stops.json` there. Core keeps the
turns: the runtime holds each one core hands it and settles it through
core, which lists the ones it holds to a runtime that restarted.

Every agent on T3 is plain T3 Code: it is given the person's text as
written and works in its T3 project. Carillon's assistant runs on
[Pi](../runtime-pi/README.md); an agent with resources in `carillon.toml` is
refused here, since T3 takes none.

## Set it up

Guided setup (the installer, or `carillon setup`) asks for the runtimes.
With T3, it asks for T3's address and token and the projects for T3 Code
channels. Each project gets an agent, and each agent a default model from
the runtime's offer.

Issue a token in T3 with `t3 auth session issue --label Carillon
--token-only`.

Earlier releases ran Carillon's assistant here through T3 instances of its
own, `carillon_claude` and `carillon_codex`. Nothing uses them now, and plain
agents may pick them like any other instance, so remove them in T3's
settings.

### Without questions

```sh
carillon runtime-t3 discover --url http://127.0.0.1:3773 < t3.token   # projects and models, as JSON
carillon runtime-t3 configure --url http://127.0.0.1:3773 --agent t3-notes \
  --project PROJECT_ID --model codex/gpt-6.1-sol < t3.token
```

`discover --agent ID` offers the models to that agent, refusing one this
runtime does not serve.

`configure` checks two things with T3:

- the project is rooted at the agent's workspace in `carillon.toml`;
- T3 offers the model.

It then writes `providers/runtime-t3.toml` under the installation's
maintenance lock, refusing while an update or backup runs. A token other
than the one the settings name goes to a new private file in
`credentials/` first, which the new settings then name; until they replace
the old ones, those keep their token. Last, it makes the model the agent's
own in the settings ledger, the only place an agent's default lives.

`carillon runtime-t3 check` reads the saved settings and token as startup
reads them, without reaching T3. Setup offers to keep them only when it
passes, and an update checks them before it stops anything. A disabled or
unregistered runtime, which does not start, passes. The token is read from
standard input only.

```toml
url = "http://127.0.0.1:3773"
token_file = "credentials/t3.token"

[agents.t3-notes]                  # by its ID in carillon.toml
project_id = "…"
```

Every agent of this runtime in `carillon.toml` needs its section, and there
is no section for any other. A section that names a `model`, as earlier
releases wrote, is refused. Rerunning `configure` keeps the other agents of
this runtime, and keeps the options chosen for an agent's model while its
model stays the same. `configure` drops the section of an agent
`carillon.toml` no longer gives this runtime.

Then `carillon start`. The runtime reads these files only when it starts,
so after `configure` or a setup rerun changes them, a running Carillon needs
`carillon restart`. It reads the settings ledger for every turn.

Startup fails closed:

- the registry entry must exist and be enabled, and the credential must
  match;
- the settings must be valid, and every agent's workspace a directory;
- no agent on the runtime has resources;
- T3 must speak orchestration protocol 2 and accept the token.

A conversation belongs to the runtime that took its latest turn. When an
agent changes runtimes, the first message in an old conversation gets an
explanation instead of running, and the next one starts a new conversation on
the agent's runtime. After an agent's T3 project
changes and the runtime restarts, the first message in an existing conversation
opens an empty thread in the current project and is refused with an explanation.
That message never runs. The next message starts work in the new thread, including
in a channel that always uses the same conversation. Command requests
are refused without creating anything until that thread exists; then they use it. Returning to the original project resumes the original
thread. Native work already running in another project is left alone, and a
report of it is refused.

## How it behaves

- **One T3 thread per conversation in a project and one message per turn**, named
  from Carillon's identifiers. The canonical thread ID derives from the interface,
  agent, and audience. It is used when absent or in the agent's current project.
  Otherwise the ID also includes the current project. T3's thread record owns
  its project; the runtime checks it before using the conversation. A lost connection or
  answer reads the work back instead of repeating it, and T3's per-command
  receipts mean a turn never runs twice.
- **Held turns.** Core hands each message over (`POST /v1/inbox`); the
  runtime keeps it, runs a conversation's messages in order, and settles
  each through core once T3 has answered, asking again until core records
  it. The same message handed over again is the same turn, even a few
  minutes after it settled. A restart neither stops nor repeats T3's work:
  the runtime reads back from core the turns it held
  (`GET /v1/runtime/turns`) and follows each again in T3's records. One it
  cannot read back fails. Until it has taken them up, a hand-over waits a
  few seconds, then is refused for core to hand it over again.
- **A turn is done** when its run, and any follow-up run T3 starts for its
  background work, have completed with nothing left open. A run waiting for
  an approval or answer the runtime can't give is stopped, and the question
  reported.
- **Stop.** A person may stop a message, waiting or running; it offers no
  steer or interrupt. A waiting message never reaches T3. A running one's
  own run is withdrawn from T3's queue or interrupted, and the turn settles
  as stopped once T3 confirms it, even while the runtime closes; a drained
  restart is refused until then. For background work a completed run left,
  T3 confirms the stop by no longer listing the work after it accepts the
  interrupt; it keeps the native session. A stop core has not recorded is
  kept in `stops.json`, so the next start settles it before anything else
  and never runs the message. A drained restart is refused until
  `stops.json` or core keeps the stop, and a `stops.json` the runtime cannot
  read refuses its start. Work by other runs (yours in T3's UI, for
  example) is never interrupted.
- **Uncertain stops.** When T3 can't confirm a turn's native work stopped,
  the turn says so, and the runtime remembers that thread in memory. Later
  turns there still run, but one that depends on a follow-up ends
  unresolved, and the runtime refuses a drained restart while it remembers
  any such thread.
- **Delegated work.** A reply can say that work delegated to T3's child
  agents goes on. Before that reply settles, the runtime opens a turn of its
  own for the work's report (`POST /v1/runtime/turns`), keyed by T3's
  records, and settles it once T3 reports, even across restarts. While core
  refuses the opening, as it does for a disabled runtime, the runtime asks
  again, or its next start does; a refusal asking again cannot change makes
  the reply say its report could not be scheduled. Neither the reply, once
  its run has answered, nor the report's turn offers controls: the work is
  T3's.
- **The person's text as written**, with a note only of files T3 could not
  take. T3 titles a thread from its first message.
- **Models.**
  - A turn runs its channel's model, else its agent's, as `/settings` and
    `carillon settings` chose them; T3's boolean options are two choices,
    Off and On. The runtime resolves it once, before the turn's first
    dispatch, and every reconnection sends the same selection.
  - A channel change governs the next turn in every thread of the channel.
    T3 makes each message's selection its thread's own, so a model picked in
    T3's own UI lasts until Carillon's next turn there.
  - With no model for the channel or its agent, a turn fails naming
    `carillon settings --agent <id> model=<provider>/<model>`.
- **Commands.** `/command` lists what T3's composer offers in the agent's
  workspace, for the instance of the conversation's thread, else of its
  agent's model, and sends an invocation verbatim; its answer names the
  files T3 did not take.
- **Images.** PNG, JPEG, WebP and GIF files up to 10 MiB each (80 MiB per
  message) are uploaded to T3, after checking them against core's digest.
  The message names the files T3 doesn't receive.

## As a library

```ts
import { createCoreClient } from "@carillon/runtime/core";
import { SettingsLedger } from "@carillon/runtime/settings";
import { createT3Runtime, readStops } from "@carillon/runtime-t3";

const runtime = createT3Runtime({
  endpoint: { url: "http://127.0.0.1:3773", token },
  agents: new Map([["t3-notes", { projectId: "…", workspace: "/path/to/workspace" }]]),
  ledger: await SettingsLedger.open("/path/to/settings.json"),
  stops: await readStops("/path/to/stops.json"),
  core: createCoreClient({ url: "http://127.0.0.1:43800", providerId: "runtime-t3", token }),
});
```

Importing [`src/index.ts`](src/index.ts) opens no connection; creating the
runtime reads back from core the turns it holds. From the
repository, run the service with
`bun packages/runtime-t3/src/cli.ts --config "$CARILLON_HOME/carillon.toml" --provider runtime-t3 --token-file "$CARILLON_HOME/credentials/runtime-t3.token"`.

## Tests

[`fake.ts`](src/fake.ts) stands in for T3, with injectable faults (dropped
answers, ignored interrupts, disconnections, stalled discovery and more).
[`runtime.test.ts`](src/runtime.test.ts) covers continuity, recovery without a
second run, stops, background and delegated work, commands and images
against it and a fake core, [`runtime/held.test.ts`](src/runtime/held.test.ts)
the turns it holds for core, and [`runtime/models.test.ts`](src/runtime/models.test.ts)
each turn's model and the settings over T3's catalogue.
