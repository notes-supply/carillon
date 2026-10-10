# T3 runtime

Runs Carillon turns on a [T3 Code](https://github.com/pingdotgg/t3code) server
instead of Pi, as another client of the threads T3's own UI shows. It serves
the same runtime boundary as Pi through [`@carillon/runtime`](../runtime/README.md)
and stores nothing of its own.

## Set it up

Guided setup (the installer, or `carillon setup`) offers T3 instead of Pi:
it asks for T3's address and a token, lists T3's projects and models by
name, creates the installation in the chosen project's folder and writes the
files below. Issue a token in T3 with
`t3 auth session issue --label Carillon --token-only`.

Without questions, create the installation with `--runtime t3` (installer
option, or `carillon init --runtime t3`) and `--workspace` set to the T3
project's folder, then:

```sh
carillon runtime-t3 discover --url http://127.0.0.1:3773 < t3.token   # projects and models, as JSON
carillon runtime-t3 configure --url http://127.0.0.1:3773 \
  --project PROJECT_ID --model claudeAgent/claude-fable-5-1 < t3.token
```

`configure` checks the project, its folder and the model with T3, then writes
`credentials/t3.token` and `providers/runtime-t3.toml` under the
installation's maintenance lock: it refuses while an update or backup runs.
The token is read from standard input only. Edit the settings file for a thread title prefix or the
model's options:

```toml
url = "http://127.0.0.1:3773"
token_file = "credentials/t3.token"
project_id = "…"
title = "Carillon"            # optional thread title prefix

[model]                       # the agent default, as T3 names it
instance_id = "claudeAgent"
model = "claude-fable-5-1"

[model.options]               # optional
effort = "high"
```

Then `carillon start`. The runtime reads these files only when it starts, so
after `configure` or a setup rerun changes them, a running Carillon needs
`carillon restart`. Startup fails closed: the registry entry must exist and
be enabled, the credential must match, the settings must be valid, and T3 must
speak orchestration protocol 2 and accept the token. Conversations that
already started stay with the runtime that took their first turn.

The agent's instructions and skills on T3 are the project's own (`CLAUDE.md`
and `.claude/skills`, or `AGENTS.md` and `.agents/skills`); Carillon's Pi
resources aren't sent.

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
- **Models.** A conversation's model is its T3 thread's own selection, which
  T3's UI can change too; the last change wins. The agent default is
  `[model]` above and can't be changed from Discord. A change is refused
  while the conversation is busy.
- **Commands.** `/command` lists what T3's composer offers in the agent's
  workspace and sends an invocation verbatim.
- **Images.** PNG, JPEG, WebP and GIF files up to 10 MiB each (80 MiB per
  message) are uploaded to T3, after checking them against core's digest.
  The message names the files T3 doesn't receive.

## As a library

```ts
import { createT3Runtime } from "@carillon/runtime-t3";

const runtime = createT3Runtime({
  endpoint: { url: "http://127.0.0.1:3773", token },
  projectId: "…",
  selection: { instanceId: "claudeAgent", model: "claude-fable-5-1", options: [] },
  workspace: "/path/to/workspace",
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
