import { fauxAssistantMessage, fauxToolCall, type TranscriptContext } from "@earendil-works/pi-ai";
import { createToolSearchExtension } from "@earendil-works/pi-durable/tool-search";
import {
  isJsonRpcRequest,
  LATEST_PROTOCOL_VERSION,
  StreamableHttpTransport,
  type JsonRpcRequest,
  type McpTransport,
} from "@earendil-works/pi-mcp";
import {
  createInMemoryTransportPair,
  type InMemoryTransport,
} from "@earendil-works/pi-mcp/testing";
import { once } from "node:events";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer, type ServerResponse } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, it, vi } from "vitest";
import { fakeClock, startFixture, textsOf, type Fixture } from "./fixture.ts";
import { createMcpServer, loadMcpConfig, type McpServer } from "./mcp.ts";

const cleanups: (() => Promise<unknown>)[] = [];
afterEach(async () => {
  for (const cleanup of cleanups.splice(0).toReversed()) await cleanup();
  vi.useRealTimers();
});

/**
 * How a fake server answers a call of its one tool: an echo, this JSON-RPC
 * error, or this failed result, with `image` after its text.
 */
interface Served {
  readonly tool?: string;
  readonly refusal?: string;
  readonly failure?: string;
  readonly image?: { readonly data: string; readonly mimeType: string };
}

/** A fake server's answer to a request, or its JSON-RPC error. */
function answer(
  request: JsonRpcRequest,
  calls: unknown[],
  { tool = "echo", refusal, failure, image }: Served,
): { result: unknown } | { error: { code: number; message: string } } {
  switch (request.method) {
    case "initialize":
      return {
        result: {
          protocolVersion: LATEST_PROTOCOL_VERSION,
          capabilities: { tools: {} },
          serverInfo: { name: "notes", version: "1.0.0" },
        },
      };
    case "tools/list":
      return {
        result: {
          tools: [
            {
              name: tool,
              description: "Echoes a text.",
              inputSchema: {
                type: "object",
                properties: { text: { type: "string" } },
                required: ["text"],
              },
            },
          ],
        },
      };
    case "tools/call": {
      calls.push(request.params);
      if (refusal !== undefined) return { error: { code: -32_000, message: refusal } };
      if (failure !== undefined)
        return {
          result: {
            content: [
              { type: "text", text: failure },
              ...(image === undefined ? [] : [{ type: "image", ...image }]),
            ],
            isError: true,
          },
        };
      const args: unknown =
        typeof request.params === "object" && request.params !== null
          ? Reflect.get(request.params, "arguments")
          : undefined;
      const text: unknown =
        typeof args === "object" && args !== null ? Reflect.get(args, "text") : "";
      return { result: { content: [{ type: "text", text: `echo: ${String(text)}` }] } };
    }
    default:
      return { result: {} };
  }
}

/** An MCP server in memory with one tool; each connection is a new transport. */
function fakeServer(served: Served = {}) {
  const calls: unknown[] = [];
  const requests: string[] = [];
  let latest: InMemoryTransport | undefined;
  let connections = 0;
  return {
    calls,
    requests,
    get connections() {
      return connections;
    },
    transport(): McpTransport {
      const { client, server } = createInMemoryTransportPair();
      connections += 1;
      latest = server;
      server.onMessage((message) => {
        if (!isJsonRpcRequest(message)) return;
        requests.push(message.method);
        void server.send({ jsonrpc: "2.0", id: message.id, ...answer(message, calls, served) });
      });
      void server.start();
      return client;
    },
    /** Ends the latest connection from the server's side. */
    disconnect: () => latest?.close(),
  };
}

/**
 * A Streamable HTTP MCP server with one tool, whose session can be made to
 * expire. `call` may answer a tool call in place of the echo `answer` sends,
 * and with `stream` the server offers a server-to-client stream, which ends
 * once the session expires.
 */
