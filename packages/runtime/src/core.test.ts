import { createServer } from "node:net";
import { once } from "node:events";
import { afterEach, expect, it } from "vitest";
import { createCoreClient } from "./core.ts";
import { startFakeCore, type FakeCore } from "./testing.ts";

const cores: FakeCore[] = [];
afterEach(async () => {
  await Promise.all(cores.splice(0).map((core) => core.close()));
});

async function core(port = 0) {
  const started = await startFakeCore(port);
  cores.push(started);
  return started;
}

const client = (url: string) =>
  createCoreClient({ url, providerId: "runtime-durable", token: "durable-only" });

it("settles a turn once and says how core took each outcome", async () => {
  const fake = await core();
  const settle = client(fake.url).settle;
  const answered = { outcome: "answered", output: "Done." } as const;
  expect(await settle("turn/1", answered)).toBe("recorded");
  expect(await settle("turn/1", answered)).toBe("recorded");
  expect(await settle("turn/1", { outcome: "stopped" })).toBe("superseded");
  fake.answer(404);
  expect(await settle("turn-2", { outcome: "steered" })).toBe("unknown");
  expect(fake.received[0]).toEqual({
    path: "/v1/runtime/turns/turn%2F1/outcome",
    provider: "runtime-durable",
    authorization: "Bearer durable-only",
    body: answered,
  });
});

it("waits out core's failures and restarts, but not its refusals", async () => {
  const fake = await core();
  const { settle } = client(fake.url);
  fake.answer(503, 500);
  expect(await settle("turn-1", { outcome: "stopped" })).toBe("recorded");
  expect(fake.received).toHaveLength(3);
  fake.answer(400);
  await expect(settle("turn-2", { outcome: "answered", output: " " })).rejects.toThrow(
    "core refused the outcome of turn turn-2: answered 400",
  );
  fake.answer(403);
  await expect(settle("turn-2", { outcome: "stopped" })).rejects.toThrow(/answered 403/u);
  // A core that is not listening yet is asked again until it is.
  const reserved = createServer().listen(0, "127.0.0.1");
  await once(reserved, "listening");
  const address = reserved.address();
  if (address === null || typeof address === "string") throw new Error("expected a TCP port");
  await new Promise((done) => reserved.close(done));
  const later = client(`http://127.0.0.1:${String(address.port)}`).settle("turn-3", {
    outcome: "failed",
  });
  await new Promise((done) => setTimeout(done, 300));
  const restarted = await core(address.port);
  expect(await later).toBe("recorded");
  expect(restarted.received.map((received) => received.path)).toEqual([
    "/v1/runtime/turns/turn-3/outcome",
  ]);
});

it("opens one turn per key and reads core's receipt", async () => {
  const fake = await core();
  const { open } = client(fake.url);
  const opening = {
    interface_id: "discord-private",
    agent_id: "default",
    audience_id: "conversation",
    key: "nightly",
  };
  const first = await open(opening);
  expect(first).toEqual({
    event_id: "runtime:runtime-durable:nightly",
    turn_id: "opened-1",
    state: "running",
    duplicate: false,
  });
  expect(await open(opening)).toEqual({ ...first, duplicate: true });
  fake.answer(409);
  await expect(open({ ...opening, key: "other" })).rejects.toThrow(
    "core opened no turn: answered 409",
  );
});
