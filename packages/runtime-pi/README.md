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
  });
  console.log(answer.output);
} finally {
  await runtime.close();
}
```

The workspace must already exist. State includes Pi's `auth.json`, `settings.json`, `models.json`, catalogue cache, model selections and (by default) native sessions. Configure a usable default provider/model in Pi settings and credentials for that provider. Construction does not opt into catalogue network refresh unless `allowModelNetwork: true` is supplied. This is not a new login or credential system.

Replace `modelRuntime`, `settings`, or `selections` independently when supplying your own concrete Pi/model-state collaborators. Omitted collaborators use the same defaults as the service. Supplied collaborators remain caller-owned. `sessionDirectory` permits a separate native-session location; the installed service uses its existing installation path.

`executeTurn` serializes execution within the instance. The tuple of interface, agent and audience identifies the native conversation; a new input in that conversation reopens its native history. Cancellation settles before another turn executes. `close` rejects new work, cancels active/queued execution, and waits for outstanding operations to settle. It does not erase sessions, replay inputs, or dispose caller-owned collaborators. For lower-level native session use, `openConversation`, `createPiExecutionSession`, and `promptSession` are the same primitives used by this runtime.

## Try it

```sh
mise run example:pi -- --state-dir /path/to/pi-state --workspace /path/to/workspace --resources /path/to/resources
```

[`examples/session.ts`](examples/session.ts) performs two real prompts in one native session, without Discord or Carillon HTTP. This command needs configured model credentials and may incur model costs; it also exposes Pi's native tools under the invoking Unix user's authority. Use a suitable workspace. The automated runtime test exercises this exact example with Pi's real session adapter and a controlled model stream: it verifies history, not a live model's reasoning or network service.

## Find the owner

- [`runtime.ts`](src/runtime.ts): instance construction, model operations, execution queue and shutdown.
- [`session.ts`](src/session.ts), [`namespace.ts`](src/namespace.ts): native Pi sessions/resources/cancellation and conversation identity.
- [`model.ts`](src/model.ts): persisted selection policy; shared wire shapes come from [`@carillon/protocol`](../protocol/README.md).
- [`http.ts`](src/http.ts): authenticated transport over runtime operations.
- [`cli.ts`](src/cli.ts): installation arguments, credentials, OAuth registration, listener and signal lifecycle.
- [`testing.ts`](src/testing.ts): explicit deterministic **service-test substitute**, not Pi execution. Production code does not branch through it during a turn.
