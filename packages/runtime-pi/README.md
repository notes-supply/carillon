# Pi runtime

Carillon's agents on [pi-durable](https://github.com/earendil-works/pi), Pi's
durable agent harness. The runtime holds every turn core hands it and settles
it through core, across restarts. `carillon-runtime-pi` is the service; run
with `pi` as its first argument, the same executable is Pi's own command line
(`carillon-pi`), which signs in and keeps Pi's settings.

## Behaviour

- **Storage.** Each agent has one pi-durable harness over
  `<component>/agents/<agent>.sqlite`. Interface and audience name a
  conversation of the agent. One process owns the files; nothing migrates
  sessions of the earlier Pi runtime.
- **Turns.** `POST /v1/inbox` submits the turn's message to its
  conversation and answers 202; the same turn again is the same message,
  also once it is answered. A background delivery task waits for the message
  to be answered or given up, decides the outcome once and posts it to core
  until core records it, sleeping durably between attempts. A refusal that
  asking again cannot change, a client error other than 401, 403, 408 or
  429, is answered once with a plain failure in its place; refused again,
  the outcome is given up.
- **Outcomes.** An answer is posted with its final activity report, a note
  added when the model's output limit cut it off. Messages steered into one
  run settle with its answer: the newest carries it, and the others are
  `steered`. A run the person stopped is `stopped`. A failed run gives the
  provider's explanation where it may be shown. A turn with no model names
  `carillon settings --agent <id> model=<provider>/<model>`, and one whose
  model is gone says to choose another with `/settings`.
- **Opened turns.** Output no message prompted, such as a schedule's run
  or a subagent's report, comes in a turn the runtime opens with core
  (`POST /v1/runtime/turns`). A background task of the conversation opens
  it under a key holding the agent's ID and a nonce of its store, so no
  other agent, and no store recreated in its place, shares it. The key
  opens and places the turn once across restarts; its input waits as a
  follow-up on the model of the channel core names in its reply, and the
  turn settles like
  any other, with its cards, typing and stop. Work that has ended, such as
  a cancelled schedule's run, opens no turn, unless core was asked already
  and may have opened one. A turn core has settled already is left alone,
  one whose work ended before it was placed is settled `stopped`, and one
  whose work ended while it was placed is stopped as a person stops it. An
  unreachable core is asked again with backoff; an opening core refuses for
  good is given up.
- **Busy conversations.** `settings.json`'s `busy` decides what a message does
  while the agent answers in its conversation: `steer` (the default) joins
  the running answer after its current tool round, `queue` waits for it, and
  `interrupt` ends it, a note saying so, with every waiting message joining
  the next answer. The message is accepted before that answer ends; if the
  process crashes before the interrupt is applied, it waits as in `queue`.
- **Controls.** `POST /v1/turns/{id}/control` acts on a waiting message for
  its interface: `steer` hands it to the running answer, `interrupt` ends that
  answer and runs the waiting messages next, and `stop` drops it. Stopping a
  running message ends its answer, never one that followed it. A waiting
  turn's activity has phase `queued` and lists the controls that apply: a
  waiting steer offers no `steer`.
- **Restarts.** A model call or a replay-safe tool cut off by a crash runs
  again; any other tool tells the model it was interrupted. Parked deliveries
  run again at startup. A run that crashed the process too often is given up,
  its turns failed.
- **Activity.** Pi-durable's agent events give each running turn's thinking
  notes, commentary, model, context use and tool calls. `/v1/restart` is
  unready only while a tool call runs.
- **Models.** Pi's own `ModelRuntime` holds the catalogue and sign-in from the
  state directory (`auth.json`, `models.json`, `settings.json`); Pi's default
  model is not read. `POST /v1/settings` shows and changes the model of an
  agent and of each of its channels, kept in `<component>/settings.json`. A
  turn runs its channel's model, else its agent's, with thinking clamped to
  the levels the model supports, resolved as it is admitted, or as it opens
  for a schedule's run or a subagent's report. pi-durable keeps one model
  per conversation, so that resolution moves the whole conversation,
  including turns still running or waiting there.
