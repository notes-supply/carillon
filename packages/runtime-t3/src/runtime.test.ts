import { createHash } from "node:crypto";
import { once } from "node:events";
import { mkdir, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { expect, it, vi } from "vitest";
import { reportOf } from "@carillon/runtime/failure";
import { derivedId, type Run } from "./contracts.ts";
import {
  answer,
  build,
  conversation,
  core,
  dispatches,
  execute,
  fake,
  openings,
  readBack,
  resources,
  restart,
  runOf,
  runtime,
  settled,
  started,
  thread,
  threadId,
  turn,
  TurnFailed,
  UNCONFIRMED,
} from "./runtime/harness.ts";
import { connectT3 } from "./t3.ts";

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

function userTexts(): string[] {
  return thread()
    .messages.filter((message) => message.role === "user")
    .map((message) => message.text);
}

const activity = () => runtime.activity("turn-1", "discord-private");

const until = (condition: () => boolean) => fake.waitFor(condition);

/** What core received settling `turnId`. */
const settles = (turnId: string) =>
  core.received.filter(({ path }) => path === `/v1/runtime/turns/${turnId}/outcome`);

const roster = () => thread().providerThread.pendingBackgroundTasks ?? [];

const pause = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** An answer apart from the turn's final report, which cases about the answer leave aside. */
const answered = (response: { readonly activity?: unknown }) => ({
  ...response,
  activity: undefined,
});

it("gives T3 the person's text as written, without the interface's metadata", async () => {
  const origin = { guild_id: "1", channel_id: "2", message_id: "4", user_id: "5" };
  const first = execute(
    turn("turn-1", "React to my message.", {
      native: { origin, discord: { content: "raw provider payload", attachments: ["signed URL"] } },
    }),
  );
  answer(await started("turn-1"), "Done.");
  await first;
  expect(userTexts()).toEqual(["React to my message."]);
});

it("keeps native commands verbatim even when origin metadata is present", async () => {
  const result = execute(
    turn("turn-1", "/compact", { native: { intent: "command", origin: { channel_id: "2" } } }),
  );
  answer(await started("turn-1"), "Compacted.");
  await result;
  expect(userTexts()).toEqual(["/compact"]);
});

it("recovers a dispatch whose answer was lost by reading the run back, without dispatching again", async () => {
  let dropped = 0;
  fake.dropAnswer = (command) => command.type === "message.dispatch" && dropped++ === 0;
  const result = execute(turn("turn-1", "hello"));
  const run = await started("turn-1");
  await resubscribed();
  answer(run, "hi");
  expect((await result).output).toBe("hi");
  expect(dispatches()).toHaveLength(1);
  expect(thread().runs).toHaveLength(1);
  expect(fake.subscriptions[0]?.afterSequence).toBeUndefined();
  expect(typeof fake.subscriptions.at(-1)?.afterSequence).toBe("number");
});

it("replays a dispatch T3 never received under the same command, so one run results", async () => {
  let dropped = 0;
  fake.dropRequest = (command) => command.type === "message.dispatch" && dropped++ === 0;
  const result = execute(turn("turn-1", "hello"));
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
  const result = execute(turn("turn-1", "hello"));
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
  const next = execute(turn("turn-2", "again"));
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
  const result = execute(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  fake.tool(threadId, run.id, { type: "command_execution", input: "sleep 60" });
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow("core disconnected");
  expect(fake.interrupts).toEqual([run.id]);
  expect(runOf("turn-1")?.status).toBe("interrupted");

  const next = execute(turn("turn-2", "again"));
  answer(await started("turn-2"), "ok");
  expect((await next).output).toBe("ok");
  expect(fake.interrupts).toEqual([run.id]);
});

it("resolves a cancellation that arrives while the dispatch is still in flight", async () => {
  fake.delayAnswer = (command) => (command.type === "message.dispatch" ? 300 : 0);
  const controller = new AbortController();
  const result = execute(turn("turn-1", "work"), controller.signal);
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
  const result = execute(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  controller.abort(new Error("cancelled"));
  await expect(result).rejects.toThrow(UNCONFIRMED);
  expect(runOf("turn-1")?.status).toBe("running");

  fake.interruptBehaviour = "settle";
  const next = execute(turn("turn-2", "again"));
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
  const result = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.stop(threadId, run.id, "interrupted");
  await expect(result).rejects.toMatchObject({
    message: "The T3 run was interrupted before it answered.",
  });
  const next = execute(turn("turn-2", "again"));
  answer(await started("turn-2"), "ok");
  expect((await next).output).toBe("ok");
});

it("stops a run that waits for an answer or approval this runtime cannot give, saying what was asked and what T3 confirmed", async () => {
  const asked = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.request(threadId, run.id, "user_input", {
    type: "user_input_request",
    questions: [{ header: "Scope", question: "Which file?" }],
  });
  await expect(asked).rejects.toMatchObject({
    message:
      "T3 asked a question this runtime cannot relay yet (Scope: Which file?); the run was stopped. Send the answer in a new message.",
  });
  expect(fake.interrupts).toEqual([run.id]);
  expect(runOf("turn-1")?.status).toBe("interrupted");

  const approval = execute(turn("turn-2", "more"));
  const second = await started("turn-2");
  fake.request(threadId, second.id, "tool_approval", {
    type: "approval_request",
    requestKind: "tool_approval",
  });
  await expect(approval).rejects.toMatchObject({
    message:
      "T3 needs an approval (tool_approval) this runtime cannot give yet; the run was stopped. Send the answer in a new message.",
  });
});

it("does not claim a stop T3 did not confirm", async () => {
  fake.interruptBehaviour = "ignore";
  const result = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.request(threadId, run.id, "tool_approval", {
    type: "approval_request",
    requestKind: "tool_approval",
  });
  await expect(result).rejects.toMatchObject({
    message:
      "T3 needs an approval (tool_approval) this runtime cannot give yet; T3 has not confirmed that the run stopped, so it may still be waiting. Send the answer in a new message.",
  });
  expect(fake.interrupts).toEqual([run.id]);
  expect(runOf("turn-1")?.status).toBe("waiting");
});

it("treats a request still open after the run ended as blocking, not as success", async () => {
  fake.retainRequests = true;
  const result = execute(turn("turn-1", "work"));
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
    message:
      "T3 needs an approval (tool_approval) this runtime cannot give yet; the run has ended but T3 still lists the request as open. Send the answer in a new message.",
  });
  expect(fake.interrupts).toEqual([]);
});

it("explains a failed run with T3's failure text and refuses an empty answer", async () => {
  const failed = execute(turn("turn-1", "work"));
  fake.fail(threadId, (await started("turn-1")).id, "Usage limit reached until 18:00");
  await expect(failed).rejects.toMatchObject({
    message: "Usage limit reached until 18:00",
  });
  const empty = execute(turn("turn-2", "work"));
  fake.complete(threadId, (await started("turn-2")).id);
  await expect(empty).rejects.toMatchObject({
    message: "T3 finished the run without an answer.",
  });
});

it("waits for the run T3 starts after background work however late it comes, and answers with its text", async () => {
  const result = execute(turn("turn-1", "work"));
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
  const result = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "monitor" }]);
  fake.assistant(threadId, run.id, "Watching.");
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools");
  fake.background(threadId, []);
  fake.session(threadId, "stopped");
  await expect(result).rejects.toMatchObject({
    message:
      "T3's native session stopped before it reported the result of its finished background work.",
  });
});

