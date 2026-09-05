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
import { createPiExecutionSession } from "./session.ts";

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
          session.extensionRunner.createContext(),
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
