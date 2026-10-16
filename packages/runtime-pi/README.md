# Pi runtime

Native [Pi](https://github.com/earendil-works/pi) sessions as a component.
Importing [`src/index.ts`](src/index.ts) starts nothing: no HTTP, credentials
or provider discovery.

```ts
import { createPiRuntime } from "@carillon/runtime-pi";

const runtime = await createPiRuntime({
  agents: [{ id: "assistant", workspace: "/path/to/workspace" }],
  stateDirectory: "/path/to/pi-state",
});
try {
  const answer = await runtime.executeTurn({
    correlation_id: "message-1",
    interface_id: "my-interface",
    agent_id: "assistant",
    audience_id: "conversation-1",
    input: "Hello",
    resource_root: "/path/to/resources",
    attachment_root: "/path/to/attachments",
  });
  console.log(answer.output);
} finally {
  await runtime.close();
}
```

The state directory holds Pi's `auth.json`, settings, model catalogue, model
selections and, by default, sessions; it needs a usable default model. Each
agent's workspace must exist; a turn of an agent the runtime does not serve
is refused. The service keeps sessions in its component directory. Pass
`modelRuntime`, `settings`, `selections` or `sessionDirectory` to supply your
own collaborators.

Service startup and `--check` require every agent's workspace and each declared
`resources` root to be an existing directory.

## Behaviour

- Turns run one at a time. Interface, agent and audience name the native
  conversation; each new input reopens its history. Cancelling settles before
  the next turn runs, and `close` cancels and waits without erasing sessions.
- A turn Pi reports as failed rejects with Pi's own failure text; the HTTP
  adapter decides what of it may be shown.
- Files arrive by path below `attachment_root`. PNG, JPEG, WebP and GIF
  images within the `inline_images` limits also go to Pi as images, checked
  from their headers before Pi decodes them. A note listing every file stays
  in the conversation's history. Text, including `/commands`, is passed as
  written.
- `activity(correlation_id, interface_id)` describes a running turn from Pi's
  session events: thinking notes, the model and context use, running and
  recent tool calls. It is never stored.
- Model selections live in `model-selection.json`, guarded by a cross-process
  lock so separate handles don't overwrite each other.
- Pi never hands core a follow-up cursor, so a follow-up turn is refused.
- Pi's `codemode`, `tool_search` and MCP extensions load with their normal
  semantics. Enable code mode with `"defaultTools": ["+codemode"]` in the
  agent's `settings.json`. The service's MCP configuration is the
  installation's `pi/mcp.json`, read as each turn starts rather than captured
  with it.
- The agent's resources add to what Pi finds itself: `AGENTS.md` or
  `CLAUDE.md` in the workspace and its parents, the workspace's `.pi/`,
  `.agents/skills` up to its git root, and `~/.agents/skills`. Ordinary Pi
  sessions never see the agent's resources.

The release compiles all of this into `carillon-runtime-pi`; run with `pi` as
its first argument, it is Pi's own command line (`carillon-pi`).

## Try it

```sh
mise run example:pi -- --state-dir /path/to/pi-state --workspace /path/to/workspace --resources /path/to/resources
```

[`examples/session.ts`](examples/session.ts) sends two real prompts in one
session. It needs model credentials, may cost money, and gives Pi's tools your
authority in that workspace.

## Where things live

- [`runtime.ts`](src/runtime.ts): construction, model operations, the turn
  queue and shutdown.
- [`session.ts`](src/session.ts), [`namespace.ts`](src/namespace.ts): native
  sessions, resources and conversation identity.
- [`attachments.ts`](src/attachments.ts), [`activity.ts`](src/activity.ts),
  [`../runtime/src/selections.ts`](../runtime/src/selections.ts): files,
  activity snapshots and selections.
- [`cli.ts`](src/cli.ts): the service's arguments, credentials and lifecycle.
- [`testing.ts`](src/testing.ts): a deterministic stand-in for service tests,
  not Pi.
