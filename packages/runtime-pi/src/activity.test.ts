import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ModelRuntime, SettingsManager } from "@earendil-works/pi-coding-agent";
import {
  createAssistantMessageEventStream,
  type AssistantMessage,
  type Model,
  type ToolCall,
} from "@earendil-works/pi-ai";
import type { TurnActivity } from "@carillon/protocol";
import { TurnActivitySchema } from "@carillon/protocol";
import { Check } from "typebox/value";
import { afterEach, expect, it, vi } from "vitest";
import { createPiRuntime } from "@carillon/runtime-pi";
import { ExplainedFailure, reportOf } from "@carillon/runtime/failure";
import { trackActivity } from "./activity.ts";

afterEach(() => {
  vi.restoreAllMocks();
});

const model: Model<"openai-responses"> = {
  id: "fixture",
  name: "Fixture",
  api: "openai-responses",
  provider: "openai",
  baseUrl: "https://example.invalid",
  reasoning: false,
  input: ["text"],
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  contextWindow: 16_000,
  maxTokens: 100,
};

function assistant(content: AssistantMessage["content"]): AssistantMessage {
  return {
    role: "assistant",
    content,
    api: model.api,
    provider: model.provider,
    model: model.id,
    usage: {
      input: 0,
      output: 0,
      cacheRead: 0,
      cacheWrite: 0,
      totalTokens: 0,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
    },
    stopReason: content.some((block) => block.type === "toolCall") ? "toolUse" : "stop",
    timestamp: Date.now(),
  };
}

/** A model response that starts only when `gate` resolves, streaming its text before its tool calls. */
function scripted(gate: Promise<unknown>, text: string, calls: ToolCall[]) {
  const message = assistant([{ type: "text", text }, ...calls]);
  const stream = createAssistantMessageEventStream();
  void gate.then(() => {
    stream.push({ type: "start", partial: message });
    stream.push({ type: "text_start", contentIndex: 0, partial: message });
    stream.push({ type: "text_delta", contentIndex: 0, delta: text, partial: message });
    stream.push({ type: "text_end", contentIndex: 0, content: text, partial: message });
    stream.push({
      type: "done",
      reason: calls.length > 0 ? "toolUse" : "stop",
      message,
    });
  });
  return stream;
}

async function piRuntime(root: string) {
  const models = await ModelRuntime.create({
    authPath: join(root, "auth.json"),
    modelsPath: null,
    modelsStorePath: join(root, "models.json"),
    allowModelNetwork: false,
    refreshOnCreate: false,
  });
  const catalogue = vi.spyOn(models, "getAvailable").mockResolvedValue([model]);
  vi.spyOn(models, "getAuth").mockResolvedValue({ auth: { apiKey: "test-only" } });
  vi.spyOn(models, "hasConfiguredAuth").mockReturnValue(true);
  const runtime = await createPiRuntime({
    workspace: root,
    stateDirectory: join(root, "state"),
    modelRuntime: models,
    settings: SettingsManager.inMemory({ defaultProvider: model.provider, defaultModel: model.id }),
  });
  return { models, catalogue, runtime };
}

const request = (root: string, correlation_id: string) => ({
  correlation_id,
  interface_id: "discord-private",
  agent_id: "default",
  audience_id: "conversation",
  input: "check the workspace",
  resource_root: join(root, "resources"),
  attachment_root: join(root, "attachments"),
});

