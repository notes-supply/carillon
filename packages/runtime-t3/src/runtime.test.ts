import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { RuntimeOperations } from "@carillon/runtime/operations";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { derivedId, type Run } from "./contracts.ts";
import { startFakeT3, type FakeT3 } from "./fake.ts";
import { createT3Runtime } from "./runtime.ts";

const PROJECT = "project-1";
const selection = {
  instanceId: "claudeAgent",
  model: "claude-fable-5-1",
  options: [{ id: "effort", value: "high" }],
};
const attachment = {
  path: `${"0".repeat(8)}-0000-0000-0000-${"0".repeat(12)}/0`,
  name: "a.png",
  size: 1,
  sha256: "0".repeat(64),
};
let fake: FakeT3;
let resources: string;
let runtime: RuntimeOperations;

beforeEach(async () => {
  fake = await startFakeT3();
  resources = await mkdtemp(join(tmpdir(), "carillon-t3-"));
  await writeFile(join(resources, "SYSTEM.md"), "You are the Carillon agent.\n");
  vi.spyOn(console, "log").mockImplementation(() => undefined);
  vi.spyOn(console, "error").mockImplementation(() => undefined);
  runtime = build();
});

afterEach(async () => {
  await runtime.close();
  await fake.close();
  await rm(resources, { recursive: true, force: true });
  vi.restoreAllMocks();
});

function build(overrides: { url?: string; token?: string; reconnectDelayMs?: number } = {}) {
  return createT3Runtime({
    endpoint: { url: overrides.url ?? fake.url, token: overrides.token ?? fake.token },
    projectId: PROJECT,
    selection,
    workspace: resources,
    reconnect: { attempts: 5, delayMs: overrides.reconnectDelayMs ?? 50 },
    settleMs: 500,
    timeoutMs: 2_000,
  });
}

const threadId = derivedId("thread", PROJECT, "discord-private", "default", "conversation-1");

function turn(correlation: string, input: string, extra: Record<string, unknown> = {}) {
  return {
    correlation_id: correlation,
    interface_id: "discord-private",
    agent_id: "default",
    audience_id: "conversation-1",
    input,
    resource_root: resources,
    attachment_root: join(resources, "attachments"),
    ...extra,
  };
}

function thread() {
  const found = fake.threads.get(threadId);
  if (found === undefined) throw new Error("the conversation's thread does not exist");
  return found;
}

function runOf(correlation: string): Run | undefined {
  return fake.threads
    .get(threadId)
    ?.runs.find((run) => run.userMessageId === derivedId("message", correlation));
}

async function started(correlation: string): Promise<Run> {
  await fake.waitFor(() => runOf(correlation) !== undefined);
  const run = runOf(correlation);
  if (run === undefined) throw new Error("run not started");
  return run;
}

/** The turn's run once it has the status. */
async function waitForStatus(correlation: string, status: Run["status"]): Promise<Run> {
  await fake.waitFor(() => runOf(correlation)?.status === status);
  const run = runOf(correlation);
  if (run === undefined) throw new Error("run not started");
  return run;
}

/** The runtime's thread subscription after a reconnection. */
async function resubscribed(): Promise<void> {
  await fake.waitFor(() => thread().subscribers.size === 1);
}

function answer(run: Run, text: string): void {
  fake.assistant(threadId, run.id, text);
  fake.complete(threadId, run.id);
}

function userTexts(): string[] {
  return thread()
    .messages.filter((message) => message.role === "user")
    .map((message) => message.text);
}

const dispatches = () => fake.commands.filter((command) => command.type === "message.dispatch");
const activity = () => runtime.activity("turn-1", "discord-private");
const until = (condition: () => boolean) => fake.waitFor(condition);
const roster = () => thread().providerThread.pendingBackgroundTasks ?? [];
const pause = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

it("keeps a conversation's turns in one T3 thread and sends the person's text as written", async () => {
  const first = runtime.executeTurn(turn("turn-1", "Remember carillon."));
  answer(await started("turn-1"), "Noted.");
  expect(await first).toEqual({ correlation_id: "turn-1", output: "Noted." });

  const second = runtime.executeTurn(turn("turn-2", "Which word?"));
  answer(await started("turn-2"), "Carillon.");
  expect((await second).output).toBe("Carillon.");

  expect(fake.threads.size).toBe(1);
  expect(thread().runs.map((run) => run.ordinal)).toEqual([1, 2]);
  // The agent's instructions are the T3 project's own configuration, not a preamble.
  expect(userTexts()).toEqual(["Remember carillon.", "Which word?"]);
  // The thread was opened with the configured selection; messages carry none, so the
  // thread's own selection (which the native UI may change) runs them.
  expect(fake.commands.find((command) => command.type === "thread.create")).toMatchObject({
    modelSelection: { instanceId: "claudeAgent", model: "claude-fable-5-1" },
  });
  for (const dispatch of dispatches()) expect(dispatch).not.toHaveProperty("modelSelection");
  // The thread is opened by the same command each turn, replayed under its receipt.
  expect(fake.commands.filter((command) => command.type === "thread.create")).toHaveLength(2);
  // Each turn asks T3 for a bounded snapshot and a completion marker.
  expect(fake.subscriptions).toEqual([
    { afterSequence: undefined, bounded: true, marker: true },
    { afterSequence: undefined, bounded: true, marker: true },
  ]);
});

it("recovers a dispatch whose answer was lost by reading the run back, without dispatching again", async () => {
  let dropped = 0;
  fake.dropAnswer = (command) => command.type === "message.dispatch" && dropped++ === 0;
  const result = runtime.executeTurn(turn("turn-1", "hello"));
  const run = await started("turn-1");
  await resubscribed();
  answer(run, "hi");
  expect((await result).output).toBe("hi");
  expect(dispatches()).toHaveLength(1);
  expect(thread().runs).toHaveLength(1);
  // The reconnection resumed from the folded sequence instead of reloading the thread.
  expect(fake.subscriptions.map((subscription) => typeof subscription.afterSequence)).toEqual([
    "undefined",
    "number",
  ]);
});

it("replays a dispatch T3 never received under the same command, so one run results", async () => {
  let dropped = 0;
  fake.dropRequest = (command) => command.type === "message.dispatch" && dropped++ === 0;
  const result = runtime.executeTurn(turn("turn-1", "hello"));
  const run = await started("turn-1");
  answer(run, "hi");
  expect((await result).output).toBe("hi");
  const sent = fake.received.filter((command) => command.type === "message.dispatch");
  expect(sent).toHaveLength(2);
  expect(sent[0]?.commandId).toBe(sent[1]?.commandId);
  expect(dispatches()).toHaveLength(1);
  expect(thread().runs).toHaveLength(1);
});

