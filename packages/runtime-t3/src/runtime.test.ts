import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { RuntimeOperations } from "@carillon/runtime-pi/operations";
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

function build(overrides: { url?: string; token?: string } = {}) {
  return createT3Runtime({
    endpoint: { url: overrides.url ?? fake.url, token: overrides.token ?? fake.token },
    projectId: PROJECT,
    selection,
    workspace: resources,
    reconnect: { attempts: 5, delayMs: 50 },
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

it("keeps a conversation's turns in one T3 thread and sends the agent's instructions once", async () => {
  const first = runtime.executeTurn(turn("turn-1", "Remember carillon."));
  answer(await started("turn-1"), "Noted.");
  expect(await first).toEqual({ correlation_id: "turn-1", output: "Noted." });

  const second = runtime.executeTurn(turn("turn-2", "Which word?"));
  answer(await started("turn-2"), "Carillon.");
  expect((await second).output).toBe("Carillon.");

  expect(fake.threads.size).toBe(1);
  expect(thread().runs.map((run) => run.ordinal)).toEqual([1, 2]);
  expect(userTexts()).toEqual([
    "[Agent instructions from Carillon]\nYou are the Carillon agent.\n[End of agent instructions]\n\nRemember carillon.",
    "Which word?",
  ]);
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

it("stops a stale run an ignored interrupt left behind before the next turn starts", async () => {
  fake.interruptBehaviour = "ignore";
  const controller = new AbortController();
  const result = runtime.executeTurn(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  controller.abort(new Error("cancelled"));
  await expect(result).rejects.toThrow("cancelled");
  expect(runOf("turn-1")?.status).toBe("running");

  fake.interruptBehaviour = "settle";
  const next = runtime.executeTurn(turn("turn-2", "again"));
  answer(await started("turn-2"), "ok");
  expect((await next).output).toBe("ok");
  expect(fake.interrupts).toEqual([run.id, run.id]);
  expect(runOf("turn-1")?.status).toBe("interrupted");
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
      "T3's native session ended before it reported the result of its finished background work.",
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

it("fails a turn on a known timeline kind that lacks the fields it consumes", async () => {
  const result = runtime.executeTurn(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.tool(threadId, run.id, { type: "user_input_request", requestId: "request-x" });
  await expect(result).rejects.toMatchObject({
    status: 502,
    message: "T3 answered in a form this runtime does not understand.",
  });
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