it("interrupts the follow-up run when cancelled during T3's wake", async () => {
  const controller = new AbortController();
  const result = execute(turn("turn-1", "work"), controller.signal);
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
  const result = execute(turn("turn-1", "work"));
  await until(() => activity()?.phase === "starting");
  // A person may stop it while it runs.
  expect(activity()).toEqual({
    phase: "starting",
    running: [],
    recent: [],
    tool_calls: 0,
    controls: ["stop"],
  });
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
  fake.tool(threadId, run.id, { type: "todo_list", steps: [], explanation: "private plan" });
  // Reasoning T3 published for display becomes a note, apart from the answer.
  fake.tool(threadId, run.id, {
    type: "reasoning",
    text: "Reading the system file\nfirst, to see the agent's instructions.",
    status: "completed",
  });
  fake.assistant(threadId, run.id, "Looking at the file.\nThen more.");
  await until(() => activity()?.tool_calls === 2 && activity()?.headline !== undefined);
  const described = activity();
  expect(described).toEqual({
    phase: "model",
    headline: "Looking at the file.",
    running: [],
    recent: [
      {
        tool: "bash",
        detail: "cargo test -p carillon-core …",
        ok: true,
        duration_ms: described?.recent[0]?.duration_ms,
      },
      {
        tool: "Read",
        detail: "SYSTEM.md",
        ok: false,
        duration_ms: described?.recent[1]?.duration_ms,
      },
    ],
    tool_calls: 2,
    notes: ["Reading the system file first, to see the agent's instructions."],
    agent: { model: "claude-sonnet-5", effort: "low" },
    controls: ["stop"],
  });
  // T3 dated both ends of each call.
  expect(described?.recent.every((call) => typeof call.duration_ms === "number")).toBe(true);
  expect(runtime.activity("turn-1", "other-interface")).toBeUndefined();
  expect(JSON.stringify(activity())).not.toContain("hunter2");
  expect(JSON.stringify(activity())).not.toContain("private plan");
  answer(run, "Done.");
  await result;
  expect(activity()).toBeUndefined();
});

it("fails a turn on a known timeline kind that lacks the fields it consumes, naming the run that may still be running", async () => {
  const result = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.tool(threadId, run.id, { type: "user_input_request", requestId: "request-x" });
  // The answer cannot be read, but the run T3 already started is not shown stopped by that.
  await expect(result).rejects.toThrow(
    new RegExp(
      `^Lost contact with T3 while the turn ran \\(T3 sent a .* this runtime does not understand\\); native run ${run.id} may still be running\\.$`,
      "u",
    ),
  );
  await expect(result).rejects.toBeInstanceOf(TurnFailed);
  expect(runOf("turn-1")?.status).toBe("running");
  // The exit left no confirmed stop: the conversation is fenced.
  await expect((await backgroundTurn("turn-2")).result).rejects.toThrow(
    /was not confirmed stopped/u,
  );
});

it("fails closed on an incompatible, unreachable or refusing server, and on answers it does not understand", async () => {
  fake.protocolVersion = 3;
  await expect(execute(turn("turn-1", "hello"))).rejects.toMatchObject({
    message:
      "The T3 server cannot be used: T3 speaks orchestration protocol 3; this runtime supports 2.",
  });
  fake.protocolVersion = 2;
  const refused = build({ token: "wrong" });
  await expect(execute(turn("turn-2", "hello"), undefined, refused)).rejects.toThrow(
    /^The T3 server cannot be used: T3 refused the connection/u,
  );
  await refused.close();
  const unreachable = build({ url: "http://127.0.0.1:1" });
  await expect(execute(turn("turn-3", "hello"), undefined, unreachable)).rejects.toThrow(
    /^The T3 server cannot be used: T3 is unreachable/u,
  );
  await unreachable.close();
  fake.malformed = true;
  await expect(execute(turn("turn-4", "hello"))).rejects.toMatchObject({
    message: "T3 answered in a form this runtime does not understand.",
  });
  fake.reject = (command) =>
    command.type === "message.dispatch" ? "Thread is archived." : undefined;
  await expect(execute(turn("turn-5", "hello"))).rejects.toThrow(
    /^T3 refused the message: .*Thread is archived\.$/u,
  );
  expect(fake.threads.get(threadId)?.runs ?? []).toHaveLength(0);
});

/** A 1×1 PNG. */
const PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
  "base64",
);
const CAPTURE = "11111111-2222-3333-4444-555555555555";

/** Writes the files below the turn's attachment root and describes them as core would. */
async function captured(files: readonly { name: string; bytes: Buffer }[]) {
  await mkdir(join(resources, "attachments", CAPTURE), { recursive: true });
  return Promise.all(
    files.map(async ({ name, bytes }, index) => {
      await writeFile(join(resources, "attachments", CAPTURE, String(index)), bytes);
      return {
        path: `${CAPTURE}/${String(index)}`,
        name,
        size: bytes.length,
        sha256: createHash("sha256").update(bytes).digest("hex"),
      };
    }),
  );
}

const imageDispatches = (): unknown[] => dispatches().map((command) => command["attachments"]);

it("uploads a message's images to T3, references them, and names the files T3 does not receive", async () => {
  // Over T3's 10 MiB image limit, with a header that is still a PNG's.
  const large = Buffer.concat([PNG, Buffer.alloc(10 * 1024 * 1024)]);
  const attachments = await captured([
    { name: "photo.png", bytes: PNG },
    { name: "notes.txt", bytes: Buffer.from("plain text") },
    { name: "huge.png", bytes: large },
  ]);
  const result = execute(turn("turn-1", "What is in the photo?", { attachments }));
  answer(await started("turn-1"), "A pixel.");
  expect((await result).output).toBe("A pixel.");
  expect(imageDispatches()).toEqual([
    [
      {
        type: "image",
        id: "pending-1",
        name: "photo.png",
        mimeType: "image/png",
        sizeBytes: PNG.length,
      },
    ],
  ]);
  expect(fake.uploads.get("pending-1")?.bytes).toEqual(PNG);
  expect(fake.uploads.size).toBe(1);
  expect(userTexts()).toEqual([
    "What is in the photo?\n\n[Carillon: 2 file(s) attached to this message are not available to this runtime: notes.txt, huge.png]",
  ]);

  // An image alone is a message of its own.
  const alone = execute(
    turn("turn-2", "", { attachments: await captured([{ name: "again.png", bytes: PNG }]) }),
  );
  answer(await started("turn-2"), "The same pixel.");
  await alone;
  expect(userTexts()[1]).toBe("");
  expect(imageDispatches()[1]).toMatchObject([{ id: "pending-2", name: "again.png" }]);
});

it("uploads only the bytes core captured, failing a turn whose image changed since", async () => {
  const [photo] = await captured([{ name: "photo.png", bytes: PNG }]);
  if (photo === undefined) throw new Error("no capture");
  // Another image of the same length, its header still a PNG's, is written
  // over the capture after core checked it, while the turn waits for T3.
  const other = Buffer.from(PNG);
  other[PNG.length - 20] = (other[PNG.length - 20] ?? 0) ^ 0xff;
  fake.delayAnswer = (command) => (command.type === "thread.create" ? 500 : 0);
  const changed = execute(turn("turn-1", "look", { attachments: [photo] }));
  await until(() => fake.received.some((command) => command.type === "thread.create"));
  await writeFile(join(resources, "attachments", photo.path), other);
  await expect(changed).rejects.toMatchObject({
    message:
      "The attached file photo.png changed after Carillon received it; please send it again.",
  });
  expect(fake.uploads.size).toBe(0);
  expect(dispatches()).toHaveLength(0);

  // The bytes core captured upload as they are.
  fake.delayAnswer = () => 0;
  await writeFile(join(resources, "attachments", photo.path), PNG);
  const unchanged = execute(turn("turn-2", "look", { attachments: [photo] }));
  answer(await started("turn-2"), "A pixel.");
  await unchanged;
  expect(fake.uploads.get("pending-1")).toMatchObject({ mimeType: "image/png", bytes: PNG });
});

it("reuses its uploads when a dispatch is retried, and dispatches nothing when an upload fails", async () => {
  const attachments = await captured([{ name: "photo.png", bytes: PNG }]);
  // The first dispatch is lost before T3 processes it; the retry references the same image.
  let dropped = false;
  fake.dropRequest = (command) => {
    if (command.type !== "message.dispatch" || dropped) return false;
    dropped = true;
    return true;
  };
  const retried = execute(turn("turn-1", "look", { attachments }));
  answer(await started("turn-1"), "Seen.");
  await retried;
  expect(fake.uploads.size).toBe(1);
  expect(
    fake.received
      .filter((command) => command.type === "message.dispatch")
      .map((command) => command["attachments"]),
  ).toEqual([
    [expect.objectContaining({ id: "pending-1" })],
    [expect.objectContaining({ id: "pending-1" })],
  ]);

  // T3 refuses the bytes: the turn fails before any message is sent.
  fake.uploadStatus = 500;
  await expect(execute(turn("turn-2", "again", { attachments }))).rejects.toMatchObject({
    message: "T3 did not accept the attached image photo.png (HTTP 500).",
  });
  // An upload address on another origin is never posted to.
  fake.uploadStatus = 204;
  fake.uploadUrl = () => "http://198.51.100.7/api/attachments/upload/x.signed";
  await expect(execute(turn("turn-3", "again", { attachments }))).rejects.toMatchObject({});
  expect(dispatches()).toHaveLength(1);
  // Nothing reached T3 for those turns, so the conversation is not fenced.
  fake.uploadUrl = (id) => `/api/attachments/upload/${id}.signed`;
  const next = execute(turn("turn-4", "text only"));
  answer(await started("turn-4"), "Fine.");
  expect((await next).output).toBe("Fine.");
});

