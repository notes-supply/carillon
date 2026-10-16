import { ModelRuntime, SessionManager } from "@earendil-works/pi-coding-agent";
import {
  createAssistantMessageEventStream,
  type AssistantMessage,
  type Model,
} from "@earendil-works/pi-ai";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, it } from "vitest";
import { ExplainedFailure } from "@carillon/runtime/failure";
import { openConversation, createPiExecutionSession, promptSession } from "@carillon/runtime-pi";

const model: Model<"openai-responses"> = {
  id: "observer-model",
  name: "Observer Model",
  api: "openai-responses",
  provider: "openai",
  baseUrl: "https://example.invalid",
  reasoning: false,
  input: ["text"],
  cost: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 },
  contextWindow: 16_000,
  maxTokens: 2_000,
};

const assistantMessage: AssistantMessage = {
  role: "assistant",
  content: [{ type: "text", text: "observed" }],
  api: model.api,
  provider: model.provider,
  model: model.id,
  responseModel: "observer-model-2026-08-31",
  responseId: "response-123",
  usage: {
    input: 11,
    output: 7,
    cacheRead: 3,
    cacheWrite: 0,
    reasoning: 2,
    totalTokens: 21,
    cost: { input: 0.11, output: 0.14, cacheRead: 0.003, cacheWrite: 0, total: 0.253 },
  },
  stopReason: "stop",
  timestamp: 1_787_846_400_000,
};

