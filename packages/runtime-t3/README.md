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

`executeTurn` ([`runtime.ts`](src/runtime.ts)) refuses attached files (not supported yet), opens one WebSocket to T3 ([`t3.ts`](src/t3.ts)), ensures the thread and subscribes to it, asking for T3's bounded snapshot (recent messages and items; runs, requests, provider threads and sessions whole) and a completion marker. If the thread has no run for this turn, any run an earlier turn left active is interrupted first (Carillon considers those turns over), then the message is dispatched. From then on the subscription drives a bounded fold of the thread's state ([`watch.ts`](src/watch.ts)) until the outcome is known. The fold keeps only the turn's own facts: its run and the runs T3 started after it, earlier runs still active, the tools running now and the last few finished, open requests, the turn's assistant messages, the provider thread's roster and session. History outside the turn is dropped as it arrives, and the turn's run is kept as its identity even if a later snapshot does not list it. T3 sends a run's provider thread record just before `run.created`, and its session record just before the thread record that names it when the turn starts, so the fold holds the few most recent provider records it cannot yet attribute and adopts them once the run or thread refers to them. A session record the fold has not seen is unknown, never detached: detachment is T3's own `provider-session.detached`, a thread that names no session, or a snapshot that omits the session a turn thread names.

**Completion** requires more than a settled session, and is decided from T3's state alone, never from a timer. The turn owns its run and every run T3 starts after it, such as the wake that delivers finished background work (Carillon dispatches nothing to the thread until the turn settles, so later runs belong to it). The last of those runs must be `completed`, no request of the turn may be open (whatever the run's status), and the provider thread must list no pending background work. A run that completed while its roster still listed work owes a follow-up: Claude delivers task notifications inside a running turn, so work that was still listed at completion is reported in a run of its own, and the turn waits for a later run however long it takes. T3 projects no "wake pending" fact; if T3 reports the provider session stopped, the owed report cannot come and the turn fails with that explanation; if the session was merely detached, T3 removes its record before the process closes and does not fence what it may still deliver, so the turn fails saying that T3 does not confirm whether the result will still arrive. The facts "work was listed for this turn's run" and "the run completed while work was listed" survive snapshots: a resumed subscription that only shows a completed run with an empty roster cannot tell whether the result reached the running turn or is still owed, so the turn fails closed as unresolved, after the same stop, rather than report the run's text as the answer. The turn's request items also survive snapshots, so a request T3 still lists pending keeps its ownership and wording even when the bounded history window omits its item. The answer is those runs' assistant messages in order; an empty answer fails the turn. A `failed` run is explained with T3's own failure text, which passes the shared display boundary of the Pi package before it reaches a conversation. A run someone else stopped, or one waiting for an approval or answer this runtime cannot give, is reported as an actionable failure: the question or request kind is named, the run is interrupted so the thread is usable again, and the explanation says only what T3 confirmed: that the run stopped, that it ended but the request is still open, or that T3 did not confirm the stop.

**Reconnection.** A lost connection, or a lost answer to a command, never repeats work. The next connection resumes the subscription after the last event sequence the fold saw, so T3 replays only what was missed (or sends a bounded snapshot when the gap is too old); a run that exists is adopted, and only a turn T3 has no run for is dispatched again, under the same command ID. Reconnections are bounded; when they run out the turn fails naming the native run that may still be running.

**Cancellation.** When core disconnects (the HTTP request closes) or the runtime closes, the runtime sends `run.interrupt` for the active run, or for the thread's latest run when only background work remains (T3 settles a stopped thread's background work through its latest run), waits for T3 to report no active run, an empty roster and no report still owed by a live process, and logs how many tool calls had started. When nothing runs but the process still owes a report (the roster cleared before its wake), T3 refuses the interrupt; the runtime then requests the public `provider-session.detach`, the only stop T3 offers for that state. T3 accepts the request, removes the session's record and closes the process afterwards, without confirming the closure or fencing a wake the process may still deliver, so the runtime reports that stop as unconfirmed and never as ended. The next turn starts a new session on the same native conversation and interrupts any run it finds active. Those are not undone. A cancellation that arrives while the dispatch is in flight waits for the dispatch's answer or the run's appearance before interrupting. If T3 does not confirm the stop in time, the runtime says so rather than claiming a stop; the next turn in the conversation interrupts any run it finds active before dispatching, which is a sweep of T3's projection, not a guarantee about the native process.

**Activity.** `activity(correlation_id, interface_id)` answers only for the interface the turn serves. It is derived from T3's timeline items: running and recently finished `command_execution`, `file_change`, `file_search`, `web_search`, `dynamic_tool` and `subagent` items, pending background work, and the first line of the model's latest public message. Each answer is assembled from the bounded state the fold keeps, not from a scan of the thread. Commands and file paths are described with the Pi package's deny-by-default descriptions and credential redaction; reasoning items and tool output are never shown.

## Protocol

The runtime speaks T3 orchestration protocol 2 only. Before sending a credential it checks T3's discovery answer and refuses any other version. The upgrade uses `/ws?orchestrationProtocol=2` with a bearer token T3 issued (`t3 auth session issue`); the token is read from a file and never passed on the command line or logged. Every answer is validated against the subset of T3's contracts the runtime consumes ([`contracts.ts`](src/contracts.ts)): timeline kinds are discriminated, each with the fields the runtime reads required, so a known kind missing them fails the turn as "a form this runtime does not understand", never a guess; kinds the runtime does not read are ignored, not retained.

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

[`fake.ts`](src/fake.ts) is a test fixture: a double of the parts of T3 this runtime uses, with sequence-based replay, completion markers, provider sessions, T3's settlement of background work through the latest run, and injectable faults (dropped answers, dropped requests, delayed answers, rejections, malformed frames, ignored interrupts, snapshot-only resumes, contradictory open requests and disconnections). [`runtime.test.ts`](src/runtime.test.ts) proves continuity, lost-receipt recovery without a second run, reconnection by replay and by snapshot, cancellation including the in-flight dispatch race, background work left by a completed run and the stale-run sweep, blocked runs with truthful stop reports, requests still open after a terminal run, late wakes and a session that ends before its wake, failed runs, activity, malformed known timeline kinds, fail-closed transport failures and attachment refusal. [`watch.test.ts`](src/watch.test.ts) covers the bounded fold and [`t3.test.ts`](src/t3.test.ts) the client.
