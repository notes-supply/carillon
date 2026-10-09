import { createHash } from "node:crypto";
import { mkdtemp, writeFile, readFile, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { createServer } from "node:http";
import { once } from "node:events";
import { afterEach, expect, it, vi } from "vitest";
import { Type } from "typebox";
import { Check } from "typebox/value";
import { DiscordClient } from "./client.ts";
import { groups } from "./schemas.ts";

const cleanups: (() => Promise<void>)[] = [];
afterEach(async () => {
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});
async function fixture(status: number, responseBody: string, redirect = false) {
  const root = await mkdtemp(join(tmpdir(), "discord-client-"));
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  const tokenFile = join(root, "token");
  await writeFile(tokenFile, "secret-local-test");
  let requests = 0;
  const server = createServer((_request, response) => {
    requests++;
    response.statusCode = status;
    if (redirect) response.setHeader("location", "/stolen");
    response.end(responseBody);
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  cleanups.push(() => {
    server.closeAllConnections();
    return new Promise<void>((resolve) =>
      server.close(() => {
        resolve();
      }),
    );
  });
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("Missing test address");
  const context = {
    interface_id: "discord",
    native: {
      origin: {
        guild_id: "1",
        channel_id: "2",
        source_channel_id: "2",
        message_id: "3",
        user_id: "4",
      },
    },
    connection: {
      url: `http://127.0.0.1:${String(address.port)}`,
      token_file: tokenFile,
      token_hash: createHash("sha256").update("secret-local-test").digest("hex"),
    },
  };
  return { client: new DiscordClient(context), context, requests: () => requests };
}

it("keeps request validation aligned with provider-owned wire fixtures", async () => {
  const fixtureSchema = Type.Array(
    Type.Object({ operation: Type.Record(Type.String(), Type.Unknown()), mutates: Type.Boolean() }),
  );
  const fixtures: unknown = JSON.parse(
    await readFile(new URL("../fixtures/operations.json", import.meta.url), "utf8"),
  );
  expect(Check(fixtureSchema, fixtures)).toBe(true);
  if (!Check(fixtureSchema, fixtures)) throw new Error("Invalid wire fixtures");
  for (const fixture of fixtures)
    expect(groups.filter((group) => Check(group.schema, fixture.operation))).toHaveLength(1);
  // An unanswered write is uncertain and an unanswered read only unavailable,
  // as the provider classifies the same operations.
  const unanswered = await fixture(502, "not a provider answer");
  for (const { operation, mutates } of fixtures)
    await expect(
      unanswered.client.call(operation),
      String(operation["action"]),
    ).rejects.toMatchObject({ outcome: mutates ? "uncertain" : "unavailable" });
  expect(unanswered.requests()).toBe(fixtures.length);
  const messages = groups.find((group) => group.name === "discord_messages");
  if (messages === undefined) throw new Error("Missing message schema");
  expect(Check(messages.schema, { action: "messages.get", channel_id: 2, message_id: "3" })).toBe(
    false,
  );
  expect(
    Check(messages.schema, {
      action: "messages.send",
      content: "hello",
      url: "https://elsewhere.invalid",
    }),
  ).toBe(false);
});

it("preserves typed failure outcomes without leaking raw response bodies or replaying", async () => {
  for (const outcome of ["rejected", "unavailable", "uncertain"] as const) {
    const test = await fixture(
      400,
      JSON.stringify({ outcome, message: "Useful provider explanation" }),
    );
    await expect(
      test.client.call({ action: "messages.send", channel_id: "2", content: "hello" }),
    ).rejects.toMatchObject({ outcome, message: "Useful provider explanation" });
    expect(test.requests()).toBe(1);
  }
  const invalid = await fixture(502, "secret-local-test arbitrary upstream stacktrace");
  await expect(
    invalid.client.call({ action: "messages.send", content: "hello" }),
  ).rejects.toMatchObject({ outcome: "uncertain" });
  await expect(invalid.client.call({ action: "messages.list" })).rejects.toMatchObject({
    outcome: "unavailable",
  });
  expect(invalid.requests()).toBe(2);
});

it("never follows credential-bearing redirects and refuses bad local credentials before HTTP", async () => {
  const redirected = await fixture(307, "", true);
  await expect(
    redirected.client.call({ action: "messages.send", content: "hello" }),
  ).rejects.toMatchObject({ outcome: "uncertain" });
  expect(redirected.requests()).toBe(1);
  await writeFile(redirected.context.connection.token_file, "wrong");
  await expect(redirected.client.call({ action: "server.info" })).rejects.toMatchObject({
    outcome: "unavailable",
  });
  expect(redirected.requests()).toBe(1);
  await expect(new DiscordClient(undefined).call({ action: "server.info" })).rejects.toMatchObject({
    outcome: "unavailable",
  });
});

it("bounds response bytes and propagates cancellation without retry", async () => {
  const big = await fixture(200, JSON.stringify({ content: "x".repeat(2 * 1024 * 1024) }));
  await expect(big.client.call({ action: "messages.list" })).rejects.toMatchObject({
    outcome: "unavailable",
  });
  const controller = new AbortController();
  controller.abort();
  await expect(
    big.client.call({ action: "messages.send", content: "hello" }, controller.signal),
  ).rejects.toMatchObject({ name: "AbortError" });
  expect(big.requests()).toBe(1);
});

it("sends the operator credential only to an HTTP loopback socket", async () => {
  const { context } = await fixture(200, "{}");
  const sent = vi.spyOn(globalThis, "fetch");
  try {
    for (const url of [
      "https://attacker.example",
      "http://attacker.example:8080",
      "http://192.0.2.1:8080",
      "http://localhost:8080",
      "https://127.0.0.1:8080",
      "http://user@127.0.0.1:8080",
      "http://127.0.0.1:8080/elsewhere",
      "http://127.0.0.1",
      "http://[2001:db8::1]:8080",
      "not a url",
    ]) {
      const client = new DiscordClient({ ...context, connection: { ...context.connection, url } });
      await expect(client.call({ action: "channel.info" }), url).rejects.toThrow(
        "Invalid configured interface endpoint.",
      );
    }
    expect(sent).not.toHaveBeenCalled();
  } finally {
    sent.mockRestore();
  }
});
