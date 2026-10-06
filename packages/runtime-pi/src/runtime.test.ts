import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, mkdir, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ModelRuntime, SettingsManager } from "@earendil-works/pi-coding-agent";
import {
  createAssistantMessageEventStream,
  type AssistantMessage,
  type Model,
} from "@earendil-works/pi-ai";
import { expect, it, vi } from "vitest";
import { createPiRuntime, openConversation } from "@carillon/runtime-pi";
import { runSessionExample } from "../examples/session.ts";

it("imports the public package without startup, discovery or installation files", async () => {
  const home = await mkdtemp(join(tmpdir(), "carillon-import-"));
  try {
    const url = new URL("./index.ts", import.meta.url).href;
    const { stdout } = await promisify(execFile)(
      process.execPath,
      [
        "--input-type=module",
        "-e",
        `await import(${JSON.stringify(url)}); console.log("imported");`,
      ],
      {
        cwd: home,
        env: { ...process.env, HOME: home, PI_CODING_AGENT_DIR: join(home, "pi") },
        timeout: 10_000,
      },
    );
    expect(stdout.trim()).toBe("imported");
    expect(await readdir(home)).toEqual([]);
  } finally {
    await rm(home, { recursive: true, force: true });
  }
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

it("composes native Pi directly, retains history and isolates independently constructed instances", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-runtime-"));
  const resources = join(root, "resources");
  await mkdir(resources);
  const models = await ModelRuntime.create({
    authPath: join(root, "auth.json"),
    modelsPath: null,
    modelsStorePath: join(root, "models.json"),
    allowModelNetwork: false,
    refreshOnCreate: false,
  });
  vi.spyOn(models, "getAvailable").mockResolvedValue([model]);
  vi.spyOn(models, "getAuth").mockResolvedValue({ auth: { apiKey: "test-only" } });
  vi.spyOn(models, "hasConfiguredAuth").mockReturnValue(true);
  const observed: string[][] = [];
  vi.spyOn(models, "streamSimple").mockImplementation((_model, context) => {
    observed.push(
      context.messages
        .filter((message) => message.role === "user")
        .map((message) =>
          typeof message.content === "string" ? message.content : JSON.stringify(message.content),
        ),
    );
    const text = `Observed ${String(observed.at(-1)?.length)} user messages.`;
    const message: AssistantMessage = {
      role: "assistant",
      content: [{ type: "text", text }],
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
      stopReason: "stop",
      timestamp: Date.now(),
    };
    const stream = createAssistantMessageEventStream();
    queueMicrotask(() => {
      stream.push({ type: "start", partial: message });
      stream.push({ type: "text_start", contentIndex: 0, partial: message });
      stream.push({ type: "text_delta", contentIndex: 0, delta: text, partial: message });
      stream.push({ type: "text_end", contentIndex: 0, content: text, partial: message });
      stream.push({ type: "done", reason: "stop", message });
    });
    return stream;
  });
  const settings = () =>
    SettingsManager.inMemory({ defaultProvider: model.provider, defaultModel: model.id });
  const first = await createPiRuntime({
    workspace: root,
    stateDirectory: join(root, "one"),
    modelRuntime: models,
    settings: settings(),
  });
  const second = await createPiRuntime({
    workspace: root,
    stateDirectory: join(root, "two"),
    modelRuntime: models,
    settings: settings(),
  });
  try {
    expect(await runSessionExample(first, resources)).toEqual([
      "Observed 1 user messages.",
      "Observed 2 user messages.",
    ]);
    const query = { interface_id: "example", agent_id: "example", audience_id: "native-session" };
    await first.setModel({
      ...query,
      scope: "agent",
      selection: { provider: model.provider, model: model.id },
    });
    expect((await first.modelState(query)).agent.overridden).toBe(true);
    expect((await second.modelState(query)).agent.overridden).toBe(false);
    const saved = openConversation(root, join(root, "one", "sessions"), query);
    expect(
      saved.buildSessionContext().messages.filter((message) => message.role === "user"),
    ).toHaveLength(2);
    await first.close();
    await expect(first.modelState(query)).rejects.toThrow("closed");
    expect(await runSessionExample(second, resources)).toEqual([
      "Observed 1 user messages.",
      "Observed 2 user messages.",
    ]);
    expect(observed.map((history) => history.length)).toEqual([1, 2, 1, 2]);
  } finally {
    await Promise.all([first.close(), second.close()]);
    vi.restoreAllMocks();
    await rm(root, { recursive: true, force: true });
  }
});

it("keeps a running turn's model when an update commits while its catalogue is read", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-pin-"));
  const later: Model<"openai-responses"> = { ...model, id: "later", name: "Later" };
  const models = await ModelRuntime.create({
    authPath: join(root, "auth.json"),
    modelsPath: null,
    modelsStorePath: join(root, "models.json"),
    allowModelNetwork: false,
    refreshOnCreate: false,
  });
  const catalogue = vi.spyOn(models, "getAvailable").mockResolvedValue([model, later]);
  vi.spyOn(models, "getAuth").mockResolvedValue({ auth: { apiKey: "test-only" } });
  vi.spyOn(models, "hasConfiguredAuth").mockReturnValue(true);
  const used: string[] = [];
  vi.spyOn(models, "streamSimple").mockImplementation((selected) => {
    used.push(selected.id);
    const message: AssistantMessage = {
      role: "assistant",
      content: [{ type: "text", text: "done" }],
      api: model.api,
      provider: model.provider,
      model: selected.id,
      usage: {
        input: 0,
        output: 0,
        cacheRead: 0,
        cacheWrite: 0,
        totalTokens: 0,
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
      },
      stopReason: "stop",
      timestamp: Date.now(),
    };
    const stream = createAssistantMessageEventStream();
    queueMicrotask(() => {
      stream.push({ type: "start", partial: message });
      stream.push({ type: "done", reason: "stop", message });
    });
    return stream;
  });
  const runtime = await createPiRuntime({
    workspace: root,
    stateDirectory: join(root, "state"),
    modelRuntime: models,
    settings: SettingsManager.inMemory({ defaultProvider: model.provider, defaultModel: model.id }),
  });
  const query = { interface_id: "test", agent_id: "test", audience_id: "test" };
  try {
    // The turn starts, and its catalogue read is slow.
    const slow = Promise.withResolvers<readonly Model<typeof model.api>[]>();
    catalogue.mockReturnValueOnce(slow.promise);
    const turn = runtime.executeTurn({
      ...query,
      correlation_id: "running",
      input: "hello",
      resource_root: root,
      attachment_root: root,
    });
    await Promise.resolve();
    // Meanwhile an update reads its own catalogue and commits.
    await runtime.setModel({
      ...query,
      scope: "agent",
      selection: { provider: later.provider, model: later.id },
    });
    expect((await runtime.modelState(query)).agent.selection?.model).toBe(later.id);
    slow.resolve([model, later]);
    await turn;
    expect(used).toEqual([model.id]);
  } finally {
    await runtime.close();
    vi.restoreAllMocks();
    await rm(root, { recursive: true, force: true });
  }
});