it("never follows T3's upload redirect to another origin", async () => {
  const attachments = await captured([{ name: "photo.png", bytes: PNG }]);
  // Another origin, which records whatever reaches it.
  const reached: string[] = [];
  const elsewhere = createServer((request, response) => {
    const chunks: Buffer[] = [];
    request.on("data", (chunk: Buffer) => chunks.push(chunk));
    request.on("end", () => {
      reached.push(`${String(request.method)} ${String(Buffer.concat(chunks).length)} bytes`);
      response.writeHead(500);
      response.end();
    });
  });
  elsewhere.listen(0, "127.0.0.1");
  await once(elsewhere, "listening");
  const address = elsewhere.address();
  if (address === null || typeof address === "string") throw new Error("no listening port");
  const { port } = address;
  // T3's own upload route answers with a redirect there.
  fake.uploadRedirect = `http://127.0.0.1:${String(port)}/collect`;
  const failure: unknown = await execute(turn("turn-1", "look", { attachments }))
    .then(
      () => undefined,
      (error: unknown) => error,
    )
    .finally(() => elsewhere.close());
  expect(reached).toEqual([]);
  expect(failure).toMatchObject({
    message: "T3 did not accept the attached image photo.png (HTTP 307).",
  });
  expect(dispatches()).toHaveLength(0);
});

it("runs plain T3 Code on the instances T3 offers, with the person's text as written and T3's own title", async () => {
  const plainThread = derivedId("thread", "discord-private", "code", "conversation-1");
  const code = { ...conversation, agent_id: "code" };
  expect((await runtime.modelState(code)).available.map((model) => model.provider)).toEqual([
    "claudeAgent",
    "claudeAgent",
    "codex",
  ]);
  const native = { origin: { guild_id: "100", channel_id: "300", user_id: "200" } };
  const reply = execute({
    ...turn("turn-1", "Fix the  flaky\ntest", { native }),
    agent_id: "code",
  });
  await fake.waitFor(() => fake.threads.get(plainThread)?.runs.length === 1);
  const plain = fake.threads.get(plainThread);
  const run = plain?.runs[0];
  if (plain === undefined || run === undefined) throw new Error("the plain thread did not run");
  expect(run.modelSelection).toMatchObject({ instanceId: "codex", model: "gpt-6-astra" });
  expect(plain.messages.filter((message) => message.role === "user").map((m) => m.text)).toEqual([
    "Fix the  flaky\ntest",
  ]);
  expect(fake.commands.find((command) => command.type === "thread.create")).toMatchObject({
    threadId: plainThread,
    title: "New thread",
  });
  expect(dispatches()[0]).toMatchObject({ titleSeed: "Fix the flaky test" });
  fake.assistant(plainThread, run.id, "Fixed.");
  fake.complete(plainThread, run.id);
  expect((await reply).output).toBe("Fixed.");
  // A conversation of an agent the runtime does not serve is refused before T3.
  await expect(execute({ ...turn("turn-2", "hi"), agent_id: "elsewhere" })).rejects.toThrow(
    "This conversation's agent is not set up on T3.",
  );
});

it("leaves the native run when the runtime closes mid-turn, and takes the turn up again at its next start", async () => {
  const result = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  // Closed, it settles nothing and stops nothing: core keeps the turn running.
  await runtime.close();
  expect(fake.interrupts).toEqual([]);
  expect(runOf("turn-1")?.status).toBe("running");
  expect(runtime.restartReadiness()).toEqual({ state: "ready" });
  // Started again, it reads the turn back from core and its run back from T3.
  await restart();
  await until(() => runtime.activity("turn-1", "discord-private")?.phase === "model");
  answer(run, "Done.");
  expect((await result).output).toBe("Done.");
  expect(dispatches()).toHaveLength(1);
  expect(core.received.filter(({ method }) => method === "GET")).toHaveLength(2);
});

it("takes up a turn core holds whose message never reached T3, dispatching it once", async () => {
  core.hold(turn("turn-1", "hello"));
  // Another process accepted it and stopped before T3 had it; this one starts.
  await restart();
  const run = await started("turn-1");
  answer(run, "hi");
  expect((await settled("turn-1")).output).toBe("hi");
  // Core handing it over again finds it held, or settled: nothing runs twice.
  await runtime.turns.submit(turn("turn-1", "hello"));
  expect(dispatches()).toHaveLength(1);
});

/** A refusal whose nested detail carries credentials and a signed address. */
const LEAKY =
  "Authorization: Bearer sk-live-0123456789abcdefghijklmnop; retry at https://t3.example/run?token=abcdef0123456789abcdef&sig=feedfacecafebeef password=hunter2-hunter2";
const LEAKED = [
  "sk-live-0123456789abcdefghijklmnop",
  "abcdef0123456789abcdef",
  "feedfacecafebeef",
  "hunter2-hunter2",
];

function refusalLogged(fragment: string): string {
  const line = vi
    .mocked(console.log)
    .mock.calls.map((call) => String(call[0]))
    .find((logged) => logged.includes(fragment));
  if (line === undefined) throw new Error(`nothing logged ${fragment}`);
  return line;
}

it("logs T3's refusal of an interrupt without the credentials its detail carries", async () => {
  fake.reject = (command) => (command.type === "run.interrupt" ? LEAKY : undefined);
  const controller = new AbortController();
  const result = execute(turn("turn-1", "work"), controller.signal);
  await started("turn-1");
  controller.abort(new Error("stopped"));
  await expect(result).rejects.toThrow("stopped");
  const line = refusalLogged("interrupt not accepted");
  for (const leaked of LEAKED) expect(line).not.toContain(leaked);
  expect(line).toContain("[redacted]");
});

it("logs T3's refusal to withdraw a queued run without the credentials its detail carries", async () => {
  const first = execute(turn("turn-1", "hello"));
  answer(await started("turn-1"), "hi");
  await first;
  const person = fake.wake(threadId, "a message typed in the T3 UI");
  fake.request(threadId, person.id, "tool_approval");
  fake.reject = (command) => (command.type === "queued-run.cancel" ? LEAKY : undefined);
  const controller = new AbortController();
  const queued = execute(turn("turn-2", "from Discord"), controller.signal);
  expect((await started("turn-2")).status).toBe("queued");
  await until(() => runtime.activity("turn-2", "discord-private")?.phase === "queued");
  controller.abort(new Error("core disconnected"));
  await expect(queued).rejects.toThrow(UNCONFIRMED);
  const line = refusalLogged("queued-run.cancel not accepted");
  for (const leaked of LEAKED) expect(line).not.toContain(leaked);
  expect(line).toContain("[redacted]");
});