async function httpServer({
  call,
  stream = false,
  get,
}: {
  call?: (request: JsonRpcRequest, response: ServerResponse, answer: () => void) => void;
  stream?: boolean;
  /** Answers the server-to-client stream's requests in the session in place of `stream`. */
  get?: (response: ServerResponse, session: number) => void;
} = {}) {
  const calls: unknown[] = [];
  let sessions = 0;
  let expired = false;
  const streams = new Set<ServerResponse>();
  const server = createServer((request, response) => {
    if (request.method === "GET" && get !== undefined) {
      get(response, sessions);
      return;
    }
    if (request.method === "GET" && stream) {
      if (expired) {
        response.statusCode = 404;
        response.end();
        return;
      }
      // Opened again a moment after it ends.
      response.writeHead(200, { "content-type": "text/event-stream" }).write("retry: 10\n\n");
      streams.add(response);
      return;
    }
    // No server-to-client stream; a session ends without one.
    if (request.method !== "POST") {
      response.statusCode = request.method === "GET" ? 405 : 200;
      response.end();
      return;
    }
    let body = "";
    request.on("data", (part: Buffer) => (body += part.toString()));
    request.on("end", () => {
      const message: unknown = JSON.parse(body);
      if (!isJsonRpcRequest(message)) {
        response.statusCode = 202;
        response.end();
        return;
      }
      if (message.method === "initialize") {
        sessions += 1;
        expired = false;
        response.setHeader("mcp-session-id", `session-${String(sessions)}`);
      } else if (expired) {
        response.statusCode = 404;
        response.end();
        return;
      }
      const echo = () => {
        response.setHeader("content-type", "application/json");
        response.end(
          JSON.stringify({ jsonrpc: "2.0", id: message.id, ...answer(message, calls, {}) }),
        );
      };
      if (message.method === "tools/call" && call !== undefined) call(message, response, echo);
      else echo();
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  cleanups.push(() => new Promise((done) => server.close(done)));
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("expected a TCP listener");
  return {
    url: `http://127.0.0.1:${String(address.port)}/mcp`,
    calls,
    get sessions() {
      return sessions;
    },
    expire() {
      expired = true;
      for (const open of streams) open.end();
      streams.clear();
    },
  };
}

function served(server: ReturnType<typeof fakeServer>, name = "notes"): McpServer {
  const mcp = createMcpServer(name, () => server.transport());
  cleanups.push(() => mcp.close());
  return mcp;
}

async function until(done: () => boolean): Promise<void> {
  for (let attempt = 0; !done() && attempt < 300; attempt += 1)
    await new Promise((resolve) => setTimeout(resolve, 10));
  expect(done()).toBe(true);
}

const offeredTools = (context: TranscriptContext) =>
  context.messages.flatMap((message) =>
    message.role === "system" ? (message.toolsAdded ?? []).map((tool) => tool.name) : [],
  );

/**
 * An agent selecting tool search and `mcp:notes`, once the server's tools
 * are offered, whose model calls `mcp__notes__echo` once for each text, at
 * once, when it is asked to echo, and answers with the calls' results.
 */
async function echoing(mcp: McpServer, texts: readonly string[] = ["hi"]) {
  const offered: string[][] = [];
  const returned: unknown[] = [];
  const echo = [
    fauxAssistantMessage(fauxToolCall("tool_search", { query: "echo a text" }), {
      stopReason: "toolUse",
    }),
    (context: TranscriptContext) => {
      offered.push(offeredTools(context));
      return fauxAssistantMessage(
        texts.map((text) => fauxToolCall("mcp__notes__echo", { text })),
        { stopReason: "toolUse" },
      );
    },
    (context: TranscriptContext) => {
      const asked = context.messages.findLastIndex((message) => message.role === "assistant");
      const calls = context.messages.slice(asked + 1);
      const results = calls.flatMap((message) =>
        message.role === "toolResult" ? [textsOf(message).join("")] : [],
      );
      for (const message of calls)
        if (message.role === "toolResult") returned.push(message.content);
      return fauxAssistantMessage(`Tool: ${results.join(" | ")}`);
    },
  ];
  const agent = await startFixture({
    builtIns: [createToolSearchExtension()],
    mcp: [mcp],
    answers: [fauxAssistantMessage("Ready."), ...echo, ...echo],
  });
  cleanups.push(() => agent.close());
  const root = await agent.snapshot({
    "settings.json": JSON.stringify({ extensions: ["tool-search", "mcp:notes"] }),
  });
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello", { resource_root: root }));
  expect(await agent.outcome("turn-1")).toMatchObject({ output: "Ready." });
  await until(() => (mcp.extension().tools ?? []).length > 0);
  return {
    agent,
    offered,
    /** The content each call returned to the model. */
    returned,
    /** Asks the agent to echo, returning its answer. */
    echo: async (turnId: string): Promise<string> => {
      await agent.runtime.turns.submit(agent.turn(turnId, "Echo hi.", { resource_root: root }));
      const outcome = await agent.outcome(turnId);
      if (outcome.outcome !== "answered") throw new Error(`turn ${turnId} was ${outcome.outcome}`);
      return outcome.output;
    },
  };
}

it("offers a selected server's tools to tool search, and calls them", async () => {
  const server = fakeServer();
  const mcp = served(server);
  expect(server.connections).toBe(0);
  const { echo, offered } = await echoing(mcp);
  expect(await echo("turn-2")).toBe("Tool: echo: hi");
  expect(offered[0]).toContain("mcp__notes__echo");
  expect(server.calls).toEqual([{ name: "echo", arguments: { text: "hi" } }]);
});

const token = "sk-live-4f9c2a7e1b8d3f6a0c5e9b2d7a4f1c8e";
const quoting = `upstream refused Authorization: Bearer ${token} for ?api_key=${token}; ${"x".repeat(5_000)}`;

it.each([
  ["an error", { refusal: quoting }],
  ["a failed result", { failure: quoting }],
])("shows neither the model nor the log the credentials %s quotes", async (_name, failing) => {
  const server = fakeServer(failing);
  const log = vi.spyOn(console, "error");
  const { echo } = await echoing(served(server));
  const answered = await echo("turn-2");
  expect(answered).toContain("echo failed: upstream refused Authorization: [redacted]");
  expect(answered).not.toContain(token);
  expect(answered.length).toBeLessThan(1_200);
  expect(log.mock.calls.flat().join("\n")).not.toContain(token);
});

it("keeps a failed result's image, its text redacted", async () => {
  const image = { data: "iVBORw0KGgo=", mimeType: "image/png" };
  const { echo, returned } = await echoing(served(fakeServer({ failure: quoting, image })));
  await echo("turn-2");
  expect(returned).toMatchObject([[{ type: "text" }, { type: "image", ...image }]]);
  const text = JSON.stringify(returned);
  expect(text).toContain("echo failed: upstream refused Authorization: [redacted]");
  expect(text).not.toContain(token);
});

it("waits longer each time a connection drops at once, and from the start after one held a minute", async () => {
  vi.useFakeTimers();
  const server = fakeServer();
  const mcp = served(server);
  let offers = 0;
  mcp.subscribe(() => {
    if ((mcp.extension().tools ?? []).length > 0) offers += 1;
  });
  /** Ends the connection once its tools are offered, and returns how long the next one waited. */
  async function dropped(): Promise<number> {
    const before = offers;
    void server.disconnect();
    let waited = 0;
    for (; offers === before && waited < 120_000; waited += 100)
      await vi.advanceTimersByTimeAsync(100);
    return waited;
  }
  mcp.use();
  await vi.advanceTimersByTimeAsync(0);
  expect(offers).toBe(1);
  expect(await dropped()).toBe(1_000);
  expect(await dropped()).toBe(2_000);
  expect(await dropped()).toBe(4_000);
  await vi.advanceTimersByTimeAsync(60_000);
  expect(await dropped()).toBe(1_000);
});

it("connects once selected, exposes no tools while disconnected, and connects again", async () => {
  const server = fakeServer();
  const mcp = served(server);
  const changes: (readonly string[])[] = [];
  mcp.subscribe(() => changes.push((mcp.extension().tools ?? []).map((tool) => tool.name)));
  expect(mcp.extension()).toEqual({ name: "mcp:notes", tools: [] });
  mcp.use();
  await until(() => changes.length === 1);
  expect(changes).toEqual([["mcp__notes__echo"]]);
  expect(mcp.extension().tools?.[0]?.deferred).toBe(true);
  void server.disconnect();
  await until(() => changes.length === 2);
  expect(changes[1]).toEqual([]);
  // The first reconnection waits a second.
  await until(() => changes.length === 3);
  expect(changes[2]).toEqual(["mcp__notes__echo"]);
  expect(server.connections).toBe(2);
});

it("withdraws an HTTP server's tools once its session expires, and initializes anew", async () => {
  const server = await httpServer();
  const mcp = createMcpServer("notes", () => new StreamableHttpTransport({ url: server.url }));
  cleanups.push(() => mcp.close());
  const { echo } = await echoing(mcp);
  const changes: (readonly string[])[] = [];
  mcp.subscribe(() => changes.push((mcp.extension().tools ?? []).map((tool) => tool.name)));
  server.expire();
  expect(await echo("turn-2")).toContain("echo failed: MCP session expired");
  await until(() => changes.length === 2);
  expect(changes).toEqual([[], ["mcp__notes__echo"]]);
  expect(server.sessions).toBe(2);
  expect(await echo("turn-3")).toBe("Tool: echo: hi");
});

it("initializes anew once a call's response stream ends without its answer", async () => {
  let cut = true;
  const server = await httpServer({
    call: (_request, response, answer) => {
      if (cut) response.writeHead(200, { "content-type": "text/event-stream" }).end();
      else answer();
      cut = false;
    },
  });
  const mcp = createMcpServer("notes", () => new StreamableHttpTransport({ url: server.url }));
  cleanups.push(() => mcp.close());
  const { echo } = await echoing(mcp);
  const changes: (readonly string[])[] = [];
  mcp.subscribe(() => changes.push((mcp.extension().tools ?? []).map((tool) => tool.name)));
  expect(await echo("turn-2")).toContain("echo failed: MCP response stream failed");
  await until(() => changes.length === 2);
  expect(changes).toEqual([[], ["mcp__notes__echo"]]);
  expect(server.sessions).toBe(2);
  expect(await echo("turn-3")).toBe("Tool: echo: hi");
});

it("initializes anew once the server-to-client stream finds the session expired", async () => {
  const server = await httpServer({ stream: true });
  const mcp = createMcpServer("notes", () => new StreamableHttpTransport({ url: server.url }));
  cleanups.push(() => mcp.close());
  const { echo } = await echoing(mcp);
  const changes: (readonly string[])[] = [];
  mcp.subscribe(() => changes.push((mcp.extension().tools ?? []).map((tool) => tool.name)));
  server.expire();
  await until(() => changes.length === 2);
  expect(changes).toEqual([[], ["mcp__notes__echo"]]);
  expect(server.sessions).toBe(2);
  expect(await echo("turn-2")).toBe("Tool: echo: hi");
});

it.each([
  ["cannot reopen it", 503],
  ["is refused it", 403],
])("initializes anew once the server-to-client stream %s", async (_name, status) => {
  // The first session's stream fails once its tools are offered; later sessions offer none.
  const offered = Promise.withResolvers<void>();
  const server = await httpServer({
    get: (response, session) => {
      void (session === 1 ? offered.promise : Promise.resolve()).then(() => {
        response.statusCode = session === 1 ? status : 405;
        response.end();
      });
    },
  });
  const mcp = createMcpServer(
    "notes",
    () =>
      new StreamableHttpTransport({
        url: server.url,
        reconnect: { initialDelayMs: 10, maxRetries: 2 },
      }),
  );
  cleanups.push(() => mcp.close());
  const changes: (readonly string[])[] = [];
  mcp.subscribe(() => changes.push((mcp.extension().tools ?? []).map((tool) => tool.name)));
  mcp.use();
  await until(() => changes.length === 1);
  offered.resolve();
  await until(() => changes.length === 3);
  expect(changes).toEqual([["mcp__notes__echo"], [], ["mcp__notes__echo"]]);
  expect(server.sessions).toBe(2);
});

it("keeps the connection and its other calls when the server refuses one call", async () => {
  const refused = Promise.withResolvers<void>();
  const server = await httpServer({
    call: (request, response, answer) => {
      if (JSON.stringify(request.params).includes('"slow"')) {
        response.statusCode = 429;
        response.end("slow down");
        refused.resolve();
        return;
      }
      void refused.promise.then(() => setTimeout(answer, 50));
    },
  });
  const mcp = createMcpServer("notes", () => new StreamableHttpTransport({ url: server.url }));
  cleanups.push(() => mcp.close());
  const { echo } = await echoing(mcp, ["slow", "fast"]);
  expect(await echo("turn-2")).toBe(
    "Tool: echo failed: MCP HTTP request failed with status 429: slow down | echo: fast",
  );
  expect(mcp.extension().tools?.map((tool) => tool.name)).toEqual(["mcp__notes__echo"]);
  expect(server.sessions).toBe(1);
});

it("leaves no transport open when it closes while the server initializes", async () => {
  const { client, server } = createInMemoryTransportPair();
  const initializing = Promise.withResolvers<void>();
  let open = true;
  server.onMessage((message) => {
    if (isJsonRpcRequest(message) && message.method === "initialize") initializing.resolve();
  });
  server.onClose(() => {
    open = false;
  });
  void server.start();
  let connections = 0;
  const mcp = createMcpServer("notes", () => {
    connections += 1;
    return client;
  });
  mcp.use();
  await initializing.promise;
  await mcp.close();
  expect(open).toBe(false);
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(connections).toBe(1);
});

it("names two servers' tools apart, as names without _ keep them", async () => {
  const servers = [
    served(fakeServer({ tool: "b__c" }), "a"),
    served(fakeServer({ tool: "c" }), "a-b"),
  ];
  for (const server of servers) server.use();
  await until(() => servers.every((server) => (server.extension().tools ?? []).length > 0));
  expect(servers.map((server) => server.extension().tools?.map((tool) => tool.name))).toEqual([
    ["mcp__a__b__c"],
    ["mcp__a_b__c"],
  ]);
});

it("keeps a due schedule while a server the resources select is gone, until they load", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  const mcp = served(fakeServer());
  let agent: Fixture = await startFixture({
    clock,
    durable: true,
    mcp: [mcp],
    answers: [
      fauxAssistantMessage(
        fauxToolCall("schedule", { prompt: "Water the plants.", at: "2026-10-24T09:00:00Z" }),
        { stopReason: "toolUse" },
      ),
      fauxAssistantMessage("Scheduled."),
      fauxAssistantMessage("Hello."),
      fauxAssistantMessage("Watered."),
    ],
  });
  cleanups.push(() => agent.close());
  const root = await agent.snapshot({
    "settings.json": JSON.stringify({ extensions: ["schedules", "mcp:notes"] }),
  });
  await agent.runtime.turns.submit(agent.turn("turn-1", "Schedule it.", { resource_root: root }));
  expect(await agent.outcome("turn-1")).toMatchObject({ output: "Scheduled." });
  // mcp.json no longer configures the server, so the recorded resources no longer load.
  agent = await agent.restart({ mcp: [] });
  clock.advance(60 * 60_000);
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(agent.openings()).toEqual([]);
  const loads = await agent.snapshot({
    "settings.json": JSON.stringify({ extensions: ["schedules"] }),
  });
  await agent.runtime.turns.submit(agent.turn("turn-2", "hello", { resource_root: loads }));
  expect(await agent.outcome("turn-2")).toMatchObject({ output: "Hello." });
  clock.advance(60_000);
  expect(await agent.outcome("opened-1")).toMatchObject({ output: "Watered." });
});

it("reads mcp.json's servers, and none without one", async () => {
  const directory = await mkdtemp(join(tmpdir(), "carillon-mcp-"));
  cleanups.push(() => rm(directory, { recursive: true, force: true }));
  const path = join(directory, "mcp.json");
  expect(await loadMcpConfig(path)).toEqual(new Map());
  const servers = {
    files: { command: "mcp-files", args: ["--root", "/srv"], env: { TOKEN: "x" }, cwd: "/srv" },
    "remote-notes": { url: "https://mcp.example.com/mcp", headers: { authorization: "Bearer x" } },
  };
  await writeFile(path, JSON.stringify({ mcpServers: servers }), { mode: 0o600 });
  expect(await loadMcpConfig(path)).toEqual(new Map(Object.entries(servers)));
});

it.each([
  ["text that is not JSON", "{", "is not valid JSON"],
  ["a file without mcpServers", JSON.stringify({ servers: {} }), 'must be {"mcpServers": {...}}'],
  [
    "a server with neither a command nor a URL",
    JSON.stringify({ mcpServers: { files: { program: "mcp-files" } } }),
    "server files must be {command, args?, env?, cwd?} or {url, headers?}",
  ],
  [
    "a server with a relative URL",
    JSON.stringify({ mcpServers: { notes: { url: "/mcp" } } }),
    "server notes must be",
  ],
  [
    "a server with a URL that is not HTTP",
    JSON.stringify({ mcpServers: { notes: { url: "file:///srv/notes" } } }),
    "with an http: or https: URL",
  ],
  [
    "a server whose name a tool name cannot hold",
    JSON.stringify({ mcpServers: { "my notes": { url: "https://mcp.example.com" } } }),
    'server name "my notes" must be words of a-z and 0-9 joined by -',
  ],
  [
    "a server whose name has _, which another server's tools could share",
    JSON.stringify({ mcpServers: { a_b: { url: "https://mcp.example.com" } } }),
    'server name "a_b" must be',
  ],
])("refuses an mcp.json with %s", async (_name, text, message) => {
  const directory = await mkdtemp(join(tmpdir(), "carillon-mcp-"));
  cleanups.push(() => rm(directory, { recursive: true, force: true }));
  const path = join(directory, "mcp.json");
  await writeFile(path, text, { mode: 0o600 });
  await expect(loadMcpConfig(path)).rejects.toThrow(message);
  await expect(loadMcpConfig(path)).rejects.toThrow(path);
});

it("refuses an mcp.json other users may read, as ssh refuses such a key", async () => {
  const directory = await mkdtemp(join(tmpdir(), "carillon-mcp-"));
  cleanups.push(() => rm(directory, { recursive: true, force: true }));
  const path = join(directory, "mcp.json");
  await writeFile(path, JSON.stringify({ mcpServers: {} }), { mode: 0o640 });
  await expect(loadMcpConfig(path)).rejects.toThrow(
    `${path} is open to other users (mode 0640); it holds credentials, so make it readable only by you: chmod 600 ${path}`,
  );
});