it.each([true, false])("preserves native Pi with SYSTEM.md: %s", async (hasSystemPrompt) => {
  const resourceRoot = await mkdtemp(join(tmpdir(), "carillon-headless-pi-"));
  const ownerHome = await mkdtemp(join(tmpdir(), "carillon-owner-home-"));
  const workspace = await mkdtemp(join(tmpdir(), "carillon-workspace-"));
  const extensions = join(resourceRoot, "extensions");
  const sessions = join(resourceRoot, "sessions");
  const extensionState = join(ownerHome, "extension-state.json");
  await mkdir(extensions);
  await mkdir(sessions);
  if (hasSystemPrompt) await writeFile(join(resourceRoot, "SYSTEM.md"), "Observe the test turn.\n");
  await writeFile(
    join(extensions, "observer.ts"),
    `import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";

export default function observer(pi) {
  const events = [];
  writeFileSync(process.env.CARILLON_TEST_EXTENSION_STATE, JSON.stringify({
    uid: process.getuid(),
    groups: process.getgroups(),
    home: process.env.HOME,
    shell: process.env.SHELL,
    path: process.env.PATH,
    subprocess: execFileSync(process.execPath, ["-e", "process.stdout.write(process.env.HOME)"], {
      encoding: "utf8",
    }),
  }));
  pi.on("session_start", () => events.push("session_start"));
  pi.on("agent_start", () => events.push("agent_start"));
  pi.on("turn_start", () => events.push("turn_start"));
  pi.on("message_end", ({ message }) => {
    if (message.role === "assistant") events.push("assistant_message_end");
  });
  pi.on("turn_end", ({ message }) => {
    events.push("turn_end");
    pi.appendEntry("observer-turn", {
      events,
      provider: message.provider,
      model: message.model,
      usage: message.usage,
    });
  });
}
`,
  );
  if (process.getuid === undefined || process.getgroups === undefined)
    throw new Error("Pi authority test requires Unix process identity APIs");
  const uid = process.getuid();
  const groups = process.getgroups();
  const previousHome = process.env["HOME"];
  const previousShell = process.env["SHELL"];
  const previousExtensionState = process.env["CARILLON_TEST_EXTENSION_STATE"];
  process.env["HOME"] = ownerHome;
  process.env["SHELL"] = "/bin/sh";
  process.env["CARILLON_TEST_EXTENSION_STATE"] = extensionState;
  await writeFile(join(ownerHome, "readable.txt"), "owner home sentinel\n");

  const baseRuntime = await ModelRuntime.create({
    allowModelNetwork: false,
    refreshOnCreate: false,
    modelsPath: null,
  });
  const runtime = new Proxy(baseRuntime, {
    get(target, property): unknown {
      if (property === "getAuth") return () => Promise.resolve({ auth: { apiKey: "fixture-key" } });
      if (property === "hasConfiguredAuth") return () => true;
      if (property === "streamSimple") {
        return () => {
          const stream = createAssistantMessageEventStream();
          queueMicrotask(() => {
            stream.push({ type: "start", partial: assistantMessage });
            stream.push({ type: "text_start", contentIndex: 0, partial: assistantMessage });
            stream.push({
              type: "text_delta",
              contentIndex: 0,
              delta: "observed",
              partial: assistantMessage,
            });
            stream.push({
              type: "text_end",
              contentIndex: 0,
              content: "observed",
              partial: assistantMessage,
            });
            stream.push({ type: "done", reason: "stop", message: assistantMessage });
          });
          return stream;
        };
      }
      const value: unknown = Reflect.get(target, property, target);
      return typeof value === "function" ? value.bind(target) : value;
    },
  });

  try {
    const selected = SessionManager.create(workspace, sessions);
    const created = await createPiExecutionSession({
      resourceRoot,
      workspace,
      manager: selected,
      modelRuntime: runtime,
      model,
    });
    const { session, extensionsResult } = created;
    try {
      expect(extensionsResult.errors).toEqual([]);
      expect(session.systemPrompt.length).toBeGreaterThan(0);
      expect(session.systemPrompt.includes("Observe the test turn.")).toBe(hasSystemPrompt);
      expect(session.sessionManager.getCwd()).toBe(workspace);
      expect(session.getActiveToolNames()).toEqual(
        expect.arrayContaining(["read", "bash", "edit", "write"]),
      );
      const executeTool = async (name: string, parameters: Record<string, unknown>) => {
        const tool = session.getToolDefinition(name);
        if (tool === undefined) throw new Error(`Pi did not activate ${name}`);
        return tool.execute(
          "authority-test",
          parameters,
          undefined,
          undefined,
          session.extensionRunner.createToolContext("authority-test", undefined),
        );
      };
      await executeTool("write", { path: "workspace.txt", content: "created in workspace\n" });
      await executeTool("edit", {
        path: "workspace.txt",
        edits: [{ oldText: "created", newText: "edited" }],
      });
      await executeTool("write", {
        path: join(ownerHome, "absolute.txt"),
        content: "absolute owner path\n",
      });
      const readResult = await executeTool("read", { path: join(ownerHome, "readable.txt") });
      const bashResult = await executeTool("bash", { command: "id -u" });
      expect(await readFile(join(workspace, "workspace.txt"), "utf8")).toBe(
        "edited in workspace\n",
      );
      expect(await readFile(join(ownerHome, "absolute.txt"), "utf8")).toBe("absolute owner path\n");
      expect(JSON.stringify(readResult)).toContain("owner home sentinel");
      expect(JSON.stringify(bashResult)).toContain(String(uid));
      const extensionEnvironment: unknown = JSON.parse(await readFile(extensionState, "utf8"));
      expect(extensionEnvironment).toEqual({
        uid,
        groups,
        home: ownerHome,
        shell: "/bin/sh",
        path: process.env["PATH"],
        subprocess: ownerHome,
      });
      await session.prompt("record this turn");
      const sessionFile = session.sessionFile;
      if (sessionFile === undefined) throw new Error("Pi did not persist the test session");

      const entries = SessionManager.open(sessionFile).getEntries();
      expect(entries).toContainEqual(
        expect.objectContaining({
          type: "model_change",
          provider: "openai",
          modelId: "observer-model",
        }),
      );
      const recordedAssistant = entries.find(
        (entry) => entry.type === "message" && entry.message.role === "assistant",
      );
      expect(recordedAssistant).toMatchObject({
        type: "message",
        message: {
          provider: "openai",
          model: "observer-model",
          responseModel: "observer-model-2026-08-31",
          responseId: "response-123",
          usage: assistantMessage.usage,
        },
      });
      const observerEntry = entries.find(
        (entry) => entry.type === "custom" && entry.customType === "observer-turn",
      );
      expect(observerEntry).toMatchObject({
        type: "custom",
        data: {
          events: [
            "session_start",
            "agent_start",
            "turn_start",
            "assistant_message_end",
            "turn_end",
          ],
          provider: "openai",
          model: "observer-model",
          usage: assistantMessage.usage,
        },
      });
    } finally {
      session.dispose();
    }
  } finally {
    process.env["HOME"] = previousHome;
    process.env["SHELL"] = previousShell;
    if (previousExtensionState === undefined) delete process.env["CARILLON_TEST_EXTENSION_STATE"];
    else process.env["CARILLON_TEST_EXTENSION_STATE"] = previousExtensionState;
    await rm(resourceRoot, { recursive: true, force: true });
    await rm(ownerHome, { recursive: true, force: true });
    await rm(workspace, { recursive: true, force: true });
  }
});

