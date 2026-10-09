# T3 runtime

Runs Carillon turns on a [T3 Code](https://github.com/pingdotgg/t3code) server
instead of Pi, as another client of the threads T3's own UI shows. It serves
the same runtime boundary as Pi through [`@carillon/runtime`](../runtime/README.md)
and stores nothing of its own.

## Set it up

The release includes `app/bin/carillon-runtime-t3` and a user unit,
`carillon-runtime-t3.service`, that Carillon's target doesn't start until you
enable it.

1. Create its installation token and the file holding your T3 token:

   ```sh
   cd ~/.carillon
   head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > credentials/runtime-t3.token
   printf '%s' 'YOUR_T3_TOKEN' > credentials/t3.token
   chmod 600 credentials/runtime-t3.token credentials/t3.token
   sha256sum credentials/runtime-t3.token   # the token_hash below
   ```

2. Add it to `carillon.toml`, and make it the agent's runtime for new
   conversations:

   ```toml
   [agent]
   runtime_provider = "runtime-t3"

   [[providers]]
   id = "runtime-t3"
   role = "runtime"
   version = "0.1.0-beta.1"
   url = "http://127.0.0.1:43803"
   token_hash = "SHA256_OF_THE_TOKEN_FILE"
   enabled = true
   parallel_turns = 4          # optional: conversations run at once (1 to 16)
   ```

3. Write `providers/runtime-t3.toml`:

   ```toml
   url = "http://127.0.0.1:3773"           # T3's HTTP origin, without credentials or path
   token_file = "credentials/t3.token"     # relative to the installation
   project_id = "…"                        # the T3 project whose workspace the agent uses
   title = "Carillon"                      # optional thread title prefix

   [model]                                 # the agent default, as T3 names it
   instance_id = "claudeAgent"
   model = "claude-fable-5-1"

   [model.options]
   effort = "high"
   ```

4. Enable the unit and restart through the target:

   ```sh
   systemctl --user enable carillon-runtime-t3.service
   carillon restart
   ```

Startup fails closed: the registry entry must exist and be enabled, the
credential must match, the settings must be valid, and T3 must speak
orchestration protocol 2 and accept the token. Conversations that already
started stay with the runtime that took their first turn.

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