it("close waits for accepted operations and prevents queued execution from starting", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-close-"));
  const models = await ModelRuntime.create({
    authPath: join(root, "auth.json"),
    modelsPath: null,
    modelsStorePath: join(root, "models.json"),
    refreshOnCreate: false,
  });
  const catalogue = vi.spyOn(models, "getAvailable").mockResolvedValue([model]);
  const runtime = await createPiRuntime({
    workspace: root,
    stateDirectory: join(root, "state"),
    modelRuntime: models,
    settings: SettingsManager.inMemory({ defaultProvider: model.provider, defaultModel: model.id }),
  });
  const deferred = Promise.withResolvers<readonly Model<typeof model.api>[]>();
  catalogue.mockReturnValue(deferred.promise);
  const request = {
    interface_id: "test",
    agent_id: "test",
    audience_id: "test",
    correlation_id: "one",
    input: "Do not execute",
    resource_root: root,
    attachment_root: root,
  };
  try {
    const active = runtime.executeTurn(request);
    // The first call has entered the serial queue and is waiting for its catalogue.
    await Promise.resolve();
    const queued = runtime.executeTurn({ ...request, correlation_id: "two" });
    const result = Promise.allSettled([active, queued]);
    let closed = false;
    const closing = runtime.close().then(() => {
      closed = true;
    });
    await Promise.resolve();
    expect(closed).toBe(false);
    deferred.resolve([model]);
    await closing;
    expect((await result).map((entry) => entry.status)).toEqual(["rejected", "rejected"]);
    expect(catalogue).toHaveBeenCalledTimes(2); // construction and the first turn, never the queued turn
    await expect(runtime.executeTurn(request)).rejects.toThrow("closed");
  } finally {
    deferred.resolve([model]);
    await runtime.close();
    vi.restoreAllMocks();
    await rm(root, { recursive: true, force: true });
  }
});
