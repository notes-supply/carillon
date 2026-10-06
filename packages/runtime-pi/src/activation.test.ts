import { ModelRuntime, SessionManager } from "@earendil-works/pi-coding-agent";
import {
  createAssistantMessageEventStream,
  getCurrentTools,
  type AssistantMessage,
  type Context,
  type Model,
} from "@earendil-works/pi-ai";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, it } from "vitest";
import { createPiExecutionSession, promptSession } from "./session.ts";

it("activates tools within a native turn and restores only that conversation's loadout", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-activation-"));
  const workspace = join(root, "workspace");
  const resources = join(root, "resources");
  await mkdir(workspace);
  await mkdir(join(resources, "extensions"), { recursive: true });
  const activation = new URL("../../discord-pi/src/activation.ts", import.meta.url).pathname;
  await writeFile(
    join(resources, "extensions", "discord.ts"),
    `
import { registerDiscordTools } from ${JSON.stringify(activation)};
export default function(pi) {
  registerDiscordTools(pi, [{
    name: "discord_probe", label: "Probe", description: "Harmless local probe",
    parameters: { type: "object", properties: {} },
    async execute() { return { content: [{type: "text", text: "probe worked"}], details: {} }; }
  }], async () => ({ ready: true }));
}
`,
  );
  const model: Model<"openai-responses"> = {
    id: "test",
    name: "Test",
    api: "openai-responses",
    provider: "openai",
    baseUrl: "https://example.invalid",
    reasoning: false,
    input: ["text"],
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
    contextWindow: 16000,
    maxTokens: 2000,
  };
  const base = await ModelRuntime.create({
    allowModelNetwork: false,
    refreshOnCreate: false,
    modelsPath: null,
  });
  const requests: string[][] = [];
  let step = 0;
  const runtime = new Proxy(base, {
    get(target, property): unknown {
      if (property === "getAuth") return () => Promise.resolve({ auth: { apiKey: "test" } });
      if (property === "hasConfiguredAuth") return () => true;
      if (property === "streamSimple")
        return (_model: unknown, context: Context) => {
          requests.push(getCurrentTools(context.messages).map((tool) => tool.name));
          const tool = ["enable_discord", "discord_probe"][step++];
          const message: AssistantMessage = {
            role: "assistant",
            api: model.api,
            provider: model.provider,
            model: model.id,
            content:
              tool !== undefined
                ? [{ type: "toolCall", id: `call-${String(step)}`, name: tool, arguments: {} }]
                : [{ type: "text", text: "finished" }],
            stopReason: tool !== undefined ? "toolUse" : "stop",
            timestamp: Date.now(),
            usage: {
              input: 1,
              output: 1,
              cacheRead: 0,
              cacheWrite: 0,
              totalTokens: 2,
              cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
            },
          };
          const stream = createAssistantMessageEventStream();
          queueMicrotask(() => {
            stream.push({ type: "start", partial: message });
            stream.push({ type: "done", reason: tool !== undefined ? "toolUse" : "stop", message });
          });
          return stream;
        };
      const value: unknown = Reflect.get(target, property, target);
      return typeof value === "function" ? value.bind(target) : value;
    },
  });
  const create = (manager: SessionManager) =>
    createPiExecutionSession({
      resourceRoot: resources,
      workspace,
      manager,
      modelRuntime: runtime,
      model,
    });
  try {
    const { session, extensionsResult } = await create(
      SessionManager.create(workspace, join(root, "sessions")),
    );
    expect(extensionsResult.errors).toEqual([]);
    let file: string | undefined;
    try {
      expect(session.getActiveToolNames()).not.toContain("discord_probe");
      await promptSession(session, "Use Discord", new AbortController().signal);
      file = session.sessionFile;
      expect(requests).toHaveLength(3);
      expect(requests[0]).toContain("enable_discord");
      expect(requests[0]).not.toContain("discord_probe");
      expect(requests[1]).toContain("discord_probe");
      expect(requests[1]).toEqual(expect.arrayContaining(["read", "bash", "write", "edit"]));
      expect(session.messages).toContainEqual(
        expect.objectContaining({ role: "toolResult", toolName: "discord_probe", isError: false }),
      );
    } finally {
      session.dispose();
    }
    if (file === undefined) throw new Error("Native session not persisted");
    const reopened = await create(SessionManager.open(file));
    try {
      expect(reopened.session.getActiveToolNames()).toContain("discord_probe");
      await promptSession(reopened.session, "Still available?", new AbortController().signal);
      expect(requests[3]).toContain("discord_probe");
    } finally {
      reopened.session.dispose();
    }
    const fresh = await create(SessionManager.create(workspace, join(root, "other")));
    try {
      expect(fresh.session.getActiveToolNames()).not.toContain("discord_probe");
    } finally {
      fresh.session.dispose();
    }
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