it("reports a real turn's phases, slow and parallel tools, failures and bursts until it settles", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-activity-"));
  await mkdir(join(root, "resources"));
  await writeFile(join(root, "notes.txt"), "notes\n");
  const { models, runtime } = await piRuntime(root);
  const firstResponse = Promise.withResolvers<undefined>();
  const finalResponse = Promise.withResolvers<undefined>();
  const wait = (name: string) => `until [ -e ${name} ]; do sleep 0.02; done`;
  const reads = Array.from({ length: 10 }, (_, index) => ({
    type: "toolCall" as const,
    id: `read-${String(index)}`,
    name: "read",
    arguments: { path: join(root, "notes.txt") },
  }));
  let calls = 0;
  vi.spyOn(models, "streamSimple").mockImplementation(() => {
    calls += 1;
    return calls === 1
      ? scripted(firstResponse.promise, "Checking the workspace first.\nThen summarising.", [
          { type: "toolCall", id: "slow", name: "bash", arguments: { command: wait("one") } },
          { type: "toolCall", id: "slower", name: "bash", arguments: { command: wait("two") } },
          { type: "toolCall", id: "missing", name: "read", arguments: { path: "missing.txt" } },
          ...reads,
        ])
      : scripted(finalResponse.promise, "All done.", []);
  });
  const activity = () => runtime.activity("turn-1", "discord-private");
  try {
    const turn = runtime.executeTurn(request(root, "turn-1"));
    // Model-only work: nothing running and no commentary yet.
    await expect.poll(() => activity()?.phase).toBe("model");
    // The session says which model runs; this one does not reason.
    expect(activity()).toMatchObject({
      phase: "model",
      running: [],
      recent: [],
      tool_calls: 0,
      agent: { model: "fixture" },
    });
    expect(activity()?.headline).toBeUndefined();
    expect(activity()?.agent?.effort).toBeUndefined();
    // Other interfaces and other turns are not described.
    expect(runtime.activity("turn-1", "other-interface")).toBeUndefined();
    expect(runtime.activity("turn-2", "discord-private")).toBeUndefined();

    firstResponse.resolve(undefined);
    await expect.poll(() => activity()?.running.length).toBe(2);
    await expect.poll(() => activity()?.recent.length).toBe(5);
    const working = activity();
    expect(Check(TurnActivitySchema, working)).toBe(true);
    expect(working).toMatchObject({
      phase: "tools",
      headline: "Checking the workspace first.",
      running: [
        { tool: "bash", detail: wait("one") },
        { tool: "bash", detail: wait("two") },
      ],
      tool_calls: 13,
    });
    // The failed call was recorded before the burst pushed it out of the recent list.
    expect(working?.recent).toMatchObject(
      Array.from({ length: 5 }, () => ({ tool: "read", detail: "notes.txt", ok: true })),
    );
    // Pi's own start and end events time each call.
    expect(working?.recent.every((call) => typeof call.duration_ms === "number")).toBe(true);
    expect(JSON.stringify(working)).not.toContain("notes\\n");

    await writeFile(join(root, "one"), "");
    await expect.poll(() => activity()?.running.length).toBe(1);
    expect(activity()?.recent.at(-1)).toMatchObject({
      tool: "bash",
      detail: wait("one"),
      ok: true,
    });
    await writeFile(join(root, "two"), "");
    // Model work after the tools: nothing is shown as still executing.
    await expect.poll(() => activity()?.running.length).toBe(0);
    expect(activity()?.phase).toBe("model");

    finalResponse.resolve(undefined);
    const answered = await turn;
    expect(answered.output).toContain("All done.");
    expect(activity()).toBeUndefined();
    // The answer carries the turn's final report, taken before its activity
    // was removed: every call counted, the latest finished, nothing running.
    expect(Check(TurnActivitySchema, answered.activity)).toBe(true);
    expect(answered.activity).toMatchObject({
      running: [],
      tool_calls: 13,
      agent: { model: "fixture" },
    });
    expect(answered.activity?.recent.at(-1)).toMatchObject({ tool: "bash", detail: wait("two") });
  } finally {
    firstResponse.resolve(undefined);
    finalResponse.resolve(undefined);
    await runtime.close();
    await rm(root, { recursive: true, force: true });
  }
});

it("records a failed tool and the call that recovers from it", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-activity-"));
  await mkdir(join(root, "resources"));
  await writeFile(join(root, "notes.txt"), "notes\n");
  const { models, runtime } = await piRuntime(root);
  const hold = Promise.withResolvers<undefined>();
  const snapshots: (TurnActivity | undefined)[] = [];
  let calls = 0;
  vi.spyOn(models, "streamSimple").mockImplementation(() => {
    calls += 1;
    snapshots.push(runtime.activity("turn-1", "discord-private"));
    const read = (id: string, path: string): ToolCall => ({
      type: "toolCall",
      id,
      name: "read",
      arguments: { path },
    });
    if (calls === 1) return scripted(Promise.resolve(), "Reading.", [read("a", "missing.txt")]);
    if (calls === 2) return scripted(Promise.resolve(), "Retrying.", [read("b", "notes.txt")]);
    return scripted(hold.promise, "Done.", []);
  });
  try {
    const turn = runtime.executeTurn(request(root, "turn-1"));
    await expect.poll(() => calls).toBe(3);
    expect(runtime.activity("turn-1", "discord-private")).toMatchObject({
      phase: "model",
      headline: "Retrying.",
      running: [],
      recent: [
        { tool: "read", detail: "missing.txt", ok: false },
        { tool: "read", detail: "notes.txt", ok: true },
      ],
      tool_calls: 2,
    });
    expect(snapshots[1]?.recent).toMatchObject([
      { tool: "read", detail: "missing.txt", ok: false },
    ]);
    hold.resolve(undefined);
    await turn;
  } finally {
    hold.resolve(undefined);
    await runtime.close();
    await rm(root, { recursive: true, force: true });
  }
});

