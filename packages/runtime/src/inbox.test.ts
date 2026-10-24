import type { ControlAction } from "@carillon/protocol";
import { once } from "node:events";
import { afterEach, expect, it, vi } from "vitest";
import { ExplainedFailure } from "./failure.ts";
import { createRuntimeServer } from "./http.ts";
import type { InboxTurns, RuntimeOperations, TurnHandling, TurnRequest } from "./operations.ts";

afterEach(() => {
  vi.restoreAllMocks();
});

const others: Omit<RuntimeOperations, "turns"> = {
  activity: () => undefined,
  modelState: () => Promise.reject(new Error("unused")),
  setModel: () => Promise.reject(new Error("unused")),
  commands: () => Promise.resolve({ commands: [] }),
  restartReadiness: () => ({ state: "ready" }),
  close: () => Promise.resolve(),
};

async function serve(turns: TurnHandling, durable = turns.mode === "inbox") {
  const server = createRuntimeServer({
    runtime: { turns, ...others },
    token: "test-only",
    providerId: "runtime-durable",
    durable,
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
  return { call, close };
}

const turn = (input: string): TurnRequest => ({
  correlation_id: "turn-1",
  interface_id: "discord-private",
  agent_id: "default",
  audience_id: "conversation",
  input,
  resource_root: "/unused",
  attachment_root: "/unused",
});

it("serves each turn contract only in its own mode", async () => {
  const answering = await serve({
    mode: "answer",
    execute: (request) =>
      Promise.resolve({ correlation_id: request.correlation_id, output: "Hi." }),
  });
  try {
    expect(await answering.call("/v1/turns", turn("hello"))).toEqual({
      status: 200,
      body: { correlation_id: "turn-1", output: "Hi." },
    });
    expect((await answering.call("/v1/inbox", turn("hello"))).status).toBe(404);
    const control = { interface_id: "discord-private", action: "stop" };
    expect((await answering.call("/v1/turns/turn-1/control", control)).status).toBe(404);
  } finally {
    await answering.close();
  }
  const holding = await serve({ mode: "inbox", submit: () => Promise.resolve() });
  try {
    expect((await holding.call("/v1/turns", turn("hello"))).status).toBe(404);
    // Without controls of its own, the runtime takes none.
    const control = { interface_id: "discord-private", action: "stop" };
    expect((await holding.call("/v1/turns/turn-1/control", control)).status).toBe(404);
  } finally {
    await holding.close();
  }
});

it("answers 202 once a turn is kept, and passes a refusal through", async () => {
  vi.spyOn(console, "error").mockImplementation(() => undefined);
  const kept: string[] = [];
  let release: (() => void) | undefined;
  const inbox: InboxTurns = {
    mode: "inbox",
    submit: async (request) => {
      if (request.input === "refused")
        throw new ExplainedFailure(422, "That agent is not set up here.");
      if (request.input === "broken") throw new Error("state at /private/inbox is corrupt");
      await new Promise<void>((done) => {
        release = done;
      });
      kept.push(request.correlation_id);
    },
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

it("relays a person's control of a held turn to the runtime", async () => {
  const controlled: [string, string, ControlAction][] = [];
  const { call, close } = await serve({
    mode: "inbox",
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

it("refuses to serve a turn contract its registry entry does not declare", async () => {
  await expect(serve({ mode: "inbox", submit: () => Promise.resolve() }, false)).rejects.toThrow(
    "a runtime that holds its turns needs durable = true in its carillon.toml entry",
  );
  const execute = () => Promise.reject(new Error("unused"));
  await expect(serve({ mode: "answer", execute }, true)).rejects.toThrow(
    "a runtime that answers while core waits must not be durable in carillon.toml",
  );
});