- **Agent page.** The agent's page adds `busy` and `extensions` after its
  model lines, read from the `settings.json` of the agent's own resource
  root, never a captured snapshot. A change is checked against its line's
  revision and written to that file under the settings ledger's lease,
  keeping every field it does not name. `extensions` takes the built-in
  extensions and the root's `extensions/<file>` modules and refuses any other
  name, and keeps the file's mode. The next resource capture carries the
  change. An agent without a resource root shows both lines with no choices
  and refuses a change; settings that cannot be used show both lines empty,
  and a change to either names why.
- **Setup.** `--check` passes once Pi is signed in and offers a model.
  `--models --agent <id>` prints the models Pi offers and the agent's model
  as JSON, and `--configure --agent <id> --model <provider>/<model>` sets the
  agent's model.
- **Files.** Files arrive by path below `attachment_root`, listed in a note
  with the message. PNG, JPEG, WebP and GIF images within the
  `inline_images` limits go along inline, prepared with Photon for the model
  the turn runs. Once a conversation has had files, its prompt says where
  they are kept.
- **Answer files.** The runtime's own `attach` tool checks that a path names
  a regular file it can read and records it, once, in the conversation's
  `carillon.attached` document, with the run that attached it, named by the
  run's first input. The message that carries the run's answer takes the
  run's files in the commit that decides its outcome, and posts their paths
  with it, so a restart before or after keeps them. Its delivery task reads
  how each run ended from the run's submissions, so every answer takes its
  own run's files whatever order the tasks decide in. A run that is stopped,
  interrupted or fails drops its files. `attach` calls run one at a time, so
  the files keep the order they were attached in. A subagent's conversation
  is not offered `attach`, since its answer goes to the agent that started
  it.

## Resources

The latest resource snapshot admitted for an agent that loads wins: its
prompt and extensions apply to the agent's next requests, also for messages
admitted earlier. Each snapshot is read once:

- `SYSTEM.md` as the prompt's base, `AGENTS.md` and `CLAUDE.md` from the
  snapshot and the workspace's parents, and the snapshot's `skills/` beside
  the runtime's own in [`skills/`](skills), which one of the same name
  replaces. The prompt also names the agent's store, which the built-in
  `conversation-history` skill reads;
