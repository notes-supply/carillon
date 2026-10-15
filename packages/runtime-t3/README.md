# T3 runtime

Runs Carillon turns on a [T3 Code](https://github.com/pingdotgg/t3code) server
instead of Pi, as another client of the threads T3's own UI shows. It serves
the same runtime boundary as Pi through [`@carillon/runtime`](../runtime/README.md)
and stores nothing of its own.

## Set it up

Guided setup (the installer, or `carillon setup`) offers T3 beside Pi. It
asks for T3's address and a token, then:

- **Carillon's assistant**, if it runs on T3: its project, and Claude, Codex
  or both. For each, setup makes Carillon's profile and signs it in.
- **T3 Code channels:** the T3 projects they work in, each an agent of its
  own.
- each agent's default model.

Issue a token in T3 with `t3 auth session issue --label Carillon
--token-only`.

### Profiles

A profile is a T3 instance of Carillon's own, `carillon_claude` or
`carillon_codex`, whose home is
`${XDG_DATA_HOME:-~/.local/share}/carillon/profiles/HARNESS`:

```text
~/.local/share/carillon/profiles/claude/   T3's instance carillon_claude (CLAUDE_CONFIG_DIR)
  CLAUDE.md -> CARILLON_HOME/agents/default/AGENTS.md
  skills    -> CARILLON_HOME/agents/default/skills
  …         Claude's own sign-in, sessions and settings
~/.local/share/carillon/profiles/codex/    T3's instance carillon_codex (CODEX_HOME)
  AGENTS.md -> CARILLON_HOME/agents/default/AGENTS.md
  skills    -> CARILLON_HOME/agents/default/skills
```

The links make the assistant's resources the one copy of Carillon's
instructions and skills. `carillon discord tools install` puts the Discord
skill there, and every profile then has it. Neither the shared workspace nor
your own Claude or Codex sees them.

A profile signs in in one of three ways:

- with the harness's own login in that home (`claude auth login`, `codex
  login --device-auth`), which finishes in a browser on any device;
- with an API key;
- with a compatible endpoint and its token.

T3 keeps a key in its secret store. Settings and profile homes can be
edited, or moved and named elsewhere, by hand.

### Without questions

```sh
carillon runtime-t3 discover --url http://127.0.0.1:3773 < t3.token   # projects and models, as JSON
carillon runtime-t3 profile --url http://127.0.0.1:3773 --agent assistant \
  --harness claude < t3.token                                         # then: CLAUDE_CONFIG_DIR=HOME claude auth login
carillon runtime-t3 configure --url http://127.0.0.1:3773 --agent assistant --assistant \
  --project PROJECT_ID --model carillon_claude/claude-fable-5-1 < t3.token
carillon runtime-t3 configure --url http://127.0.0.1:3773 --agent t3-notes \
  --project PROJECT_ID --model codex/gpt-6.1-sol < t3.token
```

`profile --with-key` reads an API key from a second line of standard input,
and `--endpoint URL` gives the endpoint it is for.

`configure` checks two things with T3:

- the project is rooted at the agent's workspace in `carillon.toml`;
- the model is one the agent may run on.

It then writes `providers/runtime-t3.toml` under the installation's
maintenance lock, refusing while an update or backup runs. A token other
than the one the settings name goes to a new private file in
`credentials/` first, which the new settings then name; until they replace
the old ones, those keep their token.

`carillon runtime-t3 check` reads the saved settings and token as startup
reads them, without reaching T3. Setup offers to keep them only when it
passes, and an update checks them before it stops anything. A disabled or
unregistered runtime, which does not start, passes. The token is read from
standard input only.

```toml
url = "http://127.0.0.1:3773"
token_file = "credentials/t3.token"
title = "Carillon"                 # optional prefix of the assistant's thread titles
profiles = ["carillon_claude"]     # Carillon's own instances: the assistant's alone

[agents.assistant]                 # by its ID in carillon.toml
project_id = "…"
assistant = true

[agents.assistant.model]           # the agent default until /model changes it
instance_id = "carillon_claude"
model = "claude-fable-5-1"

[agents.assistant.model.options]   # optional
effort = "high"

[agents.t3-notes]                  # plain T3 Code
project_id = "…"

[agents.t3-notes.model]
instance_id = "codex"
model = "gpt-6.1-sol"
```

Every agent of this runtime in `carillon.toml` needs its section, and there
is no section for any other. Rerunning `configure` keeps the title, the
profiles and the other agents, and keeps an agent's `[model.options]` while
its model stays the same.

Then `carillon start`. The runtime reads these files only when it starts,
so after `configure` or a setup rerun changes them, a running Carillon needs
`carillon restart`.

Startup fails closed:

- the registry entry must exist and be enabled, and the credential must
  match;
- the settings must be valid, and every agent's workspace a directory;
- T3 must speak orchestration protocol 2 and accept the token.

Conversations that already started stay with the runtime that took their
first turn.

## How it behaves

- **One T3 thread per conversation and one message per turn**, named from
  Carillon's identifiers. A lost connection or answer reads the work back
  instead of repeating it, and T3's per-command receipts mean a turn never
  runs twice.
- **A turn is done** when its run, and any follow-up run T3 starts for its
  background work, have completed with nothing left open. A run waiting for
  an approval or answer the runtime can't give is stopped, and the question
  reported.
- **Cancellation** withdraws a queued run or interrupts the turn's own run,
  and waits for T3 to confirm the stop. Work by other runs (yours in T3's UI,
  for example) is never interrupted.
- **Uncertain stops.** When T3 can't confirm a turn's native work stopped, the
  runtime remembers that thread in memory. Later turns there still run, but
  one that depends on a follow-up ends unresolved, and the runtime refuses a
  drained restart while it remembers any such thread.
- **Delegated work.** A reply can say that work delegated to T3's child
  agents goes on. Its report arrives later as its own message, read from T3's
  records, even across restarts.
- **Two kinds of agent.**
  - **The assistant** runs only on the profiles. Its messages carry where
    the person wrote them, as Carillon's origin metadata.
  - **Plain T3 Code** runs on every other instance. It is given the person's
    text as written, with a note only of files T3 could not take, and T3
    titles its threads from their first message.
- **Models.**
  - A conversation's model is its T3 thread's own selection, which T3's UI
    can change too; the last change wins.
  - Each message names the selection its run takes: the thread's while it
    is on one of the agent's instances, otherwise the agent default. So a
    thread moved in T3's UI onto an ordinary account comes back at
    Carillon's next turn.
  - The agent default is the channel's, which `/model` in the channel
    changes. The runtime keeps that choice in its component directory, and
    `[model]` above applies until then.
  - A change is refused while the conversation is busy.
- **Commands.** `/command` lists what T3's composer offers in the agent's
  workspace and sends an invocation verbatim.
- **Images.** PNG, JPEG, WebP and GIF files up to 10 MiB each (80 MiB per
  message) are uploaded to T3, after checking them against core's digest.
  The message names the files T3 doesn't receive.

## As a library

```ts
import { ModelSelections } from "@carillon/runtime/selections";
import { createT3Runtime } from "@carillon/runtime-t3";

const runtime = createT3Runtime({
  endpoint: { url: "http://127.0.0.1:3773", token },
  agents: new Map([
    [
      "t3-notes",
      {
        projectId: "…",
        workspace: "/path/to/workspace",
        assistant: false,
        selection: { instanceId: "codex", model: "gpt-6.1-sol", options: [] },
      },
    ],
  ]),
  profiles: new Set(),
  selections: await ModelSelections.open("/path/to/model-selection.json"),
});
```

Importing [`src/index.ts`](src/index.ts) opens no connection. From the
repository, run the service with
`bun packages/runtime-t3/src/cli.ts --config "$CARILLON_HOME/carillon.toml" --provider runtime-t3 --token-file "$CARILLON_HOME/credentials/runtime-t3.token"`.

## Tests

[`fake.ts`](src/fake.ts) stands in for T3, with injectable faults (dropped
answers, ignored interrupts, disconnections, stalled discovery and more).
[`runtime.test.ts`](src/runtime.test.ts) covers continuity, recovery without a
second run, cancellation, background and delegated work, models, commands and
images against it.
