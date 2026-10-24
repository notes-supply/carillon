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
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, it } from "vitest";
import { createDiscordExtension } from "@carillon/discord-pi";

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

it("loads the Discord tools on demand and runs them in the conversation's origin", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-discord-tools-"));
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  const token = "operator-test-token";
  const tokenFile = join(root, "operator.token");
  await writeFile(tokenFile, token);
  const discord = await provider();
  const origin = {
    guild_id: "1",
    channel_id: "2",
    source_channel_id: "2",
    message_id: "3",
    user_id: "4",
  };
  const seen: (readonly Message[])[] = [];
  const model = fauxProvider();
  model.setResponses([
    (request) => {
      seen.push(request.messages);
      return fauxAssistantMessage(fauxToolCall("enable_discord", {}), { stopReason: "toolUse" });
    },
    (request) => {
      seen.push(request.messages);
      return fauxAssistantMessage(
        fauxToolCall("discord_messages", { operation: { action: "messages.list", limit: 2 } }),
        { stopReason: "toolUse" },
      );
    },
    (request) => {
      seen.push(request.messages);
      return fauxAssistantMessage("Listed.");
    },
  ]);
  const models = createModels();
  models.setProvider(model.provider);
  const registry = createRegistry();
  registry.install(
    createDiscordExtension({
      guidance: "Discord guidance for the model.",
      context: () =>
        Promise.resolve({
          interface_id: "discord-private",
          native: { origin },
          connection: {
            url: discord.url,
            token_file: tokenFile,
            token_hash: createHash("sha256").update(token).digest("hex"),
          },
        }),
    }),
  );
  const harness = await Harness.open(new MemoryStorage(), { models, registry }, context);
  cleanups.push(() => harness.close(context));
  const root_ = await harness.root(context, {
    agent: { model: { provider: model.provider.id, modelId: model.getModel().id } },
  });
  const offered = (messages: readonly Message[] = []) =>
    messages.flatMap((message) =>
      message.role === "system" ? (message.toolsAdded ?? []).map((tool) => tool.name) : [],
    );
  const settled = await (
    await root_.submit({ type: "input", content: "list the channel" }, context)
  ).wait(context);
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
