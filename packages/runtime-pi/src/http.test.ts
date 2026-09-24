import { once } from "node:events";
import { afterEach, expect, it, vi } from "vitest";
import { ExplainedFailure } from "./failure.ts";
import { createRuntimeServer } from "./http.ts";
import type { RuntimeOperations, TurnRequest } from "./operations.ts";

const key = `sk-proj-${"Ab1".repeat(20)}`;

const runtime: RuntimeOperations = {
  executeTurn(request: TurnRequest) {
    if (request.input === "explained")
      return Promise.reject(
        new ExplainedFailure(502, "Codex error: The usage limit has been reached", {
          cause: `Codex error: The usage limit has been reached\n{"echo":"${key}"}`,
        }),
      );
    return Promise.reject(new Error(`extension state at /private/state rejected ${key}`));
  },
  modelState: () => Promise.reject(new Error("model catalogue file is corrupt")),
  setModel: () => Promise.reject(new ExplainedFailure(409, "The model settings changed.")),
  close: () => Promise.resolve(),
};

afterEach(() => {
  vi.restoreAllMocks();
});

async function serve() {
  const server = createRuntimeServer({
    runtime,
    token: "test-only",
    providerId: "runtime-pi",
    provider: "deterministic-local-substitute",
    workspace: "/unused",
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("expected a TCP listener");
  const { port } = address;
  const call = async (method: string, path: string, body?: unknown) => {
    const response = await fetch(`http://127.0.0.1:${String(port)}${path}`, {
      method,
      headers: { authorization: "Bearer test-only", "content-type": "application/json" },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const answer: unknown = await response.json();
    return { status: response.status, body: answer };
  };
  return { call, close: () => new Promise((done) => server.close(done)) };
}

const turn = (input: string) => ({
  correlation_id: "turn-1",
  interface_id: "discord-private",
  agent_id: "default",
  audience_id: "conversation",
  input,
  resource_root: "/unused",
});

it("sends callers only explanations and logs redacted diagnostics", async () => {
  const log = vi.spyOn(console, "error").mockImplementation(() => undefined);
  const { call, close } = await serve();
  try {
    expect(await call("POST", "/v1/turns", turn("explained"))).toEqual({
      status: 502,
      body: { error: "Codex error: The usage limit has been reached" },
    });
    expect(await call("POST", "/v1/turns", turn("internal"))).toEqual({ status: 500, body: {} });
    expect(await call("GET", "/v1/model?interface_id=discord-private&agent_id=default")).toEqual({
      status: 500,
      body: {},
    });
    expect(
      await call("PUT", "/v1/model", {
        interface_id: "discord-private",
        agent_id: "default",
        scope: "agent",
        selection: null,
      }),
    ).toEqual({ status: 409, body: { error: "The model settings changed." } });
  } finally {
    await close();
  }
  const logged = log.mock.calls.map((args) => args.join(" "));
  // Rejections phrased for people are not failures of the runtime.
  expect(logged).toHaveLength(3);
  expect(logged[0]).toMatch(
    /^POST \/v1\/turns failed: Codex error: .*\{"echo":"\[redacted\]"\}$/su,
  );
  expect(logged[1]).toMatch(
    /^POST \/v1\/turns failed: Error: extension state at \/private\/state rejected \[redacted\]\n\s+at /u,
  );
  expect(logged[2]).toMatch(
    /^GET \/v1\/model\?\S+ failed: Error: model catalogue file is corrupt\n/u,
  );
  expect(logged.join("\n")).not.toContain(key);
});