- `settings.json`, whose unknown settings make the snapshot unloadable:

  ```json
  { "extensions": { "add": ["codemode"], "remove": ["tool-search"] }, "busy": "steer" }
  ```

  `extensions` is a list that selects exactly those, or `add` and `remove`
  over the default: `coding`, `tool-search`, and `discord` when Carillon has
  a Discord provider. The built-in extensions are `coding` (read, write, edit
  and bash), `codemode`, `tool-search`, `discord`, and
  [`schedules`](#schedules), [`subagents`](#subagents) and
  [`mcp:<server>`](#mcp-servers) for each server of `mcp.json`, which no
  default selects. Selecting an `mcp:` server that `mcp.json` lacks is
  refused like any unknown name.

- user extensions it lists as `extensions/<file>.js`, `.mjs` or `.ts`: a
  module whose default export is a pi-durable extension object, with its
  `tools`, `sections`, `hooks`, `wraps` and `tasks`. It is imported from the
  snapshot, inside the runtime's process, and can import Node's built-ins
  and packages vendored in the snapshot's `extensions/node_modules/`, not
  the runtime's own.

A snapshot does not load when its settings or a listed module cannot be used,
including a module pi-durable cannot install beside the built-in extensions,
such as one reusing a task name, or one still importing after 5 seconds. The
turn that brought it still runs, on the last snapshot that loaded, or on the
defaults when none did, and never records the one that failed. The model
reads a note before the message saying so, with the error, cut at 4,000
characters: Bun's build errors each at their file, line and column, and a
throw with its frames in the snapshot, paths relative to the snapshot. The runtime logs it once. The
deadline cannot stop a module whose top-level code exits the process or never
yields. A newer snapshot replaces the
installed user extensions. An old one stays while a live task needs one of
its task names that no installed module of the snapshot defines, and each
later admission adopts the snapshot again, so a module the kept one blocked
installs, and takes its tasks over, once the old one can go.
A recorded snapshot that no longer loads at startup is logged, and the agent
uses the default extensions until one that loads is admitted; what it selects
is unknown meanwhile, so schedules wait for it.

## Skills

A person invokes a skill of the snapshot by naming `$<name>` anywhere
outside code in their own words, or by opening them with Pi's
`/skill:<name>`. Only the turn's `input` counts: the interface's context
(`native.context`, such as Discord's quote of the message replied to) comes
first and invokes nothing. Code is what Discord shows as code: a run of
backticks up to the next run of as many, across lines, so fences and spans
alike, with backslashes inside kept as they are. Discord shows a backtick
escaped as `` \` ``, indented lines and `~~~` fences as plain text, and so
does this. Only a skill's exact name counts, as a word of its own, so
`$HOME` or `$triaged` stays as written, but plain text such as an unquoted
shell variable named like a skill invokes it; backticks keep it literal.
Each skill invoked, once and in the order first named, precedes the
person's words in Pi's own block, after the context:

```text
<skill name="triage" location="<snapshot>/skills/triage/SKILL.md">
References are relative to <snapshot>/skills/triage.

<SKILL.md without its front matter>
</skill>
```

`$<name>` keeps the person's words whole after the blocks. `/skill:<name>
text` leaves `text`. `/skill:` without a name, or with a name no skill has,
refuses the turn with 422, listing the skills, and so does invoking a skill
whose `SKILL.md` can no longer be read; core sends that explanation as the
turn's notice. Files and images go along as they are. The runtime expands the
message as it admits the turn, so the message it keeps, and a restart
replays, is the expanded one. Turns the runtime opens itself, for a
schedule's run or a subagent's report, are not expanded.

A skill whose front matter sets `disable-model-invocation: true` stays out of
the prompt and is invoked like any other. The built-in
[`carillon-hooks`](skills/carillon-hooks/SKILL.md) is one: it explains user
extensions and pi-durable's hooks, with examples the tests load and run.

`/v1/commands` lists every skill as `skill:<name>` with its description, read
from the agent's own resource root and the built-in skills, never a captured
snapshot, so it lists what the next turn brings. A name too long for the
100-character command name is left out of the list and still invoked by
`$<name>`.

## Schedules

`schedules` gives the agent `schedule`, `schedule_list` and
`schedule_cancel`. A schedule runs its prompt in the conversation that made
it, once at an ISO 8601 time or on a cron expression of five fields, so at
most once a minute, in the time zone it names or, as it was when the
schedule was made, the machine's. Each run opens a turn whose prompt says
it is a scheduled run. As in cron, a schedule that runs more than daily
runs through the hour daylight-saving time repeats as it falls back once,
not twice.

A schedule is a background task of its conversation: stopping an answer
leaves it, and it sleeps durably until it is due. A restart resumes it, and
runs missed while the runtime was down happen once, not once per missed
time. Each run's turn is opened and placed before the schedule is next due,
counted from then, so one run is under way at a time, and a run that waited
for an unreachable core is followed by none it held up. Until then a
schedule that runs once can still be cancelled. A run core refuses to open
for good is skipped. Once the agent's settings no longer select
`schedules`, each schedule ends when it is next due, and a run whose turn
is still opening is stopped, as it is when its schedule is cancelled. While
the agent's recorded resources no longer load at startup, what is selected
is unknown: a due schedule waits for the next message whose resources load.

## Subagents

`subagents` gives the agent `subagent {task, wait?, model?, thinking?,
speed?}`, which runs the task in a child conversation that starts as a copy
of the agent's own, without the `subagent` tool or `schedules`. The child
runs the caller's model, thinking level and standard speed, except what the
call sets. `model` is a `<provider>/<model>` Pi offers when the call runs;
another is refused with the list of those it offers. `thinking` moves to the
nearest level the child's model supports, as Pi's default thinking does.
`speed: "fast"` sends OpenAI's `service_tier: "priority"` from a
`beforeProviderRequest` hook, for models on the `openai-responses`,
`openai-codex-responses` and `azure-openai-responses` APIs only, and is
refused on any other. The call checks its choices only when it creates the
child, which stores them: pi-durable's agent holds its model and thinking,
and a document of the child its speed and any thinking level its model
lacked. The call's result reads them there, so a rerun after a restart
names what the child runs even if the parent or Pi's models have changed
since. pi-ai prices a response at the tier the
response names, so where the provider names `default`, as ChatGPT's Codex
backend can, a fast child's estimated cost reads at the standard price.
Per-turn model resolution moves only the conversations Carillon's turns run
in, never a child. The child serves the same Carillon conversation, so tools
such as Discord's work there as in its parent. In the background, the
default, the call creates the child with a reporter task of the
conversation that owns it, and returns at once: stopping the answer that
started it leaves the child working. A reporter an earlier version left
without a child creates one that copies its parent's agent, at standard
speed. Once the child answers, the reporter's opening places the answer,
or why there is none, as a follow-up in a turn under the key
`subagent:<task>`; stopping that turn stops only the report, and a report
core refuses for good is given up. With `wait: true`, the call owns the
child and returns its answer as its result, and stopping the answer that
waits for it aborts the child.

## MCP servers

`mcp.json` in the state directory configures MCP servers. It holds their
credentials, so it is never part of an agent's resources, and, as ssh does
with a private key, the runtime refuses to start with one that other users
may read or write: keep it mode 0600. It takes a subset of the usual shape,
a program over stdio or a Streamable HTTP endpoint at an `http:` or `https:`
URL:

```json
{
  "mcpServers": {
    "files": { "command": "mcp-files", "args": ["--root", "/srv"], "env": {}, "cwd": "/srv" },
    "notes": { "url": "https://mcp.example.com/mcp", "headers": { "authorization": "Bearer …" } }
  }
}
```

A server's name is words of `a`-`z` and `0`-`9` joined by `-`, such as
`remote-notes`, so no two servers' tool names are alike. A file that is not
valid stops the runtime at startup, saying why; without the file there are
no servers. Each server is the extension `mcp:<server>`, on one connection
the runtime's agents share. It connects once a snapshot selects it, and its
tools, named `mcp__<server>__<tool>`, are deferred, so `tool-search` finds
and loads them. While it is disconnected it offers no tools, and it
connects again with backoff, which starts over only after a connection held
for a minute. Losing the session or the transport, as when the server's
session expired, the network failed, a response stream ended without its
answer or the server-to-client stream could not be reopened, disconnects it
too, and it initializes anew. A request the server refuses, such as with
HTTP 429, fails alone. A failed call's error, or the text of a failed
result, reaches the model and the log bounded, its credentials redacted; a
failed result's images reach the model as they are. The runtime closes the
connections as it stops, also one still connecting.

## Deterministic mode

`--deterministic` runs the real runtime on scripted local models (`local/echo`,
`local/reasoner` and `substitute/fixed`). Each answer says what reached the
model: the conversation's message count, the message, whether a section
saying `Carillon native extension active` and the `carillon-status` skill
reached the prompt, the model and thinking level, and the listed attachments.
A message `hold <path>` is answered once that file exists.

## Where things live

- [`runtime.ts`](src/runtime.ts): the agents, model operations and shutdown.
- [`agent.ts`](src/agent.ts): one agent's harness, admission, startup
  recovery, busy modes and controls.
- [`delivery.ts`](src/delivery.ts): the task that settles each turn with core.
- [`opening.ts`](src/opening.ts): the task that opens a turn for input no
  message brought.
- [`schedules.ts`](src/schedules.ts), [`subagents.ts`](src/subagents.ts): the
  `schedules` and `subagents` extensions and their tasks.
- [`mcp.ts`](src/mcp.ts): `mcp.json` and each MCP server's extension.
- [`skills.ts`](src/skills.ts): the skills a message invokes, and `/command`'s
  list of them.
- [`resources.ts`](src/resources.ts), [`attachments.ts`](src/attachments.ts),
  [`activity.ts`](src/activity.ts), [`models.ts`](src/models.ts): snapshots
  and settings, files, activity, and the catalogue.
- [`documents.ts`](src/documents.ts): Carillon's state in each agent's storage.
- [`cli.ts`](src/cli.ts): the service's arguments, credentials, built-in
  extensions and lifecycle.
- [`deterministic.ts`](src/deterministic.ts): the scripted models of
  `--deterministic`.
