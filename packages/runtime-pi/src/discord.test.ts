import { createHash } from "node:crypto";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { once } from "node:events";
import { ModelRuntime, SessionManager } from "@earendil-works/pi-coding-agent";
import {
  createAssistantMessageEventStream,
  getCurrentTools,
  type AssistantMessage,
  type Context,
  type Model,
  type JsonObject,
} from "@earendil-works/pi-ai";
import { expect, it } from "vitest";
import { createPiExecutionSession, promptSession } from "./session.ts";

it("runs the real Discord extension with private per-turn context and no duplicate default replies", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-discord-native-"));
  const workspace = join(root, "workspace");
  const resources = join(root, "resources");
  await mkdir(workspace);
  await mkdir(join(resources, "extensions"), { recursive: true });
  await mkdir(join(resources, "skills/discord"), { recursive: true });
  await writeFile(join(resources, "SYSTEM.md"), "You are the installation's assistant.");
  await writeFile(
    join(resources, "extensions/discord.ts"),
    `export { default } from ${JSON.stringify(new URL("../../discord-pi/src/index.ts", import.meta.url).pathname)};`,
  );
  await writeFile(
    join(resources, "skills/discord/SKILL.md"),
    "---\nname: discord\ndescription: Use Discord tools on demand.\n---\nCall enable_discord first.",
  );
  const token = "private-local-operator-test";
  const tokenFile = join(root, "operator.token");
  await writeFile(tokenFile, token);
  const operations: unknown[] = [];
  const headers: (string | undefined)[] = [];
  const server = createServer((request, response) => {
    headers.push(request.headers.authorization);
    const parts: Buffer[] = [];
    request.on("data", (part: Buffer) => {
      parts.push(part);
    });
    request.on("end", () => {
      operations.push(JSON.parse(Buffer.concat(parts).toString("utf8")) as unknown);
      response.setHeader("content-type", "application/json");
      response.end(JSON.stringify({ guild_id: "1", id: "9", items: [], may_have_more: false }));
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("No test address");
  const connection = {
    url: `http://127.0.0.1:${String(address.port)}`,
    token_file: tokenFile,
    token_hash: createHash("sha256").update(token).digest("hex"),
  };
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
  const calls: ({ name: string; arguments: JsonObject } | undefined)[] = [
    { name: "enable_discord", arguments: {} },
    { name: "discord_messages", arguments: { operation: { action: "messages.list", limit: 2 } } },
    {
      name: "discord_messages",
      arguments: { operation: { action: "messages.send", content: "summary" } },
    },
    {
      name: "discord_messages",
      arguments: { operation: { action: "messages.pin", message_id: "9", pinned: true } },
    },
    undefined,
    {
      name: "discord_messages",
      arguments: { operation: { action: "messages.send", content: "next turn" } },
    },
  ];
  let step = 0;
  const requests: Context[] = [];
  const runtime = new Proxy(base, {
    get(target, property): unknown {
      if (property === "getAuth") return () => Promise.resolve({ auth: { apiKey: "test" } });
      if (property === "hasConfiguredAuth") return () => true;
      if (property === "streamSimple")
        return (_model: unknown, context: Context) => {
          requests.push(structuredClone(context));
          const call = calls[step++];
          const message: AssistantMessage = {
            role: "assistant",
            api: model.api,
            provider: model.provider,
            model: model.id,
            content:
              call === undefined
                ? [{ type: "text", text: "Posted and pinned." }]
                : [{ type: "toolCall", id: `call-${String(step)}`, ...call }],
            stopReason: call === undefined ? "stop" : "toolUse",
            timestamp: Date.now(),
            usage: {
              input: 1,
              output: 0,
              cacheRead: 0,
              cacheWrite: 0,
              totalTokens: 1,
              cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
            },
          };
          const stream = createAssistantMessageEventStream();
          queueMicrotask(() => {
            stream.push({ type: "start", partial: message });
            stream.push({ type: "done", reason: call === undefined ? "stop" : "toolUse", message });
          });
          return stream;
        };
      const value: unknown = Reflect.get(target, property, target);
      return typeof value === "function" ? value.bind(target) : value;
    },
  });
  const create = (manager: SessionManager, channel: string) =>
    createPiExecutionSession({
      resourceRoot: resources,
      workspace,
      manager,
      modelRuntime: runtime,
      model,
      extensionContext: {
        interface_id: "discord",
        connection,
        native: {
          origin: {
            guild_id: "1",
            channel_id: channel,
            source_channel_id: "4",
            parent_channel_id: "4",
            message_id: "5",
            user_id: "6",
          },
        },
      },
    });
  try {
    const { session, extensionsResult } = await create(
      SessionManager.create(workspace, join(root, "sessions")),
      "2",
    );
    expect(extensionsResult.errors).toEqual([]);
    let file: string | undefined;
    try {
      await promptSession(session, "Read, send and pin a summary", new AbortController().signal);
      file = session.sessionFile;
      expect(session.messages.filter((m) => m.role === "toolResult").every((m) => !m.isError)).toBe(
        true,
      );
      expect(getCurrentTools(requests[0]?.messages ?? []).map((t) => t.name)).not.toContain(
        "discord_messages",
      );
      expect(getCurrentTools(requests[1]?.messages ?? []).map((t) => t.name)).toContain(
        "discord_messages",
      );
      expect(operations).toEqual([
        { action: "server.info" },
        { action: "messages.list", limit: 2, channel_id: "2" },
        { action: "messages.send", content: "summary", channel_id: "2" },
        { action: "messages.pin", message_id: "9", pinned: true, channel_id: "2" },
      ]);
    } finally {
      session.dispose();
    }
    if (file === undefined) throw new Error("Native session missing");
    const reopened = await create(SessionManager.open(file), "3");
    try {
      await promptSession(reopened.session, "Next", new AbortController().signal);
      expect(operations.at(-1)).toEqual({
        action: "messages.send",
        content: "next turn",
        channel_id: "3",
      });
      expect(JSON.stringify(reopened.session.messages)).not.toContain(token);
    } finally {
      reopened.session.dispose();
    }
    expect(headers.every((h) => h === `Bearer ${token}`)).toBe(true);
    expect(JSON.stringify(requests)).not.toContain(token);
    expect(JSON.stringify(requests)).toContain("installation's assistant");
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve) =>
      server.close(() => {
        resolve();
      }),
    );
    await rm(root, { recursive: true, force: true });
  }
});
