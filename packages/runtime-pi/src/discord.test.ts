import { BACKGROUND_CONTEXT as context } from "@earendil-works/chord/context";
import {
  createModels,
  fauxAssistantMessage,
  fauxProvider,
  fauxToolCall,
  type Message,
} from "@earendil-works/pi-ai";
import { createRegistry, Harness, MemoryStorage } from "@earendil-works/pi-durable";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, it } from "vitest";
import { createDiscordExtension } from "@carillon/discord-pi";
import { defaultExtensions, discordContext } from "./discord.ts";

const cleanups: (() => Promise<unknown>)[] = [];
afterEach(async () => {
  for (const cleanup of cleanups.splice(0).toReversed()) await cleanup();
});

/** The provider's operations endpoint, answering every operation alike. */
async function provider() {
  const operations: unknown[] = [];
  const authorizations: (string | undefined)[] = [];
  const server = createServer((request, response) => {
    authorizations.push(request.headers.authorization);
    let body = "";
    request.on("data", (part: Buffer) => (body += part.toString()));
    request.on("end", () => {
      operations.push(JSON.parse(body));
      response.setHeader("content-type", "application/json");
      response.end(JSON.stringify({ guild_id: "1", items: [], may_have_more: false }));
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  cleanups.push(() => new Promise((done) => server.close(done)));
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("expected a TCP listener");
  return { url: `http://127.0.0.1:${String(address.port)}`, operations, authorizations };
}

type Calls = Record<string, string | number | Record<string, string>>[];

/**
 * Runs one conversation from `origin` whose model enables the Discord tools
 * and then makes `calls`, returning what it saw and what the provider got.
 */
async function converse(origin: Record<string, unknown>, calls: Calls) {
  const root = await mkdtemp(join(tmpdir(), "carillon-discord-tools-"));
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  const token = "operator-test-token";
  const tokenFile = join(root, "operator.token");
  await writeFile(tokenFile, token);
  const discord = await provider();
  const context = {
    interface_id: "discord-private",
    agent_id: "assistant",
    native: { origin },
    connection: {
      url: discord.url,
      token_file: tokenFile,
      token_hash: createHash("sha256").update(token).digest("hex"),
    },
  };
  return { ...(await run(context, calls)), discord, token };
}

async function run(toolContext: unknown, calls: Calls) {
  const seen: (readonly Message[])[] = [];
  const model = fauxProvider();
  model.setResponses([
    ...[
      fauxToolCall("enable_discord", {}),
      ...calls.map((operation) => fauxToolCall("discord_messages", { operation })),
    ].map((call) => (request: { messages: readonly Message[] }) => {
      seen.push(request.messages);
      return fauxAssistantMessage(call, { stopReason: "toolUse" });
    }),
    (request) => {
      seen.push(request.messages);
      return fauxAssistantMessage("Done.");
    },
  ]);
  const models = createModels();
  models.setProvider(model.provider);
  const registry = createRegistry();
  registry.install(
    createDiscordExtension({
      guidance: "Discord guidance for the model.",
      context: () => Promise.resolve(toolContext),
    }),
  );
  const harness = await Harness.open(new MemoryStorage(), { models, registry }, context);
  cleanups.push(() => harness.close(context));
  const conversation = await harness.root(context, {
    agent: { model: { provider: model.provider.id, modelId: model.getModel().id } },
  });
  const settled = await (
    await conversation.submit({ type: "input", content: "use Discord" }, context)
  ).wait(context);
  return { settled, seen };
}

const offered = (messages: readonly Message[] = []) =>
  messages.flatMap((message) =>
    message.role === "system" ? (message.toolsAdded ?? []).map((tool) => tool.name) : [],
  );

it("loads the Discord tools on demand and runs them in the conversation's origin", async () => {
  const origin = {
    guild_id: "1",
    channel_id: "2",
    source_channel_id: "2",
    message_id: "3",
    user_id: "4",
  };
  const { settled, seen, discord, token } = await converse(origin, [
    { action: "messages.list", limit: 2 },
  ]);
  expect(settled.status).toBe("done");
  expect(offered(seen[0])).toEqual(["enable_discord"]);
  const enabled = seen[1]?.findLast((message) => message.role === "toolResult");
  const [connection, guidance, ...rest] = enabled?.role === "toolResult" ? enabled.content : [];
  expect(connection?.type === "text" && connection.text).toContain(
    '"origin":{"guild_id":"1","channel_id":"2"',
  );
  expect(guidance).toEqual({ type: "text", text: "Discord guidance for the model." });
  expect(rest).toEqual([]);
  expect(offered(seen[1])).toContain("discord_messages");
  expect(discord.operations).toEqual([
    { action: "server.info" },
    { action: "messages.list", limit: 2, channel_id: "2" },
  ]);
  expect(discord.authorizations).toEqual([`Bearer ${token}`, `Bearer ${token}`]);
});

it("works from a direct message and reaches a person's direct messages by their ID", async () => {
  const origin = {
    guild_id: null,
    channel_id: "9",
    source_channel_id: "9",
    message_id: "3",
    user_id: "4",
    parent_channel_id: null,
  };
  const { settled, discord } = await converse(origin, [
    { action: "messages.list", limit: 2 },
    { action: "messages.send", user_id: "4", content: "An update." },
  ]);
  expect(settled.status).toBe("done");
  expect(discord.operations).toEqual([
    { action: "server.info" },
    { action: "messages.list", limit: 2, channel_id: "9" },
    {
      action: "messages.send",
      user_id: "4",
      content: "An update.",
      sender: { agent_id: "assistant", channel_id: "9" },
    },
  ]);
});

it("says which agent sends, from which conversation, and never lets the model say it", async () => {
  const origin = {
    guild_id: "1",
    channel_id: "2",
    source_channel_id: "2",
    message_id: "3",
    user_id: "4",
  };
  const { settled, discord } = await converse(origin, [
    { action: "messages.send", user_id: "4", content: "Done in #home." },
    {
      action: "messages.send",
      channel_id: "5",
      content: "Claimed.",
      sender: { agent_id: "someone-else", channel_id: "6" },
    },
  ]);
  expect(settled.status).toBe("done");
  expect(discord.operations).toEqual([
    { action: "server.info" },
    {
      action: "messages.send",
      user_id: "4",
      content: "Done in #home.",
      sender: { agent_id: "assistant", channel_id: "2" },
    },
  ]);
});

async function installation(interfaces: Record<string, { program: string; url: string }>) {
  const root = await mkdtemp(join(tmpdir(), "carillon-discord-registry-"));
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, "credentials"));
  await writeFile(join(root, "credentials", "operator.token"), "operator-test-token");
  const hash = createHash("sha256").update("operator-test-token").digest("hex");
  return {
    root,
    operatorTokenHash: hash,
    interfaces: Object.entries(interfaces).map(([id, provider]) => ({
      id,
      token_hash: hash,
      ...provider,
    })),
  };
}

const toolResults = (messages: readonly Message[] = []) =>
  messages.flatMap((message) =>
    message.role === "toolResult"
      ? message.content.flatMap((part) => (part.type === "text" ? [part.text] : []))
      : [],
  );

it("reaches the Discord provider from another interface's conversation, with no origin", async () => {
  const [chat, discord] = [await provider(), await provider()];
  const config = await installation({
    chat: { program: "example", url: chat.url },
    "discord-private": { program: "discord", url: discord.url },
  });
  expect(defaultExtensions(config)).toContain("discord");
  const origin = { channel_id: "2", source_channel_id: "2", message_id: "3", user_id: "4" };
  const conversation = { interface_id: "chat", agent_id: "assistant", native: { origin } };
  const { settled, seen } = await run(discordContext(config, conversation), [
    { action: "messages.list", limit: 2 },
    { action: "messages.send", channel_id: "5", content: "From the chat page." },
  ]);
  expect(settled.status).toBe("done");
  expect(toolResults(seen[1])[0]).toContain('"origin":null');
  expect(toolResults(seen[2]).at(-1)).toContain(
    "This conversation has no Discord channel of its own. Supply channel_id",
  );
  expect(discord.operations).toEqual([
    { action: "server.info" },
    {
      action: "messages.send",
      channel_id: "5",
      content: "From the chat page.",
      sender: { agent_id: "assistant" },
    },
  ]);
  expect(chat.operations).toEqual([]);
});

it("keeps a Discord conversation on its own provider, and is no default without one", async () => {
  const [first, second] = [await provider(), await provider()];
  const config = await installation({
    "discord-private": { program: "discord", url: first.url },
    "discord-work": { program: "discord", url: second.url },
    chat: { program: "example", url: "http://127.0.0.1:1" },
  });
  const native = {
    origin: { channel_id: "2", source_channel_id: "2", message_id: "3", user_id: "4" },
  };
  const own = { interface_id: "discord-work", agent_id: "assistant", native };
  await run(discordContext(config, own), [{ action: "messages.list", limit: 2 }]);
  expect(first.operations).toEqual([]);
  expect(second.operations).toEqual([
    { action: "server.info" },
    { action: "messages.list", limit: 2, channel_id: "2" },
  ]);

  const elsewhere = { interface_id: "chat", agent_id: "assistant" };
  const { seen } = await run(discordContext(config, elsewhere), []);
  expect(toolResults(seen[1])[0]).toContain(
    "Discord is not connected here: this is not a Discord conversation",
  );
  expect(first.operations).toEqual([]);

  const withoutDiscord = { ...config, interfaces: config.interfaces.slice(2) };
  expect(defaultExtensions(withoutDiscord)).toEqual(["coding", "tool-search"]);
});