it.each([false, true])(
  "cancels a native tool and accepts a new message in the same session (established=%s)",
  async (established) => {
    const root = await mkdtemp(join(tmpdir(), "carillon-cancel-pi-"));
    const attempts = join(root, "attempts");
    const finished = join(root, "finished");
    const key = { interface_id: "discord", agent_id: "default", audience_id: "conversation" };
    const baseRuntime = await ModelRuntime.create({
      allowModelNetwork: false,
      refreshOnCreate: false,
      modelsPath: null,
    });
    let calls = 0;
    const runtime = new Proxy(baseRuntime, {
      get(target, property): unknown {
        if (property === "getAuth") return () => Promise.resolve({ auth: { apiKey: "test-only" } });
        if (property === "hasConfiguredAuth") return () => true;
        if (property === "streamSimple")
          return (...args: Parameters<ModelRuntime["streamSimple"]>) => {
            if (args[2]?.signal?.aborted === true) {
              const stream = createAssistantMessageEventStream();
              queueMicrotask(() =>
                stream.push({
                  type: "error",
                  reason: "aborted",
                  error: {
                    ...assistantMessage,
                    stopReason: "aborted",
                    content: [],
                    errorMessage: "aborted",
                  },
                }),
              );
              return stream;
            }
            calls += 1;
            const message: AssistantMessage =
              calls === 1
                ? {
                    ...assistantMessage,
                    stopReason: "toolUse",
                    content: [
                      {
                        type: "toolCall",
                        id: "bounded-side-effect",
                        name: "bash",
                        arguments: {
                          command: `printf 'attempt\n' >> '${attempts}'; sleep 30; printf 'finished' > '${finished}'`,
                        },
                      },
                    ],
                  }
                : assistantMessage;
            const stream = createAssistantMessageEventStream();
            queueMicrotask(() =>
              stream.push({
                type: "done",
                reason: message.stopReason === "toolUse" ? "toolUse" : "stop",
                message,
              }),
            );
            return stream;
          };
        const value: unknown = Reflect.get(target, property, target);
        return typeof value === "function" ? value.bind(target) : value;
      },
    });
    try {
      const manager = openConversation(root, join(root, "sessions"), key);
      if (established) {
        manager.appendMessage({ role: "user", content: "earlier message", timestamp: 1 });
        manager.appendMessage(assistantMessage);
      }
      const { session } = await createPiExecutionSession({
        resourceRoot: root,
        workspace: root,
        manager,
        modelRuntime: runtime,
        model,
      });
      const controller = new AbortController();
      try {
        const running = promptSession(session, "run the bounded test", controller.signal);
        const rejected = running.then(
          () => undefined,
          (error: unknown) => error,
        );
        await expect
          .poll(() => readFile(attempts, "utf8").catch(() => ""), { timeout: 5_000 })
          .toBe("attempt\n");
        controller.abort();
        expect(await rejected).toMatchObject({ name: "AbortError" });
        expect(session.isIdle).toBe(true);
      } finally {
        await session.abort();
        session.dispose();
      }
      const reopened = openConversation(root, join(root, "sessions"), key);
      expect(reopened.getSessionFile()).toBe(manager.getSessionFile());
      const { session: next } = await createPiExecutionSession({
        resourceRoot: root,
        workspace: root,
        manager: reopened,
        modelRuntime: runtime,
        model,
      });
      try {
        await promptSession(next, "new direction", new AbortController().signal);
        expect(next.messages.filter((message) => message.role === "user")).toHaveLength(
          established ? 3 : 2,
        );
        expect(calls).toBe(2);
        expect(await readFile(attempts, "utf8")).toBe("attempt\n");
        await expect(readFile(finished)).rejects.toMatchObject({ code: "ENOENT" });
      } finally {
        next.dispose();
      }
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  },
  15_000,
);

it.each([
  '403: {"error":{"message":"The usage limit has been reached","type":"usage_limit"}}',
  "403: <html><body>Forbidden by gateway</body></html>",
])(
  "reports a failed provider call as Pi's explanation, once, and continues: %j",
  async (errorMessage) => {
    const root = await mkdtemp(join(tmpdir(), "carillon-failed-pi-"));
    const key = { interface_id: "discord", agent_id: "default", audience_id: "conversation" };
    const baseRuntime = await ModelRuntime.create({
      allowModelNetwork: false,
      refreshOnCreate: false,
      modelsPath: null,
    });
    let calls = 0;
    const runtime = new Proxy(baseRuntime, {
      get(target, property): unknown {
        if (property === "getAuth") return () => Promise.resolve({ auth: { apiKey: "test-only" } });
        if (property === "hasConfiguredAuth") return () => true;
        if (property === "streamSimple")
          return () => {
            calls += 1;
            const stream = createAssistantMessageEventStream();
            queueMicrotask(() => {
              if (calls === 1)
                stream.push({
                  type: "error",
                  reason: "error",
                  error: { ...assistantMessage, stopReason: "error", content: [], errorMessage },
                });
              else stream.push({ type: "done", reason: "stop", message: assistantMessage });
            });
            return stream;
          };
        const value: unknown = Reflect.get(target, property, target);
        return typeof value === "function" ? value.bind(target) : value;
      },
    });
    const open = async () =>
      (
        await createPiExecutionSession({
          resourceRoot: root,
          workspace: root,
          manager: openConversation(root, join(root, "sessions"), key),
          modelRuntime: runtime,
          model,
        })
      ).session;
    try {
      const session = await open();
      try {
        const failure: unknown = await promptSession(
          session,
          "use the provider",
          new AbortController().signal,
        ).catch((thrown: unknown) => thrown);
        // What of it may be shown is decided where it leaves the runtime.
        expect(failure).toBeInstanceOf(ExplainedFailure);
        expect(failure).toMatchObject({ message: errorMessage, status: 502 });
        expect(calls).toBe(1);
      } finally {
        session.dispose();
      }
      // The failure is recorded in the native session, which continues normally.
      const next = await open();
      try {
        await promptSession(next, "try again", new AbortController().signal);
        expect(calls).toBe(2);
        expect(next.messages.filter((message) => message.role === "user")).toHaveLength(2);
      } finally {
        next.dispose();
      }
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  },
);
