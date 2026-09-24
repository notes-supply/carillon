import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, it, vi } from "vitest";
import { ExplainedFailure } from "./failure.ts";
import { createRuntimeServer } from "./http.ts";
import { ModelSelections } from "./model.ts";
import type { RuntimeOperations } from "./operations.ts";
import { DETERMINISTIC_DEFAULT, DETERMINISTIC_MODELS } from "./testing.ts";

const key = `sk-proj-${"Ab1".repeat(20)}`;

afterEach(() => {
  vi.restoreAllMocks();
});

/** A runtime whose turns fail with the input as the provider's explanation, and whose model changes use real selection rules. */
async function serve() {
  const state = await mkdtemp(join(tmpdir(), "carillon-http-"));
  const selections = await ModelSelections.open(join(state, "model-selection.json"));
  const runtime: RuntimeOperations = {
    executeTurn: (request) =>
      Promise.reject(
        request.input === "internal"
          ? new Error(`extension state at /private/state rejected ${key}`)
          : new ExplainedFailure(502, request.input),
      ),
    modelState: () => Promise.reject(new Error("model catalogue file is corrupt")),
    setModel: (update) => selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, update),
    close: () => Promise.resolve(),
  };
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
  const call = async (method: string, path: string, body?: unknown) => {
    const response = await fetch(`http://127.0.0.1:${String(address.port)}${path}`, {
      method,
      headers: { authorization: "Bearer test-only", "content-type": "application/json" },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const answer: unknown = await response.json();
    return { status: response.status, body: answer };
  };
  const close = async () => {
    await new Promise((done) => server.close(done));
    await rm(state, { recursive: true, force: true });
  };
  return { call, close };
}

const turn = (input: string) => ({
  correlation_id: "turn-1",
  interface_id: "discord-private",
  agent_id: "default",
  audience_id: "conversation",
  input,
  resource_root: "/unused",
});

const option = (id: string) => ({
  interface_id: "discord-private",
  agent_id: "default",
  scope: "agent",
  selection: { provider: "local", model: "reasoner", options: { [id]: "high" } },
});

it("sends callers only the displayable part of explanations and logs redacted diagnostics", async () => {
  const log = vi.spyOn(console, "error").mockImplementation(() => undefined);
  const { call, close } = await serve();
  const withheld = [
    "Authentication failed: password=short-secret",
    "Request failed: https://alice:short-secret@example.invalid",
    "Gateway error: [click here](https://example.invalid/?token=short-secret)",
    `403 <html><body>Gateway refused ${key}</body></html>`,
  ];
  try {
    expect(
      await call(
        "POST",
        "/v1/turns",
        turn('403: {"error":{"message":"The usage limit has been reached","type":"usage_limit"}}'),
      ),
    ).toEqual({ status: 502, body: { error: "403: The usage limit has been reached" } });
    for (const text of withheld)
      expect(await call("POST", "/v1/turns", turn(text))).toEqual({ status: 502, body: {} });
    expect(await call("POST", "/v1/turns", turn("internal"))).toEqual({ status: 500, body: {} });
    expect(
      await call(
        "GET",
        "/v1/model?interface_id=discord-private&agent_id=default&token=short-secret",
      ),
    ).toEqual({ status: 500, body: {} });
    // Refusals quote the request, and pass the same boundary.
    expect(await call("PUT", "/v1/model", option("speed"))).toEqual({
      status: 422,
      body: { error: "Local reasoner has no speed setting." },
    });
    expect(await call("PUT", "/v1/model", option("password=short-secret"))).toEqual({
      status: 422,
      body: {},
    });
  } finally {
    await close();
  }
  const logged = log.mock.calls.map((args) => args.join(" "));
  // Failures are logged; refusals of a caller's request are answers.
  expect(logged).toHaveLength(7);
  expect(logged[0]).toMatch(/^POST \/v1\/turns failed: Error: 403: \{"error":/u);
  expect(logged[1]).toMatch(/^POST \/v1\/turns failed: Error: .*password=\[redacted\]/u);
  expect(logged[2]).toMatch(/^POST \/v1\/turns failed: Error: .*https:\/\/example\.invalid\n/u);
  expect(logged[3]).toMatch(/^POST \/v1\/turns failed: Error: .*example\.invalid\/\?\[redacted\]/u);
  expect(logged[5]).toMatch(
    /^POST \/v1\/turns failed: Error: extension state at \/private\/state rejected \[redacted\]\n\s+at /u,
  );
  expect(logged[6]).toMatch(/^GET \/v1\/model failed: Error: model catalogue file is corrupt\n/u);
  for (const secret of [key, "short-secret"]) expect(logged.join("\n")).not.toContain(secret);
});