it("removes a turn's activity when preparation fails before its session exists", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-activity-"));
  await mkdir(join(root, "resources"));
  const { catalogue, runtime } = await piRuntime(root);
  const loading = Promise.withResolvers<readonly Model<typeof model.api>[]>();
  catalogue.mockReturnValue(loading.promise);
  try {
    const turn = runtime.executeTurn(request(root, "turn-1"));
    const failed = expect(turn).rejects.toThrow("catalogue unavailable");
    await expect
      .poll(() => runtime.activity("turn-1", "discord-private"))
      .toEqual({
        phase: "starting",
        running: [],
        recent: [],
        tool_calls: 0,
      });
    loading.reject(new Error("catalogue unavailable"));
    await failed;
    expect(runtime.activity("turn-1", "discord-private")).toBeUndefined();
  } finally {
    await runtime.close();
    await rm(root, { recursive: true, force: true });
  }
});

it("shows child agents only from a participating extension's explicit lifecycle reports", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-activity-"));
  await mkdir(join(root, "resources", "extensions"), { recursive: true });
  // A participating extension: real child processes, each reported from queued to its exit.
  // `solo` runs a child process too but reports nothing, so it stays an ordinary tool.
  await writeFile(
    join(root, "resources", "extensions", "team.ts"),
    `
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
const waitFor = (file) => \`const fs=require("fs");const t=setInterval(()=>{if(fs.existsSync(\${JSON.stringify(file)})){clearInterval(t);process.exit(fs.readFileSync(\${JSON.stringify(file)},"utf8")==="fail"?3:0)}},20)\`;
function child(file) {
  return new Promise((resolve) => {
    const process_ = spawn(process.execPath, ["-e", waitFor(file)], { stdio: "ignore" });
    process_.on("exit", (code) => resolve(code));
  });
}
export default function (pi) {
  const report = (fact) => pi.events.emit("carillon:work", fact);
  pi.registerTool({
    name: "team", label: "Team", description: "Runs two child agents",
    parameters: { type: "object", properties: {} },
    async execute() {
      report({ id: "lead", kind: "delegation", label: "lead child\\nsecond line", state: "queued", actor: "pi child" });
      report({ id: "helper", kind: "delegation", label: "helper child", state: "queued", parent: "lead" });
      report({ kind: "delegation", label: "no identity", state: "running" });
      report({ id: "bad", kind: "agent", label: "unknown kind", state: "running" });
      report({ id: "lead", kind: "delegation", label: "lead child", state: "running", actor: "pi child" });
      report({ id: "helper", kind: "delegation", label: "helper child", state: "running", parent: "lead" });
      await Promise.all([
        child(${JSON.stringify(join(root, "lead"))}).then((code) =>
          report({ id: "lead", kind: "delegation", label: "lead child", state: code === 0 ? "succeeded" : "failed", actor: "pi child" })),
        child(${JSON.stringify(join(root, "helper"))}).then((code) =>
          report({ id: "helper", kind: "delegation", label: "helper child", state: code === 0 ? "succeeded" : "failed", parent: "lead" })),
      ]);
      return { content: [{ type: "text", text: "team finished" }], details: {} };
    },
  });
  pi.registerTool({
    name: "solo", label: "Solo", description: "Runs one unreported child",
    parameters: { type: "object", properties: {} },
    async execute() {
      await child(${JSON.stringify(join(root, "solo"))});
      return { content: [{ type: "text", text: "solo finished" }], details: {} };
    },
  });
}
`,
  );
  const { models, runtime } = await piRuntime(root);
  const finalResponse = Promise.withResolvers<undefined>();
  let calls = 0;
  vi.spyOn(models, "streamSimple").mockImplementation(() => {
    calls += 1;
    return calls === 1
      ? scripted(Promise.resolve(), "Starting the team.", [
          { type: "toolCall", id: "team-1", name: "team", arguments: {} },
          { type: "toolCall", id: "solo-1", name: "solo", arguments: {} },
        ])
      : scripted(finalResponse.promise, "Team reported.", []);
  });
  const activity = () => runtime.activity("turn-1", "discord-private");
  try {
    const turn = runtime.executeTurn(request(root, "turn-1"));
    // Observed below once the children are released; kept handled meanwhile.
    void turn.catch(() => undefined);
    await expect
      .poll(() => activity()?.work?.items.map((item) => item.state))
      .toEqual(["running", "running"]);
    const running = activity();
    expect(Check(TurnActivitySchema, running)).toBe(true);
    // Start times are when the runtime first saw the work running, marked as
    // estimates; everything else is as reported.
    expect(
      running?.work?.items.map((item) => ({ ...item, started_at: typeof item.started_at })),
    ).toEqual([
      {
        id: "lead",
        kind: "delegation",
        label: "lead child",
        state: "running",
        actor: "pi child",
        started_at: "number",
        estimated: true,
      },
      {
        id: "helper",
        kind: "delegation",
        label: "helper child",
        state: "running",
        parent: "lead",
        started_at: "number",
        estimated: true,
      },
    ]);
    expect(running?.work?.partial).toBe(false);
    // The unreported child stays an ordinary running tool.
    expect(running?.running.map((tool) => tool.tool).sort()).toEqual(["solo", "team"]);

    await writeFile(join(root, "helper"), "fail");
    await expect
      .poll(() => activity()?.work?.items.map((item) => item.state))
      .toEqual(["running", "failed"]);
    // Finishing the parent tool alone proves nothing about a child: it is the report that does.
    await writeFile(join(root, "lead"), "ok");
    await writeFile(join(root, "solo"), "ok");
    // Finished work keeps the order it was reported in.
    await expect
      .poll(() => activity()?.work?.items.map((item) => item.state))
      .toEqual(["succeeded", "failed"]);
    finalResponse.resolve(undefined);
    expect((await turn).output).toContain("Team reported.");
    expect(activity()).toBeUndefined();
  } finally {
    finalResponse.resolve(undefined);
    for (const file of ["lead", "helper", "solo"]) await writeFile(join(root, file), "ok");
    await runtime.close();
    await rm(root, { recursive: true, force: true });
  }
});