it("requests a session detach when cancelled after the roster cleared, before the wake, and reports the stop as unconfirmed", async () => {
  const controller = new AbortController();
  const result = execute(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command", description: "sleep 25" }]);
  fake.assistant(threadId, run.id, "Started it in the background.");
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools");
  fake.background(threadId, []);
  await until(() => activity()?.phase === "model");
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow(UNCONFIRMED);
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
  const next = execute(turn("turn-2", "again"));
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
  const result = execute(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools");
  fake.background(threadId, []);
  await until(() => activity()?.phase === "model");
  controller.abort(new Error("cancelled"));
  await expect(result).rejects.toThrow(UNCONFIRMED);
  expect(fake.detached).toEqual([thread().session.id]);
  const logged = vi.mocked(console.log).mock.calls.map((call) => String(call[0]));
  expect(logged.some((line) => /does not confirm|did not confirm/u.test(line))).toBe(true);
  expect(logged.some((line) => line.includes("T3 confirmed the stop"))).toBe(false);
});

it("fails closed when a snapshot resume cannot show whether background work was reported, without claiming a stop", async () => {
  fake.snapshotOnResume = true;
  const result = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command", description: "sleep 25" }]);
  await until(() => activity()?.phase === "tools");
  fake.disconnectAll();
  // Missed while disconnected: the run completes with the roster listed, then the roster empties.
  fake.assistant(threadId, run.id, "Started it in the background.");
  fake.complete(threadId, run.id);
  fake.background(threadId, []);
  // The explanation shown is bounded, so it ends before its last words.
  await expect(result).rejects.toThrow(
    "Carillon lost its connection to T3 while this turn ran background work, and T3's state does not show whether the result was delivered; T3 does not confirm that its native process has stopped, so a result may still arrive",
  );
  expect(fake.interrupts).toEqual([]);
  expect(fake.detached).toEqual([thread().session.id]);
  expect(fake.subscriptions.at(-1)?.afterSequence).toBeDefined();
});

it("resumes by replay through the roster transitions and still waits for the wake", async () => {
  const result = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  await until(() => activity()?.phase === "tools");
  fake.disconnectAll();
  fake.assistant(threadId, run.id, "Started it.");
  fake.complete(threadId, run.id);
  fake.background(threadId, []);
  await resubscribed();
  await until(() => activity()?.phase === "model");
  expect(activity()).toMatchObject({ phase: "model", running: [] });
  const wake = fake.wake(threadId, "Background command completed");
  fake.assistant(threadId, wake.id, "It printed done.");
  fake.complete(threadId, wake.id);
  expect((await result).output).toBe("Started it.\n\nIt printed done.");
  expect(fake.detached).toEqual([]);
});

it("completes normally when background work finished inside the running turn", async () => {
  const result = execute(turn("turn-1", "work"));
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
  const result = execute(turn("turn-1", "work"));
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
    message:
      "T3 asked a question this runtime cannot relay yet (Scope: Which file?); T3 has not confirmed that the run stopped, so it may still be waiting. Send the answer in a new message.",
  });
});

it("never answers with, waits for or stops a run a person started in T3 while its own wake is owed", async () => {
  const result = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  fake.assistant(threadId, run.id, "Started own background work.");
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools");
  fake.background(threadId, []);
  // A person writes in T3's own UI while the turn waits for its wake.
  const person = fake.message(threadId, "Something else?");
  fake.assistant(threadId, person.id, "UNRELATED UI ANSWER");
  fake.complete(threadId, person.id);
  const settled = await Promise.race([result.then(() => true), sleep(200).then(() => false)]);
  expect(settled).toBe(false);
  // The wake T3 starts for the provider is the turn's.
  const wake = fake.wake(threadId, "Background command completed");
  fake.assistant(threadId, wake.id, "OWN RESULT.");
  fake.complete(threadId, wake.id);
  expect((await result).output).toBe("Started own background work.\n\nOWN RESULT.");
  expect(fake.interrupts).toEqual([]);
});

it("neither interrupts nor detaches the session of a person's run when cancelled while its wake is owed", async () => {
  const controller = new AbortController();
  const result = execute(turn("turn-1", "work"), controller.signal);
  void result.catch(() => undefined);
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools");
  fake.background(threadId, []);
  const person = fake.message(threadId, "Something else?");
  // The fold hears of the person's run before Carillon cancels.
  await sleep(200);
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow(UNCONFIRMED);
  expect(fake.interrupts).toEqual([]);
  expect(fake.detached).toEqual([]);
  expect(thread().runs.find((candidate) => candidate.id === person.id)?.status).toBe("running");
});

it("detaches no session while a person's completed run still lists its own background work", async () => {
  const controller = new AbortController();
  const result = execute(turn("turn-1", "work"), controller.signal);
  void result.catch(() => undefined);
  const run = await started("turn-1");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools");
  fake.background(threadId, []);
  // While the turn's wake is owed, a person's run in T3's own UI starts
  // background work of its own and completes, leaving that work listed.
  const person = fake.message(threadId, "Run this in the background.");
  fake.background(threadId, [{ taskId: "ui-task", kind: "command" }]);
  fake.complete(threadId, person.id);
  // The fold hears of it before Carillon cancels.
  await sleep(200);
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow(UNCONFIRMED);
  // T3 refuses to interrupt the turn's completed run, and the session the
  // person's work runs in is not detached under it.
  expect(fake.interrupts).toEqual([]);
  expect(fake.detached).toEqual([]);
  expect(thread().sessionDetached).toBe(false);
  expect(roster()).toEqual([{ taskId: "ui-task", kind: "command" }]);
});

/**
 * The turn's run leaves its own work listed while an older provider's
 * process, on its own provider thread, finishes a task and wakes.
 */
async function olderProviderWake(signal?: AbortSignal) {
  const result = execute(turn("turn-1", "work"), signal);
  void result.catch(() => undefined);
  const run = await started("turn-1");
  const older = (tasks: { taskId: string; kind: string }[]) => ({
    id: "old-provider",
    providerSessionId: "old-session",
    pendingBackgroundTasks: tasks,
  });
  fake.otherProvider(threadId, older([{ taskId: "old-task", kind: "command" }]));
  fake.background(threadId, [{ taskId: "own-task", kind: "command" }]);
  fake.assistant(threadId, run.id, "Started own work.");
  fake.complete(threadId, run.id);
  await until(() => activity()?.phase === "tools");
  fake.otherProvider(threadId, older([]));
  const foreign = fake.wake(threadId, "Old command completed", "old-provider");
  fake.assistant(threadId, foreign.id, "FOREIGN OLDER RESULT");
  return { result, foreign };
}

it("never answers with or settles on another provider's wake while its own work is listed", async () => {
  const { result, foreign } = await olderProviderWake();
  fake.complete(threadId, foreign.id);
  const settled = await Promise.race([result.then(() => true), sleep(200).then(() => false)]);
  expect(settled).toBe(false);
  expect(activity()?.phase).toBe("tools");
  // Its own work finishes and its own process wakes.
  fake.background(threadId, []);
  const own = fake.wake(threadId, "Own command completed");
  fake.assistant(threadId, own.id, "OWN RESULT.");
  fake.complete(threadId, own.id);
  expect((await result).output).toBe("Started own work.\n\nOWN RESULT.");
  expect(fake.interrupts).toEqual([]);
});

it("neither interrupts another provider's wake nor detaches a session under it when cancelled", async () => {
  const controller = new AbortController();
  const { result, foreign } = await olderProviderWake(controller.signal);
  // The fold hears of the wake before Carillon cancels.
  await sleep(200);
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow(UNCONFIRMED);
  expect(fake.interrupts).toEqual([]);
  expect(fake.detached).toEqual([]);
  expect(thread().runs.find((candidate) => candidate.id === foreign.id)?.status).toBe("running");
});

it("does not adopt a later run the turn does not owe, and the next turn waits behind it", async () => {
  const result = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  // A predecessor's late wake lands on the thread while this turn runs: it queues behind it.
  const stale = fake.wake(threadId, 'Background command "old sleep" completed (exit code 0)');
  answer(run, "Only mine.");
  expect((await result).output).toBe("Only mine.");
  // The stale run now runs; it is not this turn's and is not awaited.
  expect(runOf("turn-1")?.status).toBe("completed");
  expect(thread().runs.find((candidate) => candidate.id === stale.id)?.status).toBe("running");
  fake.assistant(threadId, stale.id, "Old result.");
  expect((await result).output).toBe("Only mine.");

  const next = execute(turn("turn-2", "again"));
  expect((await started("turn-2")).status).toBe("queued");
  fake.complete(threadId, stale.id);
  answer(await waitForStatus("turn-2", "running"), "ok");
  expect((await next).output).toBe("ok");
  expect(fake.interrupts).toEqual([]);
});

