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
    expect(activity()).toEqual({ phase: "model", running: [], recent: [], tool_calls: 0 });
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
    expect(working?.recent).toEqual(
      Array.from({ length: 5 }, () => ({ tool: "read", detail: "notes.txt", ok: true })),
    );
    expect(JSON.stringify(working)).not.toContain("notes\\n");

    await writeFile(join(root, "one"), "");
    await expect.poll(() => activity()?.running.length).toBe(1);
    expect(activity()?.recent.at(-1)).toEqual({ tool: "bash", detail: wait("one"), ok: true });
    await writeFile(join(root, "two"), "");
    // Model work after the tools: nothing is shown as still executing.
    await expect.poll(() => activity()?.running.length).toBe(0);
    expect(activity()?.phase).toBe("model");

    finalResponse.resolve(undefined);
    expect((await turn).output).toContain("All done.");
    expect(activity()).toBeUndefined();
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
    expect(snapshots[1]?.recent).toEqual([{ tool: "read", detail: "missing.txt", ok: false }]);
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