it("resumes observing a running turn after the connection drops, keeping completion and identity", async () => {
  const result = runtime.executeTurn(turn("turn-1", "hello"));
  const run = await started("turn-1");
  const tool = fake.tool(threadId, run.id, { type: "command_execution", input: "cargo build" });
  await until(() => activity()?.tool_calls === 1);
  fake.disconnectAll();
  await resubscribed();
  fake.finishTool(threadId, tool);
  answer(run, "still here");
  expect((await result).output).toBe("still here");
  expect(thread().runs).toHaveLength(1);
  expect(dispatches()).toHaveLength(1);

  // The same holds when T3 answers the resume with a snapshot instead of a replay.
  fake.snapshotOnResume = true;
  const next = runtime.executeTurn(turn("turn-2", "again"));
  const second = await started("turn-2");
  fake.disconnectAll();
  await resubscribed();
  answer(second, "snapshot resume");
  expect((await next).output).toBe("snapshot resume");
  expect(typeof fake.subscriptions.at(-1)?.afterSequence).toBe("number");
  expect(dispatches()).toHaveLength(2);
});

it("interrupts the native run when Carillon cancels, waits for T3 to stop it, then serves the next turn", async () => {
  const controller = new AbortController();
  const result = runtime.executeTurn(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  fake.tool(threadId, run.id, { type: "command_execution", input: "sleep 60" });
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow("core disconnected");
  expect(fake.interrupts).toEqual([run.id]);
  expect(runOf("turn-1")?.status).toBe("interrupted");

  const next = runtime.executeTurn(turn("turn-2", "again"));
  answer(await started("turn-2"), "ok");
  expect((await next).output).toBe("ok");
  expect(fake.interrupts).toEqual([run.id]);
});

it("resolves a cancellation that arrives while the dispatch is still in flight", async () => {
  fake.delayAnswer = (command) => (command.type === "message.dispatch" ? 300 : 0);
  const controller = new AbortController();
  const result = runtime.executeTurn(turn("turn-1", "work"), controller.signal);
  await fake.waitFor(() => fake.received.some((command) => command.type === "message.dispatch"));
  expect(runOf("turn-1")).toBeUndefined();
  controller.abort(new Error("cancelled"));
  await expect(result).rejects.toThrow("cancelled");
  const run = runOf("turn-1");
  expect(run?.status).toBe("interrupted");
  expect(fake.interrupts).toEqual([run?.id]);
  expect(dispatches()).toHaveLength(1);
});

it("waits behind a run its ignored interrupt left running, without interrupting it again", async () => {
  fake.interruptBehaviour = "ignore";
  const controller = new AbortController();
  const result = runtime.executeTurn(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  controller.abort(new Error("cancelled"));
  await expect(result).rejects.toThrow("cancelled");
  expect(runOf("turn-1")?.status).toBe("running");

  fake.interruptBehaviour = "settle";
  const next = runtime.executeTurn(turn("turn-2", "again"));
  const queued = await started("turn-2");
  expect(queued.status).toBe("queued");
  expect(dispatches().at(-1)).toMatchObject({ dispatchMode: { type: "queue_after_active" } });
  await until(() => runtime.activity("turn-2", "discord-private")?.phase === "queued");
  // The earlier run finishes on its own; only then does the new message run.
  fake.complete(threadId, run.id);
  answer(await waitForStatus("turn-2", "running"), "ok");
  expect((await next).output).toBe("ok");
  expect(fake.interrupts).toEqual([run.id]);
});

it("reports a run someone else stopped, and the conversation continues", async () => {
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.stop(threadId, run.id, "interrupted");
  await expect(result).rejects.toMatchObject({
    status: 502,
    message: "The T3 run was interrupted before it answered.",
  });
  const next = runtime.executeTurn(turn("turn-2", "again"));
  answer(await started("turn-2"), "ok");
  expect((await next).output).toBe("ok");
});

it("stops a run that waits for an answer or approval this runtime cannot give, saying what was asked and what T3 confirmed", async () => {
  const asked = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.request(threadId, run.id, "user_input", {
    type: "user_input_request",
    questions: [{ header: "Scope", question: "Which file?" }],
  });
  await expect(asked).rejects.toMatchObject({
    status: 502,
    message:
      "T3 asked a question this runtime cannot relay yet (Scope: Which file?); the run was stopped. Send the answer in a new message.",
  });
  expect(fake.interrupts).toEqual([run.id]);
  expect(runOf("turn-1")?.status).toBe("interrupted");

  const approval = runtime.executeTurn(turn("turn-2", "more"));
  const second = await started("turn-2");
  fake.request(threadId, second.id, "tool_approval", {
    type: "approval_request",
    requestKind: "tool_approval",
  });
  await expect(approval).rejects.toMatchObject({
    status: 502,
    message:
      "T3 needs an approval (tool_approval) this runtime cannot give yet; the run was stopped. Send the answer in a new message.",
  });
});

it("does not claim a stop T3 did not confirm", async () => {
  fake.interruptBehaviour = "ignore";
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.request(threadId, run.id, "tool_approval", {
    type: "approval_request",
    requestKind: "tool_approval",
  });
  await expect(result).rejects.toMatchObject({
    status: 502,
    message:
      "T3 needs an approval (tool_approval) this runtime cannot give yet; T3 has not confirmed that the run stopped, so it may still be waiting. Send the answer in a new message.",
  });
  expect(fake.interrupts).toEqual([run.id]);
  expect(runOf("turn-1")?.status).toBe("waiting");
});

it("treats a request still open after the run ended as blocking, not as success", async () => {
  fake.retainRequests = true;
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.assistant(threadId, run.id, "Approve this?");
  fake.request(threadId, run.id, "tool_approval", {
    type: "approval_request",
    requestKind: "tool_approval",
  });
  await until(() => runOf("turn-1")?.status === "waiting");
  // T3 contradicts itself: the run ends yet its own approval stays pending.
  fake.stop(threadId, run.id, "completed");
  await expect(result).rejects.toMatchObject({
    status: 502,
    message:
      "T3 needs an approval (tool_approval) this runtime cannot give yet; the run has ended but T3 still lists the request as open. Send the answer in a new message.",
  });
  expect(fake.interrupts).toEqual([]);
});

it("explains a failed run with T3's failure text and refuses an empty answer", async () => {
  const failed = runtime.executeTurn(turn("turn-1", "work"));
  fake.fail(threadId, (await started("turn-1")).id, "Usage limit reached until 18:00");
  await expect(failed).rejects.toMatchObject({
    status: 502,
    message: "Usage limit reached until 18:00",
  });
  const empty = runtime.executeTurn(turn("turn-2", "work"));
  fake.complete(threadId, (await started("turn-2")).id);
  await expect(empty).rejects.toMatchObject({
    status: 502,
    message: "T3 finished the run without an answer.",
  });
});

it("waits for the run T3 starts after background work however late it comes, and answers with its text", async () => {
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command", description: "sleep 25" }]);
  fake.assistant(threadId, run.id, "Started it in the background.");
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools");
  expect(activity()).toMatchObject({
    phase: "tools",
    headline: "Started it in the background.",
    running: [{ tool: "command", detail: "sleep 25" }],
  });
  fake.background(threadId, []);
  // Well past the former three-second grace: the turn still waits for the result.
  await pause(3_500);
  expect(activity()).toMatchObject({ phase: "model", running: [] });
  const wake = fake.wake(threadId, 'Background command "sleep 25" completed (exit code 0)');
  const bash = fake.tool(threadId, wake.id, { type: "command_execution", input: "cat out.txt" });
  await until(() => activity()?.running[0]?.tool === "bash");
  fake.finishTool(threadId, bash);
  fake.assistant(threadId, wake.id, "It finished: tool-finished.");
  fake.complete(threadId, wake.id);
  expect((await result).output).toBe(
    "Started it in the background.\n\nIt finished: tool-finished.",
  );
  expect(thread().runs.map((candidate) => candidate.status)).toEqual(["completed", "completed"]);
  expect(dispatches()).toHaveLength(1);
}, 10_000);

it("fails closed when the native session ends before it reports finished background work", async () => {
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "monitor" }]);
  fake.assistant(threadId, run.id, "Watching.");
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools");
  fake.background(threadId, []);
  fake.session(threadId, "stopped");
  await expect(result).rejects.toMatchObject({
    status: 502,
    message:
      "T3's native session stopped before it reported the result of its finished background work.",
  });
});