it("keeps at most the snapshot limit of reported work, never dropping live work for more", () => {
  const tracker = trackActivity("/workspace");
  const report = (id: string, state: string) =>
    tracker.report({ id, kind: "delegation", label: `child ${id}`, state });
  for (let index = 0; index < 16; index += 1) report(`c${String(index)}`, "running");
  report("extra", "running");
  expect(tracker.snapshot().work).toMatchObject({ partial: true });
  expect(tracker.snapshot().work?.items).toHaveLength(16);
  expect(tracker.snapshot().work?.items.some((item) => item.id === "extra")).toBe(false);
  // Finished work makes room, oldest first; live work stays.
  report("c0", "succeeded");
  report("late", "running");
  const ids = tracker.snapshot().work?.items.map((item) => item.id) ?? [];
  expect(ids).toContain("late");
  expect(ids).not.toContain("c0");
  // Reports outside the vocabulary or bounds are ignored, never guessed.
  for (const invalid of [
    null,
    "running",
    { id: "", kind: "delegation", label: "x", state: "running" },
    { id: "y", kind: "delegation", label: "x", state: "done" },
    { id: "z".repeat(300), kind: "delegation", label: "x", state: "running" },
  ])
    tracker.report(invalid);
  expect(tracker.snapshot().work?.items).toHaveLength(16);
});

/** A response that thinks before it answers or calls tools, or that fails with Pi's error. */
function thinking(thought: string, text: string, calls: ToolCall[], failure?: string) {
  const message = {
    ...assistant([{ type: "thinking", thinking: thought }, { type: "text", text }, ...calls]),
    ...(failure === undefined ? {} : { stopReason: "error" as const, errorMessage: failure }),
  };
  const stream = createAssistantMessageEventStream();
  queueMicrotask(() => {
    stream.push({ type: "start", partial: message });
    stream.push({ type: "thinking_start", contentIndex: 0, partial: message });
    const half = Math.floor(thought.length / 2);
    for (const delta of [thought.slice(0, half), thought.slice(half)])
      stream.push({ type: "thinking_delta", contentIndex: 0, delta, partial: message });
    stream.push({ type: "thinking_end", contentIndex: 0, content: thought, partial: message });
    stream.push({ type: "text_start", contentIndex: 1, partial: message });
    stream.push({ type: "text_delta", contentIndex: 1, delta: text, partial: message });
    stream.push({ type: "text_end", contentIndex: 1, content: text, partial: message });
    if (failure === undefined)
      stream.push({ type: "done", reason: calls.length > 0 ? "toolUse" : "stop", message });
    else stream.push({ type: "error", reason: "error", error: message });
  });
  return stream;
}