it("ignores a stale pending request until the new run's own item names its request", async () => {
  fake.retainRequests = true;
  const first = execute(turn("turn-1", "first"));
  const run = await started("turn-1");
  answer(run, "done");
  expect((await first).output).toBe("done");
  // An earlier run's request is left pending with no item of the new turn naming it.
  fake.request(threadId, run.id, "tool_approval");
  fake.retainRequests = false;

  const second = execute(turn("turn-2", "second"));
  const next = await started("turn-2");
  // The new run waits on its own request before its item has arrived: unknown, not blocked.
  const own = fake.request(threadId, next.id, "user_input");
  await until(() => runtime.activity("turn-2", "discord-private")?.phase === "waiting");
  expect(fake.interrupts).toEqual([]);
  expect(runtime.activity("turn-2", "discord-private")?.phase).toBe("waiting");
  // Its item arrives and names the request: blocked with the new turn's wording.
  fake.tool(threadId, next.id, {
    type: "user_input_request",
    requestId: own.id,
    questions: [{ header: "Scope", question: "Which file?" }],
  });
  await expect(second).rejects.toMatchObject({
    message:
      "T3 asked a question this runtime cannot relay yet (Scope: Which file?); the run was stopped. Send the answer in a new message.",
  });
  expect(fake.interrupts).toEqual([next.id]);
});

it.each([false, true])(
  "cancels promptly while the first snapshot never arrives, existing thread %s",
  async (existing) => {
    if (existing) {
      const initial = execute(turn("initial", "Hello."));
      answer(await started2("initial"), "Hi.");
      await initial;
    }
    const dispatched = dispatches().length;
    const subscribed = fake.subscriptions.length;
    fake.holdSnapshot = true;
    const controller = new AbortController();
    const result = execute(turn("turn-1", "work"), controller.signal);
    await until(() => fake.subscriptions.length === subscribed + 1);
    const started = Date.now();
    controller.abort(new Error("core disconnected"));
    await expect(result).rejects.toThrow("core disconnected");
    expect(Date.now() - started).toBeLessThan(1_000);
    expect(dispatches()).toHaveLength(dispatched);
    fake.holdSnapshot = false;
    const next = execute(turn("turn-2", "again"));
    answer(await started2("turn-2"), "ok");
    expect((await next).output).toBe("ok");
  },
);

it("cancels promptly during a reconnect delay before anything was dispatched", async () => {
  const slow = build({ reconnectDelayMs: 5_000 });
  let dropped = 0;
  fake.dropAnswer = (command) => command.type === "thread.create" && dropped++ === 0;
  const controller = new AbortController();
  const result = execute(turn("turn-1", "work"), controller.signal, slow);
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
  const result = execute(turn("turn-1", "work"), controller.signal, slow);
  const run = await started2("turn-1");
  await pause(100);
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow("core disconnected");
  // The abort did not abandon the native run: the reconnection found and interrupted it.
  expect(fake.interrupts).toEqual([run.id]);
  expect(runOf("turn-1")?.status).toBe("interrupted");
  await slow.close();
}, 10_000);

it.each([false, true])(
  "closes promptly while the first snapshot never arrives, existing thread %s",
  async (existing) => {
    if (existing) {
      const initial = execute(turn("initial", "Hello."));
      answer(await started2("initial"), "Hi.");
      await initial;
    }
    const dispatched = dispatches().length;
    const subscribed = fake.subscriptions.length;
    fake.holdSnapshot = true;
    void execute(turn("turn-1", "work"));
    await until(() => fake.subscriptions.length === subscribed + 1);
    const started = Date.now();
    await runtime.close();
    expect(Date.now() - started).toBeLessThan(1_000);
    expect(dispatches()).toHaveLength(dispatched);
    // Core keeps the turn running, for the next process to take up.
    expect(settles("turn-1")).toEqual([]);
  },
);

const started2 = started;

it("fails closed on any follow-up after an unconfirmed stop on the conversation, while plain turns continue", async () => {
  expect(runtime.restartReadiness()).toEqual({ state: "ready" });
  // Turn 1 is cancelled while waiting and T3 never confirms the stop.
  fake.interruptBehaviour = "ignore";
  const controller = new AbortController();
  const first = execute(turn("turn-1", "first"), controller.signal);
  const run1 = await started("turn-1");
  // A running turn is core's to list, so a restart loses nothing of it.
  expect(runtime.restartReadiness()).toEqual({ state: "ready" });
  controller.abort(new Error("core disconnected"));
  await expect(first).rejects.toThrow(UNCONFIRMED);
  // Only this process remembers the fence, so a restart now would lose it.
  const fenced = {
    state: "unready",
    reason:
      "1 conversation thread(s) have T3 work whose stop T3 never confirmed, which only this process remembers",
  };
  expect(runtime.restartReadiness()).toEqual(fenced);
  fake.interruptBehaviour = "settle";
  fake.stop(threadId, run1.id, "interrupted");

  // Turn 2 owes a wake; the follow-up cannot be told from turn 1's late wake.
  const second = execute(turn("turn-2", "second"));
  const run2 = await started("turn-2");
  fake.background(threadId, [{ taskId: "task-1", kind: "command" }]);
  fake.assistant(threadId, run2.id, "Started.");
  fake.complete(threadId, run2.id);
  await until(() => runtime.activity("turn-2", "discord-private")?.phase === "tools");
  fake.background(threadId, []);
  const wake = fake.wake(threadId, "Background command completed");
  fake.assistant(threadId, wake.id, "Whose result?");
  await expect(second).rejects.toThrow(/was not confirmed stopped/u);
  await expect(second).rejects.toBeInstanceOf(TurnFailed);
  expect(fake.interrupts).toEqual([run1.id, wake.id]);

  // The fence is not lifted by inference: a plain turn still completes on its own text,
  // a turn that depends on a follow-up still ends unresolved.
  const third = execute(turn("turn-3", "third"));
  answer(await started("turn-3"), "Plain answer.");
  expect((await third).output).toBe("Plain answer.");
  const fourth = execute(turn("turn-4", "fourth"));
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
  // Turns that completed since do not lift it either.
  expect(runtime.restartReadiness()).toEqual(fenced);
});

/**
 * Drives a turn that owes a wake, then its wake, and hands back the still
 * pending result: it completes, or ends unresolved when the thread is fenced.
 */
async function backgroundTurn(
  correlation: string,
  instance = runtime,
): Promise<{ readonly result: Promise<{ output: string }> }> {
  const result = execute(turn(correlation, "work"), undefined, instance);
  // The assertion attaches after the fake has been driven; keep an early rejection handled meanwhile.
  void result.catch(() => undefined);
  const run = await started(correlation);
  // A run waiting behind another starts before it can start background work.
  if (run.status === "queued") fake.stop(threadId, run.id, "running");
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
  const result = execute(turn("turn-1", "work"), controller.signal);
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
  const result = execute(turn("turn-1", "work"));
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
  const detachedResult = execute(turn("turn-1", "work"));
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
  await restart();
  const controller = new AbortController();
  const cancelled = execute(turn("turn-3", "work"), controller.signal);
  const run3 = await started("turn-3");
  controller.abort(new Error("core disconnected"));
  await expect(cancelled).rejects.toThrow("core disconnected");
  expect(runOf("turn-3")?.status).toBe("interrupted");
  expect((await (await backgroundTurn("turn-4")).result).output).toBe("Started.\n\nResult.");
  expect(fake.interrupts).toEqual([run3.id]);
});

it("fences the conversation when a turn read back from an existing run exits without a confirmed stop", async () => {
  // Another runtime instance retries the same correlation while its run is active: it dispatches nothing.
  const first = execute(turn("turn-1", "work"));
  await started("turn-1");
  const retry = build();
  const again = execute(turn("turn-1", "work"), undefined, retry);
  // Both turns will fail; observe each rejection as it happens.
  const firstFails = first.catch((failure: unknown) => failure);
  const againFails = again.catch((failure: unknown) => failure);
  await until(() => retry.activity("turn-1", "discord-private")?.phase === "model");
  expect(dispatches()).toHaveLength(1);
  fake.protocolVersion = 3;
  fake.disconnectAll();
  for (const failure of [await againFails, await firstFails])
    expect(failure).toHaveProperty(
      "message",
      expect.stringMatching(/^Lost contact with T3 while the turn ran .*may still be running\.$/u),
    );
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
  const result = execute(turn("turn-1", "work"), controller.signal, slow);
  await until(() => fake.received.some((command) => command.type === "message.dispatch"));
  await pause(100);
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow(UNCONFIRMED);
  fake.dropRequest = () => false;
  await expect((await backgroundTurn("turn-2", slow)).result).rejects.toThrow(
    /was not confirmed stopped/u,
  );
  await slow.close();
}, 10_000);

