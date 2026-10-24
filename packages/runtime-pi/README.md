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
  provider's explanation where it may be shown; no available model says to
  choose one with `/model`.
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
  state directory (`auth.json`, `models.json`, `settings.json`), whose default
  model is the runtime default. Carillon's agent and conversation choices live
  in `<component>/model-selection.json` and apply to each conversation as its
  turns are admitted.
- **Files.** Files arrive by path below `attachment_root`, listed in a note
  with the message. PNG, JPEG, WebP and GIF images within the
  `inline_images` limits go along inline, prepared with Photon for the model
  the turn runs. Once a conversation has had files, its prompt says where
  they are kept.

## Resources

The latest resource snapshot admitted for an agent wins: its prompt and
extensions apply to the agent's next requests, also for messages admitted
earlier. Each snapshot is read once:

- `SYSTEM.md` as the prompt's base, `AGENTS.md` and `CLAUDE.md` from the
  snapshot and the workspace's parents, and the snapshot's `skills/`;
- `settings.json`, whose unknown settings are refused:

  ```json
  { "extensions": { "add": ["codemode"], "remove": ["tool-search"] }, "busy": "steer" }
  ```

  `extensions` is a list that selects exactly those, or `add` and `remove`
  over the default: `coding`, `tool-search`, and `discord` when Carillon has
  a Discord provider. The built-in extensions are `coding` (read, write, edit
  and bash), `codemode`, `tool-search` and `discord`.

- user extensions it lists as `extensions/<file>.js`, `.mjs` or `.ts`: a
  module whose default export is a pi-durable extension object, with its
  `tools`, `sections`, `hooks`, `wraps` and `tasks`. It is loaded from the
  snapshot and cannot import packages.

Settings or a listed module that cannot be used refuse the turn that brought
them with 422, as does a module pi-durable cannot install beside the built-in
extensions, such as one reusing a task name. A newer snapshot replaces the
installed user extensions. An old one stays while a live task needs one of
its task names that no installed module of the snapshot defines, and each
later admission adopts the snapshot again, so a module the kept one blocked
installs, and takes its tasks over, once the old one can go.
A recorded snapshot that no longer loads at startup is logged, and the agent
uses the default extensions until the next one is admitted.

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
- [`resources.ts`](src/resources.ts), [`attachments.ts`](src/attachments.ts),
  [`activity.ts`](src/activity.ts), [`models.ts`](src/models.ts): snapshots
  and settings, files, activity, and the catalogue.
- [`documents.ts`](src/documents.ts): Carillon's state in each agent's storage.
- [`cli.ts`](src/cli.ts): the service's arguments, credentials, built-in
  extensions and lifecycle.
- [`deterministic.ts`](src/deterministic.ts): the scripted models of
  `--deterministic`.
