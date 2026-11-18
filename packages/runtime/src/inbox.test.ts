import { TURN_REQUEST_LIMIT, type ControlAction } from "@carillon/protocol";
import { once } from "node:events";
import { request } from "node:http";
import { expect, it, vi } from "vitest";
import { ExplainedFailure } from "./failure.ts";
import { createRuntimeServer } from "./http.ts";
import type { InboxTurns, RuntimeOperations, TurnRequest } from "./operations.ts";

const others: Omit<RuntimeOperations, "turns"> = {
  activity: () => undefined,
  settings: () => Promise.reject(new Error("unused")),
  commands: () => Promise.resolve({ commands: [] }),
  restartReadiness: () => ({ state: "ready" }),
  close: () => Promise.resolve(),
};

async function serve(turns: InboxTurns) {
  const server = createRuntimeServer({
    runtime: { turns, ...others },
    token: "test-only",
    providerId: "runtime-durable",
    provider: "test",
    version: "9.9.9-test",
    features: [],
    agents: [],
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("expected a TCP listener");
  const call = async (path: string, body: unknown, token = "test-only") => {
    const response = await fetch(`http://127.0.0.1:${String(address.port)}${path}`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    const text = await response.text();
    return {
      status: response.status,
      body: text.length === 0 ? undefined : (JSON.parse(text) as unknown),
    };
  };
  const close = async () => {
    await new Promise((done) => server.close(done));
  };
  return { call, close, url: `http://127.0.0.1:${String(address.port)}` };
}

const turn = (input: string): TurnRequest => ({
  correlation_id: "turn-1",
  interface_id: "discord-private",
  agent_id: "default",
  channel_id: "300",
  audience_id: "conversation",
  input,
  resource_root: "/unused",
  attachment_root: "/unused",
});

it("answers 202 once a turn is kept, and passes a refusal through", async () => {
  vi.spyOn(console, "error").mockImplementation(() => undefined);
  const kept: string[] = [];
  let release: (() => void) | undefined;
  const inbox: InboxTurns = {
    submit: async (request) => {
      if (request.input === "refused")
        throw new ExplainedFailure(422, "That agent is not set up here.");
      if (request.input === "broken") throw new Error("state at /private/inbox is corrupt");
      await new Promise<void>((done) => {
        release = done;
      });
      kept.push(request.correlation_id);
    },
    control: () => Promise.reject(new Error("unused")),
  };
  const { call, close } = await serve(inbox);
  try {
    let answered = false;
    const answer = call("/v1/inbox", turn("hello")).finally(() => {
      answered = true;
    });
    await vi.waitFor(() => {
      expect(release).toBeDefined();
    });
    await new Promise((done) => setTimeout(done, 50));
    expect(answered).toBe(false);
    release?.();
    expect(await answer).toEqual({ status: 202, body: {} });
    expect(kept).toEqual(["turn-1"]);
    expect(await call("/v1/inbox", turn("refused"))).toEqual({
      status: 422,
      body: { error: "That agent is not set up here." },
    });
    // Core hands it over again later.
    expect(await call("/v1/inbox", turn("broken"))).toEqual({ status: 500, body: {} });
    expect(await call("/v1/inbox", { input: "no turn" })).toEqual({
      status: 400,
      body: { error: "invalid runtime turn request" },
    });
    expect((await call("/v1/inbox", turn("hello"), "wrong")).status).toBe(403);
  } finally {
    await close();
  }
});

it("takes a turn as large as core hands over and refuses a larger one with 413", async () => {
  let kept = 0;
  const { call, close } = await serve({
    submit: () => {
      kept += 1;
      return Promise.resolve();
    },
    control: () => Promise.resolve(),
  });
  const sized = (bytes: number) =>
    turn("x".repeat(bytes - Buffer.byteLength(JSON.stringify(turn("")))));
  try {
    expect(await call("/v1/inbox", sized(TURN_REQUEST_LIMIT))).toEqual({ status: 202, body: {} });
    expect(await call("/v1/inbox", sized(TURN_REQUEST_LIMIT + 1))).toEqual({
      status: 413,
      body: {
        error: "That message is too large for this agent's runtime, which takes about 2 MiB.",
      },
    });
    expect(kept).toBe(1);
  } finally {
    await close();
  }
});

it("answers a control or settings request past 1 MiB before the rest of it arrives", async () => {
  const { url, close } = await serve({
    submit: () => Promise.resolve(),
    control: () => Promise.resolve(),
  });
  try {
    for (const path of ["/v1/turns/turn-1/control", "/v1/settings"]) {
      const sent = request(`${url}${path}`, {
        method: "POST",
        headers: { authorization: "Bearer test-only", "content-type": "application/json" },
      });
      sent.on("error", () => undefined);
      sent.write(Buffer.alloc(1_048_577, " "));
      const answered: unknown[] = await once(sent, "response");
      expect(answered[0]).toMatchObject({ statusCode: 400 });
      sent.destroy();
    }
  } finally {
    await close();
  }
});

it("relays a person's control of a held turn to the runtime", async () => {
  const controlled: [string, string, ControlAction][] = [];
  const { call, close } = await serve({
    submit: () => Promise.resolve(),
    control: (turnId, interfaceId, action) => {
      if (action === "interrupt")
        return Promise.reject(new ExplainedFailure(409, "That message is already running."));
      controlled.push([turnId, interfaceId, action]);
      return Promise.resolve();
    },
  });
  try {
    const control = (action: string) => ({ interface_id: "discord-private", action });
    expect(await call("/v1/turns/turn%2F1/control", control("steer"))).toEqual({
      status: 204,
      body: undefined,
    });
    expect(await call("/v1/turns/turn-1/control", control("interrupt"))).toEqual({
      status: 409,
      body: { error: "That message is already running." },
    });
    for (const invalid of [control("pause"), { action: "stop" }])
      expect(await call("/v1/turns/turn-1/control", invalid)).toEqual({
        status: 400,
        body: { error: "invalid turn control" },
      });
    expect((await call("/v1/turns/turn-1/control", control("stop"), "wrong")).status).toBe(403);
    expect(controlled).toEqual([["turn/1", "discord-private", "steer"]]);
  } finally {
    await close();
  }
});
