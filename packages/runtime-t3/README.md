# T3 runtime

A Carillon runtime provider that executes turns on a [T3 Code](https://github.com/pingdotgg/t3code) server instead of Pi. It is a separate execution runtime behind the same runtime boundary core already speaks: `POST /v1/turns`, `GET /v1/turns/{id}/activity`, `GET /v1/environment`, `GET`/`PUT /v1/model` and `/health`, authenticated with the installation credential and fail-closed before any T3 work. The public entry point is [`src/index.ts`](src/index.ts); importing it opens no connection.

```ts
import { createT3Runtime } from "@carillon/runtime-t3";

const runtime = createT3Runtime({
  endpoint: { url: "http://127.0.0.1:3773", token },
  projectId: "…",
  selection: { instanceId: "claudeAgent", model: "claude-fable-5-1", options: [{ id: "effort", value: "high" }] },
  workspace: "/path/to/workspace",
});
const answer = await runtime.executeTurn({ correlation_id: "turn-1", interface_id: "discord", agent_id: "default", audience_id: "conversation-1", input: "Hello", resource_root: "…", attachment_root: "…" });
```

## Ownership

Carillon owns admission, turn identity and delivery. T3 owns execution, its transcript, jobs and provider policy. The runtime stores nothing: every T3 identifier it needs is derived from Carillon's own facts ([`contracts.ts`](src/contracts.ts)):

- one T3 thread per conversation (project, interface, agent, audience), opened by a `thread.create` command whose ID is derived from the thread's, so opening it again replays the command's receipt instead of creating another thread;
- one T3 message per Carillon turn, dispatched by a `message.dispatch` command derived from the turn's correlation ID. T3 keeps a receipt per command, so the same turn can never produce two runs;
- the turn's run is the one whose `userMessageId` is the turn's message.

## How a turn runs

`executeTurn` ([`runtime.ts`](src/runtime.ts)) refuses attached files (not supported yet), opens one WebSocket to T3 ([`t3.ts`](src/t3.ts)), ensures the thread, subscribes to it and reads the snapshot. If the snapshot has no run for this turn, any run an earlier turn left active is interrupted first (Carillon considers those turns over), then the message is dispatched. From then on the subscription drives a pure fold of the thread's state ([`watch.ts`](src/watch.ts)) until the outcome is known.

**Completion** requires more than a settled session. The turn owns its run and every run T3 starts after it, such as the wake that delivers finished background work (Carillon dispatches nothing to the thread until the turn settles, so later runs belong to it). The last of those runs must be `completed`, no runtime request may be pending, and the provider thread must list no pending background work; after background work was seen, completion waits a short grace (`wakeGraceMs`, three seconds) for T3's wake run before it is final. The answer is those runs' assistant messages in order; an empty answer fails the turn. A `failed` run is explained with T3's own failure text, which passes the shared display boundary of the Pi package before it reaches a conversation. A run someone else stopped, or one waiting for an approval or answer this runtime cannot give, is reported as an actionable failure: the question or request kind is named, the run is interrupted so the thread is usable again, and the person is asked to continue with the answer included.

**Reconnection.** A lost connection, or a lost answer to a command, never repeats work. The next connection reads the thread back; a run that exists is adopted, and only a turn T3 has no run for is dispatched again, under the same command ID. Reconnections are bounded; when they run out the turn fails naming the native run that may still be running.

**Cancellation.** When core disconnects (the HTTP request closes) or the runtime closes, the runtime interrupts the turn's run with `run.interrupt`, waits for T3 to report it stopped, and logs how many tool calls had started. Those are not undone. A cancellation that arrives while the dispatch is in flight waits for the dispatch's answer or the run's appearance before interrupting. If T3 does not confirm the stop in time, the runtime logs that the run may still be running; the next turn in the conversation interrupts it before dispatching.

**Activity.** `activity(correlation_id, interface_id)` answers only for the interface the turn serves. It is derived from T3's timeline items: running and recently finished `command_execution`, `file_change`, `file_search`, `web_search`, `dynamic_tool` and `subagent` items, pending background work, and the first line of the model's latest public message. Commands and file paths are described with the Pi package's deny-by-default descriptions and credential redaction; reasoning items and tool output are never shown.

## Protocol

The runtime speaks T3 orchestration protocol 2 only. Before sending a credential it checks T3's discovery answer and refuses any other version. The upgrade uses `/ws?orchestrationProtocol=2` with a bearer token T3 issued (`t3 auth session issue`); the token is read from a file and never passed on the command line or logged. Every answer is validated against the subset of T3's contracts the runtime consumes; an answer outside those shapes fails the turn as "a form this runtime does not understand", never a guess.

## Settings

The service reads the registry for its own entry and `providers/<id>.toml`, checked strictly:

```toml
url = "http://127.0.0.1:3773"           # T3's HTTP origin (https for a tailnet)
token_file = "credentials/t3.token"     # relative to the installation
project_id = "…"                        # the T3 project whose workspace is the agent's
title = "Carillon"                      # optional thread title prefix

[model]
instance_id = "claudeAgent"
model = "claude-fable-5-1"

[model.options]
effort = "high"
```

The model selection is fixed by this file: `GET /v1/model` offers it as the only choice and `PUT /v1/model` is refused with an explanation. Run it from the repository with:

```sh
bun packages/runtime-t3/src/cli.ts --config "$CARILLON_HOME/carillon.toml" --provider runtime-t3 --token-file "$CARILLON_HOME/credentials/runtime-t3.token"
```

Startup fails closed: the registry entry must exist and be enabled, the installation credential must match, the settings file must be valid, and T3 must answer discovery with protocol 2 and accept the token.

## Resource snapshot

T3 has no system-prompt facility for a thread. The snapshot's `SYSTEM.md` and `AGENTS.md` travel as a delimited preamble of the thread's first message; later snapshots do not update an existing thread. The agent's workspace is the T3 project's workspace root, so the provider's own instruction files there apply natively. The snapshot's Pi extensions and skills are not loaded. Files attached to a turn are refused with an explanation; files of earlier turns that core offers again are named in the message as unavailable, so the model and the person know.

## Tests

[`fake.ts`](src/fake.ts) is a test double of the parts of T3 this runtime uses, with injectable faults: dropped answers, dropped requests, delayed answers, rejections, malformed frames, ignored interrupts and disconnections. [`runtime.test.ts`](src/runtime.test.ts) proves continuity, lost-receipt recovery without a second run, reconnection, cancellation including the in-flight dispatch race and stale-run sweep, blocked and failed runs, background-work completion, activity, fail-closed transport failures and attachment refusal. [`watch.test.ts`](src/watch.test.ts) and [`t3.test.ts`](src/t3.test.ts) cover the pure fold and the client.