it("keeps thinking as notes beside the answer, and a failed turn's report with its failure", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-activity-"));
  await mkdir(join(root, "resources"));
  await writeFile(join(root, "notes.txt"), "notes\n");
  const { models, runtime } = await piRuntime(root);
  const read: ToolCall = {
    type: "toolCall",
    id: "read",
    name: "read",
    arguments: { path: join(root, "notes.txt") },
  };
  const responses = [
    () => thinking("The notes file\nshould say what to check.", "Reading the notes.", [read]),
    () => thinking("Nothing else is needed.", "The notes say notes.", []),
    () => thinking("Reading them again.", "Again.", [read]),
    () => thinking("Answering.", "", [], "The usage limit has been reached"),
  ];
  vi.spyOn(models, "streamSimple").mockImplementation(() => {
    const next = responses.shift();
    if (next === undefined) throw new Error("no more responses");
    return next();
  });
  try {
    const answered = await runtime.executeTurn(request(root, "turn-1"));
    // Thinking is kept as notes, one per block on one line, and never
    // becomes the answer.
    expect(answered.output).toBe("Reading the notes.The notes say notes.");
    expect(answered.activity?.notes).toEqual([
      "The notes file should say what to check.",
      "Nothing else is needed.",
    ]);

    const failed = await runtime.executeTurn(request(root, "turn-2")).then(
      () => undefined,
      (failure: unknown) => failure,
    );
    expect(failed).toBeInstanceOf(ExplainedFailure);
    expect(failed).toMatchObject({ status: 502, message: "The usage limit has been reached" });
    // The failure keeps the turn's report: what it read, and what it thought.
    expect(reportOf(failed)).toMatchObject({
      tool_calls: 1,
      recent: [{ tool: "read", detail: "notes.txt", ok: true }],
      notes: ["Reading them again.", "Answering."],
      agent: { model: "fixture" },
    });
  } finally {
    await runtime.close();
    await rm(root, { recursive: true, force: true });
  }
});

it("times calls and reported work on the monotonic clock, keeps a finished duration, and marks observed starts", () => {
  let wall = 1_790_000_000_000;
  let monotonic = 500;
  vi.spyOn(Date, "now").mockImplementation(() => wall);
  vi.spyOn(performance, "now").mockImplementation(() => monotonic);
  const activity = trackActivity("/workspace");
  const fact = { id: "child", kind: "delegation", label: "Summarise the log", state: "running" };
  activity.report(fact);
  // Reported work has no start of its own: when it was first reported running is an estimate.
  expect(activity.snapshot().work?.items[0]).toMatchObject({
    started_at: 1_790_000_000,
    estimated: true,
  });
  // The wall clock steps back while the work runs; its duration does not.
  monotonic += 1_000;
  wall -= 60_000;
  activity.report({ ...fact, state: "succeeded" });
  expect(activity.snapshot().work?.items[0]).toMatchObject({
    state: "succeeded",
    started_at: 1_790_000_000,
    duration_ms: 1_000,
  });
  // Its end reported again keeps the duration it finished with.
  monotonic += 7_000;
  activity.report({ ...fact, state: "succeeded" });
  expect(activity.snapshot().work?.items[0]?.duration_ms).toBe(1_000);

  // A call's start is Pi's own event, never an estimate, and its duration is monotonic too.
  activity.observe({
    type: "tool_execution_start",
    toolCallId: "call",
    toolName: "bash",
    args: { command: "cargo check" },
  });
  expect(activity.snapshot().running[0]).toEqual({
    tool: "bash",
    detail: "cargo check",
    started_at: Math.floor(wall / 1000),
  });
  wall -= 1_000;
  monotonic += 250;
  activity.observe({
    type: "tool_execution_end",
    toolCallId: "call",
    toolName: "bash",
    isError: false,
    result: { content: [], details: {} },
  });
  expect(activity.snapshot().recent).toEqual([
    { tool: "bash", detail: "cargo check", ok: true, duration_ms: 250 },
  ]);
});
