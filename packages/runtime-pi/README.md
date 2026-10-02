# Pi runtime

Native Pi sessions as a constructible component. The public entry point is [`src/index.ts`](src/index.ts); importing it does not launch HTTP, load credentials, or discover providers.

```ts
import { createPiRuntime } from "@carillon/runtime-pi";

const runtime = await createPiRuntime({
  workspace: "/path/to/workspace",
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

The workspace must already exist. State includes Pi's `auth.json`, `settings.json`, `models.json`, catalogue cache, model selections and (by default) native sessions. Configure a usable default provider/model in Pi settings and credentials for that provider. Construction does not opt into catalogue network refresh unless `allowModelNetwork: true` is supplied. This is not a new login or credential system.

Carillon loads Pi's native `codemode`, `tool_search` and MCP extensions, with their normal replacement and disable semantics. They do not replace Pi's native tools or permissions. To enable code mode, add `"defaultTools": ["+codemode"]` to the agent resource root's `settings.json`; `codemode.mode` may be `"on"` (keep direct tools) or `"only"` (call tools through scripts). MCP uses Pi's native global agent directory (`PI_CODING_AGENT_DIR`, or Pi's normal default), plus trusted project configuration. The installed service sets that directory to the installation's `pi`, so its global configuration is `pi/mcp.json`, not the turn resource snapshot. The SDK alone does not load these extensions automatically. The release package includes Pi's native QuickJS setup and worker entries for both its CLI and the Carillon runtime.

Replace `modelRuntime`, `settings`, or `selections` independently when supplying your own concrete Pi/model-state collaborators. Omitted collaborators use the same defaults as the service. Supplied collaborators remain caller-owned. `sessionDirectory` permits a separate native-session location; the installed service uses its existing installation path.

`executeTurn` serializes execution within the instance. The tuple of interface, agent and audience identifies the native conversation; a new input in that conversation reopens its native history. Cancellation settles before another turn executes. `close` rejects new work, cancels active/queued execution, and waits for outstanding operations to settle. It does not erase sessions, replay inputs, or dispose caller-owned collaborators. For lower-level native session use, `openConversation`, `createPiExecutionSession`, and `promptSession` are the same primitives used by this runtime. A turn Pi reports as failed rejects with `ExplainedFailure` carrying Pi's failure text as is; the HTTP adapter decides what of it may be displayed.

A turn may carry `attachments`: files below `attachment_root`, named by relative path. [`attachments.ts`](src/attachments.ts) passes PNG, JPEG, WebP and GIF images within the inline policy (`inlineImages`; dimensions are read from headers with `image-size` before Pi decodes anything) to `session.prompt` as Pi's native images. A note listing every file travels with the message as a native custom message (`sendCustomMessage` with `deliverAs: "nextTurn"`), or is kept on its own when a command or extension consumes the message, and a per-turn native extension puts the current attachment directory in the system prompt while the conversation has such notes. Other formats, including ones `image-size` recognizes loosely, are files. The service's `providers/<id>.toml` is checked strictly: an unknown or mistyped setting refuses startup. The text is passed unchanged, so commands and input handlers keep their meaning. Pi normalizes the images for the model that runs and gives text-only models its own placeholder. The installed service reads `inline_images` limits from its optional `providers/<id>.toml`. The release ships Photon's WebAssembly beside the compiled programs, where Pi's compiled image resizing looks for it.

While a turn executes, `activity(correlation_id, interface_id)` returns a bounded snapshot of what it is doing, derived from Pi's native session events, or `undefined` for a turn that is not executing for that interface. The snapshot exists from before session preparation until the turn settles; it is never stored. Tool inputs are described in [`activity.ts`](src/activity.ts), beside the tools' semantics, and pass the same best-effort redaction as diagnostics. The HTTP adapter serves it at `GET /v1/turns/{id}/activity?interface_id=…`.

`ModelSelections` reads the selection file as the authority, not a permanently cached
copy. Its asynchronous reads refresh from disk; updates hold a short cross-process
`proper-lockfile` lease around refresh, precondition checking and atomic persistence.
Independent handles therefore preserve each other's acknowledged writes. Contention
is bounded and reported as a failure, not a successful overwritten update. This is
selection-file coordination only: it does not make concurrent runtime instances safe
to execute against the same native sessions.

## Try it

```sh
mise run example:pi -- --state-dir /path/to/pi-state --workspace /path/to/workspace --resources /path/to/resources
```

[`examples/session.ts`](examples/session.ts) performs two real prompts in one native session, without Discord or Carillon HTTP. This command needs configured model credentials and may incur model costs; it also exposes Pi's native tools under the invoking Unix user's authority. Use a suitable workspace. The automated runtime test exercises this exact example with Pi's real session adapter and a controlled model stream: it verifies history, not a live model's reasoning or network service.

## Find the owner

- [`runtime.ts`](src/runtime.ts): instance construction, model operations, execution queue and shutdown.
- [`session.ts`](src/session.ts), [`namespace.ts`](src/namespace.ts): native Pi sessions/resources/cancellation and conversation identity.
- [`model.ts`](src/model.ts): persisted selection policy; shared wire shapes come from [`@carillon/protocol`](../protocol/README.md).
- [`activity.ts`](src/activity.ts): a running turn's snapshot and the deny-by-default description of tool inputs, including the Bash command-header policy.
- [`failure.ts`](src/failure.ts): `ExplainedFailure` and the conservative, best-effort rules for what of an explanation may be displayed and how diagnostics are redacted.
- [`http.ts`](src/http.ts): authenticated transport over runtime operations. It sends callers only the displayable part of `ExplainedFailure` messages, and logs other failures and runtime-side explanations by method and path.
- [`cli.ts`](src/cli.ts): installation arguments, credentials, OAuth registration, listener and signal lifecycle.
- [`testing.ts`](src/testing.ts): explicit deterministic **service-test substitute**, not Pi execution. Production code does not branch through it during a turn.