it("cancels or closes promptly while T3's discovery or upgrade stalls before any dispatch", async () => {
  fake.holdDiscovery = true;
  const controller = new AbortController();
  const result = execute(turn("turn-1", "work"), controller.signal);
  await until(() => fake.heldRequests.size === 1);
  let begun = Date.now();
  controller.abort(new Error("core disconnected"));
  await expect(result).rejects.toThrow("core disconnected");
  expect(Date.now() - begun).toBeLessThan(500);
  fake.holdDiscovery = false;
  fake.disconnectAll();

  fake.holdUpgrade = true;
  void execute(turn("turn-2", "work"));
  await until(() => fake.heldRequests.size === 1);
  begun = Date.now();
  await runtime.close();
  expect(Date.now() - begun).toBeLessThan(500);
  expect(settles("turn-2")).toEqual([]);
  fake.holdUpgrade = false;
  fake.disconnectAll();
  expect(fake.threads.size).toBe(0);
  expect(dispatches()).toHaveLength(0);
});

it("keeps reconnecting through a stalled discovery after the dispatch, and stops the run when it gets through", async () => {
  let dropped = 0;
  fake.dropAnswer = (command) => command.type === "message.dispatch" && dropped++ === 0;
  const controller = new AbortController();
  const result = execute(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  // The reconnection's discovery stalls; the caller cancels meanwhile.
  fake.holdDiscovery = true;
  await until(() => fake.heldRequests.size === 1);
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
  const result = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  const task = fake.tool(threadId, run.id, { type: "command_execution", input: "sleep 300 &" });
  fake.background(threadId, [{ taskId: "task-1", kind: "command", description: "sleep 300" }]);
  await until(() => activity()?.phase === "tools");
  fake.fail(threadId, run.id, "Provider crashed");
  await expect(result).rejects.toMatchObject({
    message: "Provider crashed; T3 confirmed that its background work was stopped.",
  });
  expect(fake.interrupts).toEqual([run.id]);
  expect(roster()).toEqual([]);
  expect(thread().items.find((item) => item.id === task.id)?.status).toBe("interrupted");
  // The stop was confirmed: the conversation stays trusted.
  expect((await (await backgroundTurn("turn-2")).result).output).toBe("Started.\n\nResult.");
  expect(fake.interrupts).toEqual([run.id]);

  // A run someone else stopped with work still listed goes the same way.
  const stopped = execute(turn("turn-3", "work"));
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
  const result = execute(turn("turn-1", "work"));
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
  const incompatible = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.protocolVersion = 3;
  fake.disconnectAll();
  await expect(incompatible).rejects.toThrow(
    `Lost contact with T3 while the turn ran (T3 speaks orchestration protocol 3; this runtime supports 2); native run ${run.id} may still be running.`,
  );
  await expect(incompatible).rejects.toBeInstanceOf(TurnFailed);
  fake.protocolVersion = 2;
  // A fresh turn that never reached T3 still reports plain unavailability.
  fake.protocolVersion = 3;
  await expect(execute(turn("turn-2", "work"))).rejects.toThrow(
    /^The T3 server cannot be used: T3 speaks orchestration protocol 3/u,
  );
  fake.protocolVersion = 2;

  // The server goes away entirely after a run was accepted.
  const unreachable = execute(turn("turn-3", "work"));
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
  await expect(execute(turn("turn-1", "hello"))).rejects.toThrow(
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
  await expect(execute(turn("turn-3", "hello"))).rejects.toThrow(
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
  const result = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  answer(run, "Committed after all.");
  expect((await result).output).toBe("Committed after all.");
  expect(fake.received.filter((command) => command.type === "message.dispatch")).toHaveLength(2);
  expect(thread().runs).toHaveLength(1);
  expect((await (await backgroundTurn("turn-2")).result).output).toBe("Started.\n\nResult.");
});

it("refuses a resumed snapshot that omits the run it observed, keeping the run possibly running", async () => {
  const result = execute(turn("turn-1", "work"));
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
  const result = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  fake.assistant(threadId, run.id, "Part one.");
  await until(() => activity()?.headline === "Part one.");
  fake.snapshotOnResume = true;
  fake.messageWindow = 1;
  fake.disconnectAll();
  fake.assistant(threadId, run.id, "Part two.", false, "2");
  fake.complete(threadId, run.id);
  expect((await result).output).toBe("Part one.\n\nPart two.");
  expect(fake.subscriptions.at(-1)?.afterSequence).toBeDefined();
});

it("queues behind a person's run in the native UI and cancels only its own queued run", async () => {
  const first = execute(turn("turn-1", "hello"));
  answer(await started("turn-1"), "hi");
  await first;
  // The person starts a run in T3's own UI; it waits on their approval.
  const person = fake.wake(threadId, "a message typed in the T3 UI");
  fake.request(threadId, person.id, "tool_approval");

  const controller = new AbortController();
  const queued = execute(turn("turn-2", "from Discord"), controller.signal);
  const own = await started("turn-2");
  expect(own.status).toBe("queued");
  expect(dispatches().at(-1)).toMatchObject({ dispatchMode: { type: "queue_after_active" } });
  await until(() => runtime.activity("turn-2", "discord-private")?.phase === "queued");
  controller.abort(new Error("core disconnected"));
  await expect(queued).rejects.toThrow("core disconnected");
  // Only the turn's own queued run was withdrawn from T3's queue (a queued run
  // cannot be interrupted); the person's run still waits on them.
  expect(fake.interrupts).toEqual([]);
  expect(fake.commands.filter((command) => command.type === "queued-run.cancel")).toMatchObject([
    { runId: own.id },
  ]);
  expect(runOf("turn-2")?.status).toBe("cancelled");
  expect(thread().runs.find((run) => run.id === person.id)?.status).toBe("waiting");
  const logged = vi.mocked(console.log).mock.calls.map((call) => String(call[0]));
  expect(logged.some((line) => line.includes("T3 confirmed the stop"))).toBe(true);
});

/** The opening of the turn that carries the report of work `correlation`'s message delegated. */
const reportOpening = (correlation: string, after: number) => ({
  method: "POST",
  path: "/v1/runtime/turns",
  body: {
    interface_id: "discord-private",
    agent_id: "default",
    audience_id: "conversation-1",
    key: `t3:${derivedId("message", correlation)}:${String(after)}`,
    label: "The report of work delegated in T3.",
  },
});

it("answers when the parent run completes and delivers the delegated report in a turn it opens", async () => {
  const first = execute(turn("turn-1", "delegate it"));
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
  expect(answered(reply)).toEqual({ output: "DELEGATED", activity: undefined });
  // The answer's report still shows the child working, as T3 last confirmed
  // it, and the report it owes this conversation.
  expect(reply.activity?.work?.items).toMatchObject([
    { id: child.id, kind: "delegation", label: "pong child", state: "running" },
    { kind: "task", label: "report back to this conversation", state: "queued" },
  ]);
  // The report's turn was opened before the answer settled, keyed by T3's records.
  expect(openings()).toMatchObject([reportOpening("turn-1", 1)]);
  // It dispatches nothing and waits.
  await until(() => runtime.activity("opened-1", "discord-private")?.work?.items.length === 2);
  expect(runtime.activity("opened-1", "discord-private")?.phase).toBe("waiting");
  expect(runtime.activity("opened-1", "discord-private")?.work?.items).toEqual([
    expect.objectContaining({ id: child.id, kind: "delegation", state: "running" }),
    expect.objectContaining({
      kind: "task",
      label: "report back to this conversation",
      state: "queued",
    }),
  ]);
  const continuation = fake.finishDelegation(threadId, child.id);
  fake.assistant(threadId, continuation.id, "PONG reported.");
  fake.complete(threadId, continuation.id);
  expect(answered(await settled("opened-1"))).toEqual({
    output: "PONG reported.",
    activity: undefined,
  });
  expect(dispatches()).toHaveLength(1);
  expect(fake.interrupts).toEqual([]);
});

it("runs the conversation's next message while a report's turn still observes delegated work", async () => {
  const first = execute(turn("turn-1", "delegate it"));
  const parent = await started("turn-1");
  const child = fake.delegate(threadId, parent.id, { title: "slow child" });
  answer(parent, "DELEGATED");
  await first;
  await until(() => runtime.activity("opened-1", "discord-private")?.work?.items.length === 2);
  // The next message reaches T3 and is answered while the child still works.
  const next = execute(turn("turn-2", "anything else?"));
  answer(await started("turn-2"), "Yes.");
  expect((await next).output).toBe("Yes.");
  expect(thread().subagents.find((task) => task.id === child.id)?.status).toBe("running");
  // The child finishes: only the report's turn reports it, once.
  const continuation = fake.finishDelegation(threadId, child.id);
  fake.assistant(threadId, continuation.id, "Child done.");
  fake.complete(threadId, continuation.id);
  const report = await settled("opened-1");
  expect(report.output).toBe("Child done.");
  // Its report shows how the child ended, with T3's own timing.
  expect(report.activity?.work?.items[0]).toMatchObject({ id: child.id, state: "succeeded" });
  expect(typeof report.activity?.work?.items[0]?.duration_ms).toBe("number");
  expect(dispatches()).toHaveLength(2);
  expect(fake.interrupts).toEqual([]);
});

it("says where a child's result went when T3 delivers it into the next message's run", async () => {
  const first = execute(turn("turn-1", "delegate it"));
  const parent = await started("turn-1");
  const child = fake.delegate(threadId, parent.id, { title: "child" });
  answer(parent, "DELEGATED");
  await first;
  await until(() => runtime.activity("opened-1", "discord-private")?.work?.items.length === 2);
  const next = execute(turn("turn-2", "anything else?"));
  const own = await started("turn-2");
  // The child finishes while that message runs: T3 delivers the result into it.
  expect(fake.finishDelegation(threadId, child.id).id).toBe(own.id);
  answer(own, "Nothing else; the child said PONG.");
  expect((await next).output).toBe("Nothing else; the child said PONG.");
  expect((await settled("opened-1")).output).toBe(
    "T3 gave the result of the delegated work to the agent during a later message's run in this conversation, not in a report of its own; T3 shows it.",
  );
});

/** Closes the runtime once turn-1's work was delegated, leaving its report's turn running in core. */
async function delegatedThenClosed(): Promise<{ readonly child: string }> {
  const first = execute(turn("turn-1", "delegate it"));
  const parent = await started("turn-1");
  const child = fake.delegate(threadId, parent.id, { title: "child" });
  answer(parent, "DELEGATED");
  await first;
  await runtime.close();
  return { child: child.id };
}

it("takes a report's turn up again after a restart, reporting what T3 delivered meanwhile once", async () => {
  const first = execute(turn("turn-1", "delegate twice"));
  const parent = await started("turn-1");
  const quick = fake.delegate(threadId, parent.id, { title: "quick child" });
  const slow = fake.delegate(threadId, parent.id, { title: "slow child" });
  answer(parent, "DELEGATED");
  await first;
  await until(() => runtime.activity("opened-1", "discord-private")?.work !== undefined);
  // The runtime stops; nothing observes while T3 works.
  await runtime.close();
  // Meanwhile one child reports in a continuation of its own (whose message
  // T3 does not project), and the other's result is delivered into the run
  // of a message a person sends in T3's own UI.
  const reported = fake.finishDelegation(threadId, quick.id);
  fake.assistant(threadId, reported.id, "Quick result.");
  fake.complete(threadId, reported.id);
  const person = fake.message(threadId, "more");
  expect(fake.finishDelegation(threadId, slow.id).id).toBe(person.id);
  fake.assistant(threadId, person.id, "More, and the slow child is done.");
  fake.complete(threadId, person.id);
  // Read back from core, the report's turn is rebuilt from T3's records
  // alone. It reports the continuation once, and not the person's reply.
  await restart();
  expect((await settled("opened-1")).output).toBe(
    "Quick result.\n\n[T3 gave the result of the delegated work to the agent during a later message's run in this conversation, not in a report of its own; T3 shows it.]",
  );
  // No dispatch, interrupt or detach on T3's work.
  expect(dispatches()).toHaveLength(1);
  expect(fake.interrupts).toEqual([]);
  expect(fake.detached).toEqual([]);
  expect(openings()).toHaveLength(1);
});

it("says so when a report taken up again is older than the window it reads", async () => {
  const { child } = await delegatedThenClosed();
  const continuation = fake.finishDelegation(threadId, child);
  fake.assistant(threadId, continuation.id, "Old report.");
  fake.complete(threadId, continuation.id);
  // Later messages, a person's in T3's own UI, push it out of the window.
  for (const text of ["later", "later still"]) {
    const later = fake.message(threadId, text);
    fake.assistant(threadId, later.id, "ok");
    fake.complete(threadId, later.id);
  }
  fake.messageWindow = 2;
  await restart();
  await expect(settled("opened-1")).rejects.toThrow(
    "T3 finished the report of the delegated work, but it is older than the part of the thread this runtime reads; read it in T3",
  );
});

it("reads a snapshot over T3's byte budget as whole, so a report there is neither older nor partial", async () => {
  const { child } = await delegatedThenClosed();
  const continuation = fake.finishDelegation(threadId, child);
  fake.assistant(threadId, continuation.id, "Whole report.");
  fake.complete(threadId, continuation.id);
  // T3 keeps complete turns over its byte budget and says so; it omits nothing.
  fake.payloadBudgetExceeded = true;
  await restart();
  expect((await settled("opened-1")).output).toBe("Whole report.");
});

it("does not call a missing report older than the window when only T3's byte budget was exceeded", async () => {
  const { child } = await delegatedThenClosed();
  // T3 delivers the result to a continuation that ends without a report:
  // the snapshot holds all of it, so the report is missing, not older.
  fake.complete(threadId, fake.finishDelegation(threadId, child).id);
  fake.payloadBudgetExceeded = true;
  await restart();
  await expect(settled("opened-1")).rejects.toThrow("T3 finished the run without an answer.");
});

it("sends a command with the images it could upload, and prose names a file T3 cannot take", async () => {
  const [notes, photo] = await captured([
    { name: "notes.txt", bytes: Buffer.from("plain text") },
    { name: "photo.png", bytes: PNG },
  ]);
  if (notes === undefined || photo === undefined) throw new Error("no capture");
  // A command with a file T3 cannot receive runs as written, with the image
  // it could upload, and its answer names the file.
  const mixed = execute(
    turn("turn-1", "/compact", { native: { intent: "command" }, attachments: [notes, photo] }),
  );
  answer(await started("turn-1"), "Compacted.");
  expect((await mixed).output).toBe(
    "Compacted.\n\n[Carillon: 1 file(s) attached to this message are not available to this runtime: notes.txt]",
  );
  expect(userTexts()[0]).toBe("/compact");
  expect(imageDispatches()[0]).toMatchObject([{ name: "photo.png" }]);
  const prose = execute(turn("turn-2", "read it", { attachments: [notes] }));
  answer(await started("turn-2"), "Read.");
  expect((await prose).output).toBe("Read.");
  expect(userTexts()[1]).toBe(
    "read it\n\n[Carillon: 1 file(s) attached to this message are not available to this runtime: notes.txt]",
  );
});

it("sends a command invocation verbatim", async () => {
  const command = execute(
    turn("turn-1", "/compact keep the decisions", { native: { intent: "command" } }),
  );
  answer(await started("turn-1"), "Compacted.");
  expect((await command).output).toBe("Compacted.");
  expect(userTexts()).toEqual(["/compact keep the decisions"]);

  // A command T3 runs to completion without a reply of its own is acknowledged
  // by name; a failed one keeps T3's explanation.
  const done = execute(turn("turn-3", "/compact", { native: { intent: "command" } }));
  fake.complete(threadId, (await started("turn-3")).id);
  expect((await done).output).toBe("`/compact` finished.");
  const failed = execute(turn("turn-4", "/review main", { native: { intent: "command" } }));
  fake.fail(threadId, (await started("turn-4")).id, "Unknown skill: review");
  await expect(failed).rejects.toThrow("Unknown skill: review");
});

it("lists the commands and user-invocable skills T3 offers the conversation's provider in the workspace", async () => {
  const listing = (instanceId: string, names: string[]) => ({
    instanceId,
    driver: instanceId,
    enabled: true,
    installed: true,
    status: "ready",
    models: [],
    slashCommands: [{ name: "machine-only" }],
    skills: [],
    workspaceSnapshots: [
      {
        cwd: resources,
        checkedAt: "2026-10-05T10:00:00Z",
        slashCommands: names.map((name, index) =>
          index === 0
            ? {
                name,
                description: " Summarize the conversation ",
                input: { hint: "<instructions>" },
              }
            : { name },
        ),
        skills: [
          { name: "deploy-notes", description: "Write deploy notes", path: "/s", enabled: true },
          { name: "disabled", path: "/s", enabled: false },
          { name: "agent-only", path: "/s", enabled: true, userInvocable: false },
          { name: "only-person", path: "/s", enabled: true, userInvocationOnly: true },
          { name: names[1], path: "/s", enabled: true },
          { name: "x".repeat(101), path: "/s", enabled: true },
          { malformed: true },
        ],
      },
    ],
  });
  const commands = Array.from({ length: 30 }, (_, index) =>
    index === 0 ? "compact" : `command-${String(index)}`,
  );
  fake.providers = [listing("claudeAgent", commands), listing("codex", ["status"])];
  const { commands: listed } = await runtime.commands(conversation);
  expect(listed.slice(0, 2)).toEqual([
    { name: "compact", description: "Summarize the conversation", hint: "<instructions>" },
    { name: "command-1" },
  ]);
  expect(listed.map((command) => command.name).slice(30)).toEqual(["deploy-notes", "only-person"]);
  expect(listed).toHaveLength(32);
  // Reading creates no thread; the instance is the one the thread would open with.
  expect(fake.threads.size).toBe(0);
  expect(fake.refreshes).toEqual([{ instanceId: "claudeAgent", cwd: resources }]);

  // A thread T3's UI moved onto another instance offers that instance's commands.
  const reply = execute(turn("turn-1", "hi"));
  answer(await started("turn-1"), "Hello.");
  await reply;
  thread().modelSelection = { instanceId: "codex", model: "gpt-6-astra", options: [] };
  expect((await runtime.commands(conversation)).commands.map((command) => command.name)).toEqual([
    "status",
    "deploy-notes",
    "only-person",
  ]);
  // So does an agent whose default is on it.
  const code = { ...conversation, agent_id: "code" };
  expect((await runtime.commands(code)).commands.map((command) => command.name)).toEqual([
    "status",
    "deploy-notes",
    "only-person",
  ]);
});

it("runs different conversations at once and each conversation's turns in order", async () => {
  const other = { audience_id: "conversation-2" };
  const otherThread = derivedId("thread", "discord-private", "default", "conversation-2");
  const otherRun = (correlation: string) =>
    fake.threads
      .get(otherThread)
      ?.runs.find((run) => run.userMessageId === derivedId("message", correlation));
  const first = execute(turn("turn-1", "long work"));
  const long = await started("turn-1");
  // The same conversation's next turn waits for the first, in Carillon's order.
  const second = execute(turn("turn-2", "next"));
  // Another conversation proceeds meanwhile.
  const unrelated = execute(turn("turn-3", "elsewhere", other));
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
  const otherThread = derivedId("thread", "discord-private", "default", "conversation-2");
  const controller = new AbortController();
  const cancelled = execute(turn("turn-1", "stop me"), controller.signal);
  const mine = await started("turn-1");
  const kept = execute(turn("turn-2", "keep me", { audience_id: "conversation-2" }));
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

it("refuses a snapshot of another thread, whether read once or followed through a turn", async () => {
  const reply = execute(turn("turn-1", "hi"));
  answer(await started("turn-1"), "Hello.");
  await reply;
  fake.snapshotThread = () => "another-thread";
  // Another conversation's model and commands are never shown as this one's.
  await expect(runtime.modelState(conversation)).rejects.toMatchObject({ status: 502 });
  await expect(runtime.commands(conversation)).rejects.toMatchObject({ status: 502 });
  // A turn does not fold another thread's state as its own, nor send its
  // message on the strength of it.
  await expect(execute(turn("turn-2", "again"))).rejects.toMatchObject({
    message: "T3 answered in a form this runtime does not understand.",
  });
  expect(fake.received.filter((command) => command.type === "message.dispatch")).toHaveLength(1);
});

it("keeps a failed turn's report with its failure, taken before the turn's entry goes", async () => {
  const result = execute(turn("turn-1", "build it"));
  const run = await started("turn-1");
  const make = fake.tool(threadId, run.id, { type: "command_execution", input: "make release" });
  fake.finishTool(threadId, make, "failed");
  fake.fail(threadId, run.id, "The build broke.");
  const failure = await result.then(
    () => undefined,
    (error: unknown) => error,
  );
  expect(failure).toMatchObject({ message: "The build broke." });
  expect(reportOf(failure)).toMatchObject({
    tool_calls: 1,
    recent: [{ tool: "bash", detail: "make release", ok: false }],
    agent: { model: "claude-sonnet-5", effort: "low" },
  });
  expect(runtime.activity("turn-1", "discord-private")).toBeUndefined();
});

it("settles a failure that is no error without an explanation, keeping the turn's report", async () => {
  // A transport whose message dispatch fails, as an abandoned call does, for
  // a reason that is no error; its other calls reach T3.
  const real = await connectT3({ url: fake.url, token: fake.token });
  const abandoned = () =>
    Promise.resolve().then(() => {
      AbortSignal.abort("the socket went away").throwIfAborted();
    });
  const failing = build({
    connect: () =>
      Promise.resolve({
        call: (tag, payload) =>
          typeof payload === "object" &&
          payload !== null &&
          "type" in payload &&
          payload.type === "message.dispatch"
            ? abandoned()
            : real.call(tag, payload),
        subscribe: (tag, payload, onItem) => real.subscribe(tag, payload, onItem),
        closed: real.closed,
        close: () => undefined,
      }),
  });
  try {
    const failure = await execute(turn("turn-1", "build it"), undefined, failing).then(
      () => undefined,
      (error: unknown) => error,
    );
    expect(failure).toBeInstanceOf(TurnFailed);
    expect(failure).toMatchObject({ message: "" });
    expect(reportOf(failure)).toMatchObject({ tool_calls: 0, recent: [] });
  } finally {
    await failing.close();
    real.close();
  }
});

it("leaves no snapshot deadline behind when a thread read cannot subscribe", async () => {
  // A transport whose subscription fails as it is set up; its calls reach T3.
  const real = await connectT3({ url: fake.url, token: fake.token });
  const failure = new Error("the transport could not send the subscription");
  const refusing = await readBack(() =>
    build({
      connect: () =>
        Promise.resolve({
          call: (tag, payload) => real.call(tag, payload),
          subscribe: () => {
            throw failure;
          },
          closed: real.closed,
          close: () => undefined,
        }),
    }),
  );
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  try {
    // The model read fails with the subscription's own failure, and nothing
    // of it is left to fire once the read's deadline has passed.
    await expect(refusing.modelState(conversation)).rejects.toBe(failure);
    expect.soft(vi.getTimerCount()).toBe(0);
    expect.soft(() => vi.advanceTimersByTime(2_000)).not.toThrow();
  } finally {
    vi.useRealTimers();
    await refusing.close();
    real.close();
  }
});

it("still bounds a thread read whose snapshot never arrives", async () => {
  const reply = execute(turn("turn-1", "hi"));
  answer(await started("turn-1"), "Hello.");
  await reply;
  fake.holdSnapshot = true;
  await expect(runtime.modelState(conversation)).rejects.toMatchObject({
    status: 502,
    message: "Lost contact with T3: T3 did not send the thread snapshot in time.",
  });
});