it("stops background work left by a completed run when Carillon cancels, through T3's latest run", async () => {
  const controller = new AbortController();
  const result = runtime.executeTurn(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  const task = fake.tool(threadId, run.id, { type: "command_execution", input: "sleep 300 &" });
  fake.background(threadId, [{ taskId: "task-1", kind: "command", description: "sleep 300" }]);
  fake.assistant(threadId, run.id, "Running it in the background.");
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools" && runOf("turn-1")?.status === "completed");
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow("core disconnected");
  expect(fake.interrupts).toEqual([run.id]);
  expect(roster()).toEqual([]);
  expect(thread().items.find((item) => item.id === task.id)?.status).toBe("interrupted");

  const next = runtime.executeTurn(turn("turn-2", "again"));
  answer(await started("turn-2"), "ok");
  expect((await next).output).toBe("ok");
  expect(fake.interrupts).toEqual([run.id]);
});

it("interrupts the follow-up run when cancelled during T3's wake", async () => {
  const controller = new AbortController();
  const result = runtime.executeTurn(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  fake.complete(threadId, run.id);
  fake.background(threadId, []);
  const wake = fake.wake(threadId, "Background command completed");
  fake.tool(threadId, wake.id, { type: "command_execution", input: "cat out.txt" });
  await until(() => activity()?.running[0]?.tool === "bash");
  controller.abort(new Error("cancelled"));
  await expect(result).rejects.toThrow("cancelled");
  expect(fake.interrupts).toEqual([wake.id]);
  expect(thread().runs.map((candidate) => candidate.status)).toEqual(["completed", "interrupted"]);
});

it("describes the running turn to its own interface, bounded and redacted, from T3's timeline", async () => {
  fake.delayAnswer = (command) => (command.type === "message.dispatch" ? 200 : 0);
  const result = runtime.executeTurn(turn("turn-1", "work"));
  await until(() => activity() !== undefined);
  expect(activity()).toEqual({ phase: "starting", running: [], recent: [], tool_calls: 0 });
  const run = await started("turn-1");
  const command = fake.tool(threadId, run.id, {
    type: "command_execution",
    input: "cargo test -p carillon-core\nexport API_KEY=hunter2",
  });
  await until(() => activity()?.phase === "tools");
  const running = activity();
  expect(running).toMatchObject({
    phase: "tools",
    running: [{ tool: "bash", detail: "cargo test -p carillon-core …" }],
    recent: [],
    tool_calls: 1,
  });
  expect(typeof running?.running[0]?.started_at).toBe("number");
  fake.finishTool(threadId, command);
  const read = fake.tool(threadId, run.id, {
    type: "dynamic_tool",
    toolName: "Read",
    input: { file_path: join(resources, "SYSTEM.md") },
  });
  fake.finishTool(threadId, read, "failed");
  // Kinds this runtime does not consume are ignored, not retained.
  fake.tool(threadId, run.id, { type: "reasoning", text: "private thoughts", status: "completed" });
  fake.assistant(threadId, run.id, "Looking at the file.\nThen more.");
  await until(() => activity()?.tool_calls === 2 && activity()?.headline !== undefined);
  expect(activity()).toEqual({
    phase: "model",
    headline: "Looking at the file.",
    running: [],
    recent: [
      { tool: "bash", detail: "cargo test -p carillon-core …", ok: true },
      { tool: "Read", detail: "SYSTEM.md", ok: false },
    ],
    tool_calls: 2,
  });
  expect(runtime.activity("turn-1", "other-interface")).toBeUndefined();
  expect(JSON.stringify(activity())).not.toContain("hunter2");
  expect(JSON.stringify(activity())).not.toContain("private thoughts");
  answer(run, "Done.");
  await result;
  expect(activity()).toBeUndefined();
});

it("fails a turn on a known timeline kind that lacks the fields it consumes, naming the run that may still be running", async () => {
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.tool(threadId, run.id, { type: "user_input_request", requestId: "request-x" });
  // The answer cannot be read, but the run T3 already started is not shown stopped by that.
  await expect(result).rejects.toThrow(
    new RegExp(
      `^Lost contact with T3 while the turn ran \\(T3 sent a .* this runtime does not understand\\); native run ${run.id} may still be running\\.$`,
      "u",
    ),
  );
  await expect(result).rejects.toMatchObject({ status: 502 });
  expect(runOf("turn-1")?.status).toBe("running");
  // The exit left no confirmed stop: the conversation is fenced.
  await expect((await backgroundTurn("turn-2")).result).rejects.toThrow(
    /was not confirmed stopped/u,
  );
});

it("fails closed on an incompatible, unreachable or refusing server, and on answers it does not understand", async () => {
  fake.protocolVersion = 3;
  await expect(runtime.executeTurn(turn("turn-1", "hello"))).rejects.toMatchObject({
    status: 503,
    message:
      "The T3 server cannot be used: T3 speaks orchestration protocol 3; this runtime supports 2.",
  });
  fake.protocolVersion = 2;
  const refused = build({ token: "wrong" });
  await expect(refused.executeTurn(turn("turn-1", "hello"))).rejects.toThrow(
    /^The T3 server cannot be used: T3 refused the connection/u,
  );
  await refused.close();
  const unreachable = build({ url: "http://127.0.0.1:1" });
  await expect(unreachable.executeTurn(turn("turn-1", "hello"))).rejects.toThrow(
    /^The T3 server cannot be used: T3 is unreachable/u,
  );
  await unreachable.close();
  fake.malformed = true;
  await expect(runtime.executeTurn(turn("turn-1", "hello"))).rejects.toMatchObject({
    status: 502,
    message: "T3 answered in a form this runtime does not understand.",
  });
  fake.reject = (command) =>
    command.type === "message.dispatch" ? "Thread is archived." : undefined;
  await expect(runtime.executeTurn(turn("turn-1", "hello"))).rejects.toThrow(
    /^T3 refused the message: .*Thread is archived\.$/u,
  );
  expect(fake.threads.get(threadId)?.runs ?? []).toHaveLength(0);
});

it("refuses attached files and names earlier files it cannot provide", async () => {
  await expect(
    runtime.executeTurn(turn("turn-1", "see", { attachments: [attachment] })),
  ).rejects.toMatchObject({ status: 422 });
  expect(fake.threads.size).toBe(0);
  const result = runtime.executeTurn(
    turn("turn-2", "hello", {
      unanswered: [{ correlation_id: "turn-1", attachments: [attachment] }],
    }),
  );
  answer(await started("turn-2"), "hi");
  await result;
  expect(userTexts()[0]).toMatch(
    /\n\n\[Carillon: 1 file\(s\) attached to earlier messages are not available to this runtime: a\.png\]$/u,
  );
});

it("offers only the configured native selection and refuses to change it", async () => {
  const query = { interface_id: "discord-private", agent_id: "default", audience_id: "c" };
  const chosen = {
    provider: "claudeAgent",
    model: "claude-fable-5-1",
    options: { effort: "high" },
  };
  expect(await runtime.modelState(query)).toEqual({
    available: [
      {
        provider: "claudeAgent",
        model: "claude-fable-5-1",
        name: "claude-fable-5-1",
        options: [{ id: "effort", name: "effort", choices: ["high"], default: "high" }],
      },
    ],
    agent: { selection: chosen, overridden: false },
    conversation: { selection: chosen, overridden: false },
  });
  await expect(
    runtime.setModel({ ...query, scope: "agent", selection: { provider: "x", model: "y" } }),
  ).rejects.toMatchObject({ status: 422 });
});

it("stops the native run when the runtime closes mid-turn", async () => {
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  await runtime.close();
  await expect(result).rejects.toThrow("T3 runtime is closed");
  expect(fake.interrupts).toEqual([run.id]);
  expect(runOf("turn-1")?.status).toBe("interrupted");
});

it("requests a session detach when cancelled after the roster cleared, before the wake, and reports the stop as unconfirmed", async () => {
  const controller = new AbortController();
  const result = runtime.executeTurn(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command", description: "sleep 25" }]);
  fake.assistant(threadId, run.id, "Started it in the background.");
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools");
  fake.background(threadId, []);
  await until(() => activity()?.phase === "model");
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow("core disconnected");
  // The completed run cannot be interrupted; the only public stop left is a
  // session detach, whose acceptance is a receipt, not a confirmed stop.
  expect(fake.interrupts).toEqual([]);
  expect(fake.detached).toEqual([thread().session.id]);
  expect(thread().sessionDetached).toBe(true);
  const logged = vi.mocked(console.log).mock.calls.map((call) => String(call[0]));
  expect(logged.some((line) => line.includes("does not confirm that the process has closed"))).toBe(
    true,
  );
  expect(logged.some((line) => line.includes("T3 confirmed the stop"))).toBe(false);
  expect(logged.some((line) => line.includes("had already ended"))).toBe(false);

  // The detached process delivers its wake anyway; the next turn waits behind it in T3's queue.
  const wake = fake.wake(threadId, "Background command completed");
  const next = runtime.executeTurn(turn("turn-2", "again"));
  expect((await started("turn-2")).status).toBe("queued");
  fake.complete(threadId, wake.id);
  answer(await waitForStatus("turn-2", "running"), "ok");
  expect((await next).output).toBe("ok");
  expect(fake.interrupts).toEqual([]);
  expect(thread().runs.map((candidate) => candidate.status)).toEqual([
    "completed",
    "completed",
    "completed",
  ]);
});

it("does not report an owed wake as stopped when T3 does not confirm the session detach", async () => {
  fake.interruptBehaviour = "ignore";
  const controller = new AbortController();
  const result = runtime.executeTurn(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools");
  fake.background(threadId, []);
  await until(() => activity()?.phase === "model");
  controller.abort(new Error("cancelled"));
  await expect(result).rejects.toThrow("cancelled");
  expect(fake.detached).toEqual([thread().session.id]);
  const logged = vi.mocked(console.log).mock.calls.map((call) => String(call[0]));
  expect(logged.some((line) => /does not confirm|did not confirm/u.test(line))).toBe(true);
  expect(logged.some((line) => line.includes("T3 confirmed the stop"))).toBe(false);
});

it("fails closed when a snapshot resume cannot show whether background work was reported, without claiming a stop", async () => {
  fake.snapshotOnResume = true;
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command", description: "sleep 25" }]);
  await until(() => activity()?.phase === "tools");
  fake.disconnectAll();
  // Missed while disconnected: the run completes with the roster listed, then the roster empties.
  fake.assistant(threadId, run.id, "Started it in the background.");
  fake.complete(threadId, run.id);
  fake.background(threadId, []);
  await expect(result).rejects.toMatchObject({
    status: 502,
    message:
      "Carillon lost its connection to T3 while this turn ran background work, and T3's state does not show whether the result was delivered; T3 does not confirm that its native process has stopped, so a result may still arrive; the next turn interrupts any run it finds active. Send another message to continue.",
  });
  expect(fake.interrupts).toEqual([]);
  expect(fake.detached).toEqual([thread().session.id]);
  expect(fake.subscriptions.at(-1)?.afterSequence).toBeDefined();
});

it("resumes by replay through the roster transitions and still waits for the wake", async () => {
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  await until(() => activity()?.phase === "tools");
  fake.disconnectAll();
  fake.assistant(threadId, run.id, "Started it.");
  fake.complete(threadId, run.id);
  fake.background(threadId, []);
  await resubscribed();
  await pause(300);
  expect(activity()).toMatchObject({ phase: "model", running: [] });
  const wake = fake.wake(threadId, "Background command completed");
  fake.assistant(threadId, wake.id, "It printed done.");
  fake.complete(threadId, wake.id);
  expect((await result).output).toBe("Started it.\n\nIt printed done.");
  expect(fake.detached).toEqual([]);
});

it("completes normally when background work finished inside the running turn", async () => {
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  await until(() => activity()?.phase === "tools");
  fake.background(threadId, []);
  await until(() => activity()?.phase === "model");
  answer(run, "Done within the turn.");
  expect((await result).output).toBe("Done within the turn.");
  expect(fake.detached).toEqual([]);
});

it("keeps a request's ownership and wording through a bounded snapshot that omits its item", async () => {
  fake.interruptBehaviour = "ignore";
  fake.omitRequestItems = true;
  fake.snapshotOnResume = true;
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.request(threadId, run.id, "user_input", {
    type: "user_input_request",
    questions: [{ header: "Scope", question: "Which file?" }],
  });
  await until(() => fake.interrupts.length === 1);
  // While the ignored interrupt is awaited, the connection drops and resumes
  // through a snapshot whose history window lacks the request item.
  fake.disconnectAll();
  await resubscribed();
  await expect(result).rejects.toMatchObject({
    status: 502,
    message:
      "T3 asked a question this runtime cannot relay yet (Scope: Which file?); T3 has not confirmed that the run stopped, so it may still be waiting. Send the answer in a new message.",
  });
});

it("does not adopt a later run the turn does not owe, and the next turn waits behind it", async () => {
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  // A predecessor's late wake lands on the thread while this turn runs: it queues behind it.
  const stale = fake.wake(threadId, 'Background command "old sleep" completed (exit code 0)');
  expect(stale.status).toBe("queued");
  answer(run, "Only mine.");
  expect((await result).output).toBe("Only mine.");
  // The stale run now runs; it is not this turn's and is not awaited.
  expect(runOf("turn-1")?.status).toBe("completed");
  expect(thread().runs.find((candidate) => candidate.id === stale.id)?.status).toBe("running");
  fake.assistant(threadId, stale.id, "Old result.");
  expect((await result).output).toBe("Only mine.");

  const next = runtime.executeTurn(turn("turn-2", "again"));
  expect((await started("turn-2")).status).toBe("queued");
  fake.complete(threadId, stale.id);
  answer(await waitForStatus("turn-2", "running"), "ok");
  expect((await next).output).toBe("ok");
  expect(fake.interrupts).toEqual([]);
});

it("ignores a stale pending request until the new run's own item names its request", async () => {
  fake.retainRequests = true;
  const first = runtime.executeTurn(turn("turn-1", "first"));
  const run = await started("turn-1");
  answer(run, "done");
  expect((await first).output).toBe("done");
  // An earlier run's request is left pending with no item of the new turn naming it.
  fake.request(threadId, run.id, "tool_approval");
  fake.retainRequests = false;

  const second = runtime.executeTurn(turn("turn-2", "second"));
  const next = await started("turn-2");
  // The new run waits on its own request before its item has arrived: unknown, not blocked.
  const own = fake.request(threadId, next.id, "user_input");
  await until(() => runOf("turn-2")?.status === "waiting");
  await pause(300);
  expect(fake.interrupts).toEqual([]);
  expect(runtime.activity("turn-2", "discord-private")?.phase).toBe("waiting");
  // Its item arrives and names the request: blocked with the new turn's wording.
  fake.tool(threadId, next.id, {
    type: "user_input_request",
    requestId: own.id,
    questions: [{ header: "Scope", question: "Which file?" }],
  });
  await expect(second).rejects.toMatchObject({
    status: 502,
    message:
      "T3 asked a question this runtime cannot relay yet (Scope: Which file?); the run was stopped. Send the answer in a new message.",
  });
  expect(fake.interrupts).toEqual([next.id]);
});

it("cancels promptly while the first snapshot never arrives, dispatching nothing", async () => {
  fake.holdSnapshot = true;
  const controller = new AbortController();
  const result = runtime.executeTurn(turn("turn-1", "work"), controller.signal);
  await until(() => fake.subscriptions.length === 1);
  const started = Date.now();
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow("core disconnected");
  expect(Date.now() - started).toBeLessThan(1_000);
  expect(dispatches()).toHaveLength(0);
  fake.holdSnapshot = false;
  const next = runtime.executeTurn(turn("turn-2", "again"));
  answer(await started2("turn-2"), "ok");
  expect((await next).output).toBe("ok");
});

it("cancels promptly during a reconnect delay before anything was dispatched", async () => {
  const slow = build({ reconnectDelayMs: 5_000 });
  let dropped = 0;
  fake.dropAnswer = (command) => command.type === "thread.create" && dropped++ === 0;
  const controller = new AbortController();
  const result = slow.executeTurn(turn("turn-1", "work"), controller.signal);
  await until(() => dropped === 1);
  await pause(100);
  const started = Date.now();
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow("core disconnected");
  expect(Date.now() - started).toBeLessThan(1_000);
  expect(dispatches()).toHaveLength(0);
  await slow.close();
});

it("keeps recovering a possibly dispatched turn when cancelled during a reconnect delay", async () => {
  const slow = build({ reconnectDelayMs: 1_000 });
  let dropped = 0;
  fake.dropAnswer = (command) => command.type === "message.dispatch" && dropped++ === 0;
  const controller = new AbortController();
  const result = slow.executeTurn(turn("turn-1", "work"), controller.signal);
  const run = await started2("turn-1");
  await pause(100);
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow("core disconnected");
  // The abort did not abandon the native run: the reconnection found and interrupted it.
  expect(fake.interrupts).toEqual([run.id]);
  expect(runOf("turn-1")?.status).toBe("interrupted");
  await slow.close();
}, 10_000);

it("closes promptly while the first snapshot never arrives", async () => {
  fake.holdSnapshot = true;
  const result = runtime.executeTurn(turn("turn-1", "work"));
  await until(() => fake.subscriptions.length === 1);
  const started = Date.now();
  await runtime.close();
  await expect(result).rejects.toThrow("T3 runtime is closed");
  expect(Date.now() - started).toBeLessThan(1_000);
  expect(dispatches()).toHaveLength(0);
});

const started2 = started;

it("fails closed on any follow-up after an unconfirmed stop on the conversation, while plain turns continue", async () => {
  // Turn 1 is cancelled while waiting and T3 never confirms the stop.
  fake.interruptBehaviour = "ignore";
  const controller = new AbortController();
  const first = runtime.executeTurn(turn("turn-1", "first"), controller.signal);
  const run1 = await started("turn-1");
  controller.abort(new Error("core disconnected"));
  await expect(first).rejects.toThrow("core disconnected");
  fake.interruptBehaviour = "settle";
  fake.stop(threadId, run1.id, "interrupted");

  // Turn 2 owes a wake; the follow-up cannot be told from turn 1's late wake.
  const second = runtime.executeTurn(turn("turn-2", "second"));
  const run2 = await started("turn-2");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  fake.assistant(threadId, run2.id, "Started.");
  fake.complete(threadId, run2.id);
  await until(() => runtime.activity("turn-2", "discord-private")?.phase === "tools");
  fake.background(threadId, []);
  const wake = fake.wake(threadId, "Background command completed");
  fake.assistant(threadId, wake.id, "Whose result?");
  await expect(second).rejects.toThrow(/was not confirmed stopped/u);
  await expect(second).rejects.toMatchObject({ status: 502 });
  expect(fake.interrupts).toEqual([run1.id, wake.id]);

  // The fence is not lifted by inference: a plain turn still completes on its own text,
  // a turn that depends on a follow-up still ends unresolved.
  const third = runtime.executeTurn(turn("turn-3", "third"));
  answer(await started("turn-3"), "Plain answer.");
  expect((await third).output).toBe("Plain answer.");
  const fourth = runtime.executeTurn(turn("turn-4", "fourth"));
  const run4 = await started("turn-4");
  fake.background(threadId, [{ taskId: "task-2", kind: "command" }]);
  fake.assistant(threadId, run4.id, "Started again.");
  fake.complete(threadId, run4.id);
  await until(() => runtime.activity("turn-4", "discord-private")?.phase === "tools");
  fake.background(threadId, []);
  const own = fake.wake(threadId, "Background command completed");
  fake.assistant(threadId, own.id, "My result.");
  await expect(fourth).rejects.toThrow(/was not confirmed stopped/u);
  expect(fake.interrupts).toEqual([run1.id, wake.id, own.id]);
});

/**
 * Drives a turn that owes a wake, then its wake, and hands back the still
 * pending result: it completes, or ends unresolved when the thread is fenced.
 */
async function backgroundTurn(
  correlation: string,
  instance: RuntimeOperations = runtime,
): Promise<{ readonly result: Promise<{ output: string }> }> {
  const result = instance.executeTurn(turn(correlation, "work"));
  // The assertion attaches after the fake has been driven; keep an early rejection handled meanwhile.
  void result.catch(() => undefined);
  const run = await started(correlation);
  fake.background(threadId, [{ taskId: `task-${correlation}`, kind: "command" }]);
  fake.assistant(threadId, run.id, "Started.");
  fake.complete(threadId, run.id);
  await until(() => instance.activity(correlation, "discord-private")?.phase === "tools");
  fake.background(threadId, []);
  const wake = fake.wake(threadId, "Background command completed");
  fake.assistant(threadId, wake.id, "Result.");
  fake.complete(threadId, wake.id);
  return { result };
}

it("fences the conversation when the receipt of an accepted detach is lost", async () => {
  const controller = new AbortController();
  const result = runtime.executeTurn(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools");
  fake.background(threadId, []);
  await until(() => activity()?.phase === "model");
  // T3 processes the detach but its answer never arrives; recovery then reads the detached session back.
  fake.dropAnswer = (command) => command.type === "provider-session.detach";
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow(/was detached before it reported/u);
  expect(fake.detached).toEqual([thread().session.id]);
  await expect((await backgroundTurn("turn-2")).result).rejects.toThrow(
    /was not confirmed stopped/u,
  );
});

it("fences the conversation when reconnection is exhausted after a dispatch", async () => {
  const result = runtime.executeTurn(turn("turn-1", "work"));
  await started("turn-1");
  fake.protocolVersion = 3;
  fake.disconnectAll();
  await expect(result).rejects.toThrow(
    /^Lost contact with T3 while the turn ran .*may still be running\.$/u,
  );
  fake.protocolVersion = 2;
  await expect((await backgroundTurn("turn-2")).result).rejects.toThrow(
    /was not confirmed stopped/u,
  );
});

it("fences the conversation when a native failure does not prove a stop, but not after a confirmed stop or a completed turn", async () => {
  // A detached session while work is owed: a failure, not a stop.
  const detachedResult = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools");
  fake.background(threadId, []);
  thread().sessionDetached = true;
  fake.snapshotOnResume = true;
  fake.disconnectAll();
  await expect(detachedResult).rejects.toThrow(/was detached before it reported/u);
  await expect((await backgroundTurn("turn-2")).result).rejects.toThrow(
    /was not confirmed stopped/u,
  );
  fake.snapshotOnResume = false;

  // A fresh runtime: a confirmed interrupt and a completed turn leave the thread trusted.
  await runtime.close();
  runtime = build();
  const controller = new AbortController();
  const cancelled = runtime.executeTurn(turn("turn-3", "work"), controller.signal);
  const run3 = await started("turn-3");
  controller.abort(new Error("core disconnected"));
  await expect(cancelled).rejects.toThrow("core disconnected");
  expect(runOf("turn-3")?.status).toBe("interrupted");
  expect((await (await backgroundTurn("turn-4")).result).output).toBe("Started.\n\nResult.");
  expect(fake.interrupts).toEqual([run3.id]);
});

it("fences the conversation when a turn read back from an existing run exits without a confirmed stop", async () => {
  // Another runtime instance retries the same correlation while its run is active: it dispatches nothing.
  const first = runtime.executeTurn(turn("turn-1", "work"));
  await started("turn-1");
  const retry = build();
  const again = retry.executeTurn(turn("turn-1", "work"));
  // Both turns will fail; observe each rejection as it happens.
  const firstFails = expect(first).rejects.toThrow(
    /^Lost contact with T3 while the turn ran .*may still be running\.$/u,
  );
  const againFails = expect(again).rejects.toThrow(
    /^Lost contact with T3 while the turn ran .*may still be running\.$/u,
  );
  await until(() => fake.subscriptions.length === 2);
  expect(dispatches()).toHaveLength(1);
  fake.protocolVersion = 3;
  fake.disconnectAll();
  await againFails;
  await firstFails;
  fake.protocolVersion = 2;
  await expect((await backgroundTurn("turn-2", retry)).result).rejects.toThrow(
    /was not confirmed stopped/u,
  );
  await retry.close();
});

it("treats a cancel with a possibly accepted dispatch and no observed run as unconfirmed", async () => {
  const slow = build({ reconnectDelayMs: 1_000 });
  // The dispatch never reaches T3's handler and the socket drops; the caller then cancels.
  fake.dropRequest = (command) => command.type === "message.dispatch";
  const controller = new AbortController();
  const result = slow.executeTurn(turn("turn-1", "work"), controller.signal);
  await until(() => fake.received.some((command) => command.type === "message.dispatch"));
  await pause(100);
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow("core disconnected");
  fake.dropRequest = () => false;
  await expect((await backgroundTurn("turn-2", slow)).result).rejects.toThrow(
    /was not confirmed stopped/u,
  );
  await slow.close();
}, 10_000);

it("cancels or closes promptly while T3's discovery or upgrade stalls before any dispatch", async () => {
  fake.holdDiscovery = true;
  const controller = new AbortController();
  const result = runtime.executeTurn(turn("turn-1", "work"), controller.signal);
  await pause(100);
  let begun = Date.now();
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow("core disconnected");
  expect(Date.now() - begun).toBeLessThan(500);
  fake.holdDiscovery = false;

  fake.holdUpgrade = true;
  const closing = runtime.executeTurn(turn("turn-2", "work"));
  await pause(100);
  begun = Date.now();
  await runtime.close();
  await expect(closing).rejects.toThrow("T3 runtime is closed");
  expect(Date.now() - begun).toBeLessThan(500);
  fake.holdUpgrade = false;
  fake.disconnectAll();
  expect(fake.threads.size).toBe(0);
  expect(dispatches()).toHaveLength(0);
});

it("keeps reconnecting through a stalled discovery after the dispatch, and stops the run when it gets through", async () => {
  let dropped = 0;
  fake.dropAnswer = (command) => command.type === "message.dispatch" && dropped++ === 0;
  const controller = new AbortController();
  const result = runtime.executeTurn(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  // The reconnection's discovery stalls; the caller cancels meanwhile.
  fake.holdDiscovery = true;
  await pause(200);
  controller.abort(new Error("core disconnected"));
  let settled = false;
  void result.catch(() => undefined).finally(() => (settled = true));
  await pause(200);
  expect(settled).toBe(false);
  // Discovery answers again (the held request fails as on a restart): the run is found and interrupted.
  fake.holdDiscovery = false;
  fake.disconnectAll();
  await expect(result).rejects.toThrow("core disconnected");
  expect(fake.interrupts).toEqual([run.id]);
  expect(runOf("turn-1")?.status).toBe("interrupted");
});

it("stops background work a failed run left behind before releasing the queue, and says what T3 confirmed", async () => {
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  const task = fake.tool(threadId, run.id, { type: "command_execution", input: "sleep 300 &" });
  fake.background(threadId, [{ taskId: "task-1", kind: "command", description: "sleep 300" }]);
  await until(() => activity()?.phase === "tools");
  fake.fail(threadId, run.id, "Provider crashed");
  await expect(result).rejects.toMatchObject({
    status: 502,
    message: "Provider crashed; T3 confirmed that its background work was stopped.",
  });
  expect(fake.interrupts).toEqual([run.id]);
  expect(roster()).toEqual([]);
  expect(thread().items.find((item) => item.id === task.id)?.status).toBe("interrupted");
  // The stop was confirmed: the conversation stays trusted.
  expect((await (await backgroundTurn("turn-2")).result).output).toBe("Started.\n\nResult.");
  expect(fake.interrupts).toEqual([run.id]);

  // A run someone else stopped with work still listed goes the same way.
  const stopped = runtime.executeTurn(turn("turn-3", "work"));
  const run3 = await started("turn-3");
  fake.background(threadId, [{ taskId: "task-3", kind: "command" }]);
  await until(() => runtime.activity("turn-3", "discord-private")?.phase === "tools");
  fake.stop(threadId, run3.id, "cancelled");
  await expect(stopped).rejects.toThrow(
    "The T3 run was cancelled before it answered; T3 confirmed that its background work was stopped.",
  );
  expect(fake.interrupts).toEqual([run.id, run3.id]);
  expect(roster()).toEqual([]);
});

it("fences the conversation when the background work of a failed run is not confirmed stopped", async () => {
  fake.interruptBehaviour = "ignore";
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  await until(() => activity()?.phase === "tools");
  fake.fail(threadId, run.id, "Provider crashed.");
  await expect(result).rejects.toThrow(
    "Provider crashed; T3 has not confirmed that its background work stopped, so it may still be running; the next turn interrupts any run it finds active.",
  );
  expect(fake.interrupts).toEqual([run.id]);
  expect(roster()).toHaveLength(1);
  fake.interruptBehaviour = "settle";
  fake.background(threadId, []);
  await expect((await backgroundTurn("turn-2")).result).rejects.toThrow(
    /was not confirmed stopped/u,
  );
});

it("keeps the possibly-running warning when T3 cannot be reached again after the dispatch", async () => {
  // Discovery changes protocol after the run started: every reconnection is refused.
  const incompatible = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.protocolVersion = 3;
  fake.disconnectAll();
  await expect(incompatible).rejects.toThrow(
    `Lost contact with T3 while the turn ran (T3 speaks orchestration protocol 3; this runtime supports 2); native run ${run.id} may still be running.`,
  );
  await expect(incompatible).rejects.toMatchObject({ status: 502 });
  fake.protocolVersion = 2;
  // A fresh turn that never reached T3 still reports plain unavailability.
  fake.protocolVersion = 3;
  await expect(runtime.executeTurn(turn("turn-2", "work"))).rejects.toThrow(
    /^The T3 server cannot be used: T3 speaks orchestration protocol 3/u,
  );
  fake.protocolVersion = 2;

  // The server goes away entirely after a run was accepted.
  const unreachable = runtime.executeTurn(turn("turn-3", "work"));
  const run3 = await started("turn-3");
  await fake.close();
  await expect(unreachable).rejects.toThrow(
    new RegExp(
      `^Lost contact with T3 while the turn ran \\(T3 is unreachable: .*\\); native run ${run3.id} may still be running\\.$`,
      "u",
    ),
  );
});

it("clears the doubt on a refusal T3 holds a rejected receipt for, and keeps it on a defect", async () => {
  fake.reject = (command) =>
    command.type === "message.dispatch" ? "Thread is archived." : undefined;
  await expect(runtime.executeTurn(turn("turn-1", "hello"))).rejects.toThrow(
    /^T3 refused the message: .*Thread is archived\.$/u,
  );
  // The receipt was read back by replaying the same command once.
  expect(fake.received.filter((command) => command.type === "message.dispatch")).toHaveLength(2);
  expect(thread().runs).toHaveLength(0);
  fake.reject = () => undefined;
  // Nothing was committed: the next turn's follow-up is its own.
  expect((await (await backgroundTurn("turn-2")).result).output).toBe("Started.\n\nResult.");

  // An internal defect, on the dispatch and on the receipt read-back, shows
  // nothing about what was committed: the turn exits as possibly running
  // and the doubt stays.
  fake.defect = (command) => command.type === "message.dispatch";
  await expect(runtime.executeTurn(turn("turn-3", "hello"))).rejects.toThrow(
    /^Lost contact with T3 while the turn ran \(T3 failed the request\); native run .* may still be running\.$/u,
  );
  fake.defect = () => false;
  await expect((await backgroundTurn("turn-4")).result).rejects.toThrow(
    /was not confirmed stopped/u,
  );
});

it("reads the run back when T3 fails a dispatch it had already committed", async () => {
  let failed = 0;
  fake.failAfterDispatch = (command) =>
    command.type === "message.dispatch" && failed++ === 0 ? "publication failed" : undefined;
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  answer(run, "Committed after all.");
  expect((await result).output).toBe("Committed after all.");
  expect(fake.received.filter((command) => command.type === "message.dispatch")).toHaveLength(2);
  expect(thread().runs).toHaveLength(1);
  expect((await (await backgroundTurn("turn-2")).result).output).toBe("Started.\n\nResult.");
});

it("refuses a resumed snapshot that omits the run it observed, keeping the run possibly running", async () => {
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.snapshotOnResume = true;
  fake.omitRun = (candidate) => candidate.id === run.id;
  fake.disconnectAll();
  await expect(result).rejects.toThrow(
    `Lost contact with T3 while the turn ran (T3 sent a thread snapshot that omits run ${run.id} this turn observed); native run ${run.id} may still be running.`,
  );
  expect(runOf("turn-1")?.status).toBe("running");
  fake.omitRun = () => false;
  fake.snapshotOnResume = false;
  // The run finishes on its own. The exit left no confirmed stop, so the conversation is fenced.
  fake.complete(threadId, run.id);
  await expect((await backgroundTurn("turn-2")).result).rejects.toThrow(
    /was not confirmed stopped/u,
  );
  expect(fake.interrupts).toEqual([]);
});

it("keeps the answer text it observed when a bounded snapshot resumes without it", async () => {
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.assistant(threadId, run.id, "Part one.");
  await until(() => activity()?.headline === "Part one.");
  fake.snapshotOnResume = true;
  fake.messageWindow = 1;
  fake.disconnectAll();
  fake.assistant(threadId, run.id, "Part two.", false, "2");
  fake.complete(threadId, run.id);
  expect((await result).output).toBe("Part one.\n\nPart two.");
  expect(fake.subscriptions).toHaveLength(2);
});

it("queues behind a person's run in the native UI and cancels only its own queued run", async () => {
  const first = runtime.executeTurn(turn("turn-1", "hello"));
  answer(await started("turn-1"), "hi");
  await first;
  // The person starts a run in T3's own UI; it waits on their approval.
  const person = fake.wake(threadId, "a message typed in the T3 UI");
  fake.request(threadId, person.id, "tool_approval");
  expect(thread().runs.find((run) => run.id === person.id)?.status).toBe("waiting");

  const controller = new AbortController();
  const queued = runtime.executeTurn(turn("turn-2", "from Discord"), controller.signal);
  const own = await started("turn-2");
  expect(own.status).toBe("queued");
  expect(dispatches().at(-1)).toMatchObject({ dispatchMode: { type: "queue_after_active" } });
  await until(() => runtime.activity("turn-2", "discord-private")?.phase === "queued");
  controller.abort(new Error("core disconnected"));
  await expect(queued).rejects.toThrow("core disconnected");
  // Only the turn's own queued run was interrupted; the person's run still waits on them.
  expect(fake.interrupts).toEqual([own.id]);
  expect(runOf("turn-2")?.status).toBe("interrupted");
  expect(thread().runs.find((run) => run.id === person.id)?.status).toBe("waiting");
});

it("answers when the parent run completes and delivers the delegated report in a follow-up turn", async () => {
  const first = runtime.executeTurn(turn("turn-1", "delegate it"));
  const parent = await started("turn-1");
  const child = fake.delegate(threadId, parent.id, {
    title: "pong child",
    model: "claude-sonnet-5",
  });
  await until(
    () => runtime.activity("turn-1", "discord-private")?.work?.items[0]?.state === "running",
  );
  answer(parent, "DELEGATED");
  const reply = await first;
  expect(reply).toEqual({
    correlation_id: "turn-1",
    output: "DELEGATED",
    follow_up: { message: derivedId("message", "turn-1"), after: 1 },
  });

  // Core queues the follow-up turn with the cursor; it dispatches nothing and waits.
  const followUp = runtime.executeTurn(
    turn("turn-1-report", "", { native: { follow_up: reply.follow_up } }),
  );
  await until(() => runtime.activity("turn-1-report", "discord-private")?.phase === "waiting");
  expect(runtime.activity("turn-1-report", "discord-private")?.work?.items).toEqual([
    expect.objectContaining({ id: child.id, kind: "delegation", state: "running" }),
  ]);
  const continuation = fake.finishDelegation(threadId, child.id);
  fake.assistant(threadId, continuation.id, "PONG reported.");
  fake.complete(threadId, continuation.id);
  expect(await followUp).toEqual({ correlation_id: "turn-1-report", output: "PONG reported." });
  expect(dispatches()).toHaveLength(1);
  expect(fake.interrupts).toEqual([]);
});

it("stops nothing when a follow-up turn is cancelled: the continuation is T3's", async () => {
  const first = runtime.executeTurn(turn("turn-1", "delegate it"));
  const parent = await started("turn-1");
  const child = fake.delegate(threadId, parent.id, { title: "slow child" });
  answer(parent, "Working on it.");
  const reply = await first;
  const controller = new AbortController();
  const followUp = runtime.executeTurn(
    turn("turn-1-report", "", { native: { follow_up: reply.follow_up } }),
    controller.signal,
  );
  const continuation = fake.finishDelegation(threadId, child.id);
  await until(() => runtime.activity("turn-1-report", "discord-private")?.phase === "model");
  controller.abort(new Error("core disconnected"));
  await expect(followUp).rejects.toThrow("core disconnected");
  expect(fake.interrupts).toEqual([]);
  expect(thread().runs.find((run) => run.id === continuation.id)?.status).toBe("running");
  // Nor does it fence the conversation: a later turn's own follow-up is still trusted.
  fake.complete(threadId, continuation.id);
  expect((await (await backgroundTurn("turn-2")).result).output).toBe("Started.\n\nResult.");
});

it("sends a command invocation verbatim, without notes about earlier files", async () => {
  const earlier = [{ correlation_id: "turn-0", attachments: [attachment] }];
  const command = runtime.executeTurn(
    turn("turn-1", "/compact keep the decisions", {
      native: { intent: "command" },
      unanswered: earlier,
    }),
  );
  answer(await started("turn-1"), "Compacted.");
  await command;
  const prose = runtime.executeTurn(turn("turn-2", "hello", { unanswered: earlier }));
  answer(await started("turn-2"), "hi");
  await prose;
  expect(userTexts()).toEqual([
    "/compact keep the decisions",
    "hello\n\n[Carillon: 1 file(s) attached to earlier messages are not available to this runtime: a.png]",
  ]);
});

it("runs different conversations at once and each conversation's turns in order", async () => {
  const other = { audience_id: "conversation-2" };
  const otherThread = derivedId("thread", PROJECT, "discord-private", "default", "conversation-2");
  const otherRun = (correlation: string) =>
    fake.threads
      .get(otherThread)
      ?.runs.find((run) => run.userMessageId === derivedId("message", correlation));
  const first = runtime.executeTurn(turn("turn-1", "long work"));
  const long = await started("turn-1");
  // The same conversation's next turn waits for the first, in Carillon's order.
  const second = runtime.executeTurn(turn("turn-2", "next"));
  // Another conversation proceeds meanwhile.
  const unrelated = runtime.executeTurn(turn("turn-3", "elsewhere", other));
  await fake.waitFor(() => otherRun("turn-3") !== undefined);
  const elsewhere = otherRun("turn-3");
  if (elsewhere === undefined) throw new Error("other conversation did not start");
  fake.assistant(otherThread, elsewhere.id, "Done elsewhere.");
  fake.complete(otherThread, elsewhere.id);
  expect((await unrelated).output).toBe("Done elsewhere.");
  expect(runOf("turn-2")).toBeUndefined();
  expect(runOf("turn-1")?.status).toBe("running");
  answer(long, "Long done.");
  expect((await first).output).toBe("Long done.");
  answer(await started("turn-2"), "Next done.");
  expect((await second).output).toBe("Next done.");
  expect(fake.interrupts).toEqual([]);
});

it("cancels one conversation's turn without touching another's", async () => {
  const otherThread = derivedId("thread", PROJECT, "discord-private", "default", "conversation-2");
  const controller = new AbortController();
  const cancelled = runtime.executeTurn(turn("turn-1", "stop me"), controller.signal);
  const mine = await started("turn-1");
  const kept = runtime.executeTurn(turn("turn-2", "keep me", { audience_id: "conversation-2" }));
  await fake.waitFor(() => (fake.threads.get(otherThread)?.runs.length ?? 0) === 1);
  controller.abort(new Error("core disconnected"));
  await expect(cancelled).rejects.toThrow("core disconnected");
  const other = fake.threads.get(otherThread)?.runs[0];
  if (other === undefined) throw new Error("other conversation did not start");
  expect(fake.interrupts).toEqual([mine.id]);
  expect(other.status).toBe("running");
  fake.assistant(otherThread, other.id, "Still here.");
  fake.complete(otherThread, other.id);
  expect((await kept).output).toBe("Still here.");
});
