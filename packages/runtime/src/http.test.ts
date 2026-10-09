import type { TurnActivity } from "@carillon/protocol";
import { once } from "node:events";
import { afterEach, expect, it, vi } from "vitest";
import { ExplainedFailure, RuntimeClosed, withReport } from "./failure.ts";
import { createRuntimeServer } from "./http.ts";
import type { RuntimeOperations } from "./operations.ts";

const key = `sk-proj-${"Ab1".repeat(20)}`;

afterEach(() => {
  vi.restoreAllMocks();
});

/** A runtime whose turns fail with the input as the provider's explanation, and whose model changes are refused quoting the requested option. */
async function serve() {
  const runtime: RuntimeOperations = {
    executeTurn: (request) =>
      Promise.reject(
        request.input === "internal"
          ? new Error(`extension state at /private/state rejected ${key}`)
          : request.input === "closing"
            ? new RuntimeClosed("runtime is closed")
            : request.input === "reported internal"
              ? reported(new Error(`state at /private/state rejected ${key}`))
              : request.input === "reported value"
                ? reportedValue(`the session's state rejected ${key}`)
                : request.input === "reported closing"
                  ? reported(new RuntimeClosed("runtime is closed"))
                  : request.input.startsWith("reported ")
                    ? reported(new ExplainedFailure(502, request.input.slice(9)))
                    : new ExplainedFailure(502, request.input),
      ),
    activity: (correlationId, interfaceId) =>
      correlationId === "turn/1" && interfaceId === "discord-private"
        ? { phase: "tools", running: [], recent: [{ tool: "read", ok: true }], tool_calls: 1 }
        : undefined,
    modelState: () => Promise.reject(new Error("model catalogue file is corrupt")),
    setModel: (update) =>
      Promise.reject(
        new ExplainedFailure(
          422,
          `Local reasoner has no ${Object.keys(update.selection?.options ?? {}).join(", ")} setting.`,
        ),
      ),
    commands: (query) =>
      Promise.resolve({
        commands:
          query.audience_id === undefined ? [] : [{ name: "compact", hint: "<instructions>" }],
      }),
    restartReadiness: () => ({ state: "unready", reason: "1 turn(s) still executing" }),
    close: () => Promise.resolve(),
  };
  const server = createRuntimeServer({
    runtime,
    token: "test-only",
    providerId: "runtime-pi",
    provider: "deterministic-local-substitute",
    version: "9.9.9-test",
    features: [],
    workspace: "/unused",
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("expected a TCP listener");
  const call = async (method: string, path: string, body?: unknown, token = "test-only") => {
    const response = await fetch(`http://127.0.0.1:${String(address.port)}${path}`, {
      method,
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const answer: unknown = await response.json();
    return { status: response.status, body: answer };
  };
  const close = async () => {
    await new Promise((done) => server.close(done));
  };
  return { call, close };
}

const REPORT: TurnActivity = {
  phase: "tools",
  running: [],
  recent: [{ tool: "bash", detail: "make", ok: false, duration_ms: 1200 }],
  tool_calls: 1,
};

/** `failure`, after a turn left its report with it. */
function reported(failure: Error): Error {
  withReport(failure, REPORT);
  return failure;
}

/** What a runtime throws when a turn that left its report ended with `value`, which is no error. */
function reportedValue(value: string): Error {
  const failure = withReport(value, REPORT);
  if (!(failure instanceof Error)) throw new TypeError("the report was not kept with an error");
  return failure;
}

const turn = (input: string) => ({
  correlation_id: "turn-1",
  interface_id: "discord-private",
  agent_id: "default",
  audience_id: "conversation",
  input,
  resource_root: "/unused",
  attachment_root: "/unused",
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
    // A runtime that closes under a request gives no answer, which is not a failure.
    expect(await call("POST", "/v1/turns", turn("closing"))).toEqual({ status: 503, body: {} });
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
    // The command catalogue is the runtime's, for the queried conversation.
    expect(
      await call("GET", "/v1/commands?interface_id=discord-private&agent_id=default&audience_id=c"),
    ).toEqual({ status: 200, body: { commands: [{ name: "compact", hint: "<instructions>" }] } });
    expect(await call("GET", "/v1/commands?agent_id=default")).toEqual({
      status: 400,
      body: { error: "invalid command query" },
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

it("reports the build version its runtime was given, not a shared package's", async () => {
  const { call, close } = await serve();
  try {
    expect(await call("GET", "/health")).toMatchObject({
      status: 200,
      body: { id: "runtime-pi", role: "runtime", version: "9.9.9-test", status: "ready" },
    });
  } finally {
    await close();
  }
});

it("tells core, and only core, whether a restart would lose what the runtime holds", async () => {
  const { call, close } = await serve();
  try {
    expect(await call("GET", "/v1/restart")).toEqual({
      status: 200,
      body: { state: "unready", reason: "1 turn(s) still executing" },
    });
    expect((await call("GET", "/v1/restart", undefined, "wrong")).status).toBe(403);
  } finally {
    await close();
  }
});

it("describes an executing turn only to its own interface, after authentication", async () => {
  const { call, close } = await serve();
  const path = "/v1/turns/turn%2F1/activity";
  try {
    expect(await call("GET", `${path}?interface_id=discord-private`)).toEqual({
      status: 200,
      body: { phase: "tools", running: [], recent: [{ tool: "read", ok: true }], tool_calls: 1 },
    });
    expect(
      (await call("GET", `${path}?interface_id=discord-private`, undefined, "wrong")).status,
    ).toBe(403);
    expect((await call("GET", `${path}?interface_id=other-interface`)).status).toBe(404);
    expect(
      (await call("GET", "/v1/turns/turn-2/activity?interface_id=discord-private")).status,
    ).toBe(404);
    for (const query of ["", "?interface_id=", "?interface_id=a%20b"])
      expect((await call("GET", `${path}${query}`)).status).toBe(400);
    expect(
      (await call("GET", "/v1/turns/%E0%A4%A/activity?interface_id=discord-private")).status,
    ).toBe(400);
  } finally {
    await close();
  }
});

it("sends a failed turn's report whether or not its explanation can be shown", async () => {
  const log = vi.spyOn(console, "error").mockImplementation(() => undefined);
  const { call, close } = await serve();
  try {
    expect(await call("POST", "/v1/turns", turn("reported The build failed."))).toEqual({
      status: 502,
      body: { error: "The build failed.", activity: REPORT },
    });
    // Unsafe text is withheld; the report is not.
    expect(
      await call("POST", "/v1/turns", turn("reported Authentication failed: password=x")),
    ).toEqual({ status: 502, body: { activity: REPORT } });
    // An internal failure keeps its status and its diagnostics in the log.
    expect(await call("POST", "/v1/turns", turn("reported internal"))).toEqual({
      status: 500,
      body: { activity: REPORT },
    });
    // So does a value that is no error, which has no explanation to show;
    // its diagnostic is the value's, redacted.
    expect(await call("POST", "/v1/turns", turn("reported value"))).toEqual({
      status: 500,
      body: { activity: REPORT },
    });
    expect(log.mock.calls.map((args) => args.join(" "))).toContainEqual(
      expect.stringMatching(
        /^POST \/v1\/turns failed: Error: the session's state rejected \[redacted\]\n\s+at /u,
      ),
    );
    // A closing runtime still gives no answer, report or not.
    expect(await call("POST", "/v1/turns", turn("reported closing"))).toEqual({
      status: 503,
      body: {},
    });
    expect(JSON.stringify(log.mock.calls)).not.toContain(key);
  } finally {
    await close();
  }
});
