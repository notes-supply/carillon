/**
 * MCP servers from the runtime's `mcp.json`, each the built-in extension
 * `mcp:<server>`: the server's tools, deferred so tool search loads them,
 * while it is connected, and none otherwise. A server connects once an
 * agent selects it, and connects again with backoff after it fails or
 * closes, or its session or transport is lost. The file holds credentials,
 * so it lives in the runtime's state directory, readable only by its owner,
 * never in an agent's resources.
 */
import { createHash } from "node:crypto";
import { open, type FileHandle } from "node:fs/promises";
// pi-durable types tools' arguments with pi-ai's TypeBox, a later one than Carillon's.
import { Type as ToolType } from "@earendil-works/pi-ai";
import {
  defineExtension,
  defineTool,
  type Extension,
  type ToolRegistration,
} from "@earendil-works/pi-durable";
import {
  McpClient,
  McpError,
  McpHttpError,
  McpSessionExpiredError,
  StdioTransport,
  StreamableHttpTransport,
  toLlmContent,
  type CallToolResult,
  type McpTransport,
  type Tool,
} from "@earendil-works/pi-mcp";
import { bounded, redactCredentials } from "@carillon/runtime/failure";
import { mismatch } from "@carillon/runtime/mismatch";
import { Type, type Static } from "typebox";
import { Check } from "typebox/value";
import packageInfo from "../package.json" with { type: "json" };
import { backoff } from "./delivery.ts";

const Strings = Type.Record(Type.String(), Type.String());
const StdioSchema = Type.Object(
  {
    command: Type.String({ minLength: 1 }),
    args: Type.Optional(Type.Array(Type.String())),
    env: Type.Optional(Strings),
    cwd: Type.Optional(Type.String({ minLength: 1 })),
  },
  { additionalProperties: false },
);
const HttpSchema = Type.Object(
  { url: Type.String({ minLength: 1 }), headers: Type.Optional(Strings) },
  { additionalProperties: false },
);
const FileSchema = Type.Object(
  { mcpServers: Type.Record(Type.String(), Type.Unknown()) },
  { additionalProperties: false },
);
/**
 * Without `_`, a server's name ends where `__` first follows `mcp__`, so no
 * two servers' tool names are alike.
 */
const SERVER_NAME = /^[a-z0-9]+(?:-[a-z0-9]+)*$/u;
/** Providers allow tool names of at most 64 characters. */
const LONGEST_TOOL_NAME = 64;
/** The most of an MCP failure the model or the log sees. */
const LONGEST_FAILURE = 1_000;
/** A connection that held this long was healthy, so the next one waits from the start. */
const HEALTHY_MS = 60_000;

/** How to reach one server: a program over stdio, or a Streamable HTTP endpoint. */
export type McpServerConfig = Static<typeof StdioSchema> | Static<typeof HttpSchema>;

/** The servers `mcp.json` configures, by name; none without the file. */
export async function loadMcpConfig(path: string): Promise<Map<string, McpServerConfig>> {
  let handle: FileHandle;
  try {
    handle = await open(path, "r");
  } catch (failure) {
    if (failure instanceof Error && "code" in failure && failure.code === "ENOENT")
      return new Map();
    throw failure;
  }
  let text: string;
  try {
    // As ssh does with a private key: others may neither read its
    // credentials nor change the programs it runs.
    const mode = (await handle.stat()).mode & 0o777;
    if ((mode & 0o077) !== 0)
      throw new Error(
        `${path} is open to other users (mode ${mode.toString(8).padStart(4, "0")}); it holds credentials, so make it readable only by you: chmod 600 ${path}`,
      );
    text = await handle.readFile("utf8");
  } finally {
    await handle.close();
  }
  let file: unknown;
  try {
    file = JSON.parse(text);
  } catch {
    throw new Error(`${path} is not valid JSON`);
  }
  if (!Check(FileSchema, file)) {
    const first = mismatch(FileSchema, file);
    throw new Error(
      `${path} must be {"mcpServers": {...}}: ${first === undefined ? "invalid" : `${first.instancePath || "/"} ${first.message}`}`,
    );
  }
  const servers = new Map<string, McpServerConfig>();
  for (const [name, server] of Object.entries(file.mcpServers)) {
    if (!SERVER_NAME.test(name))
      throw new Error(
        `${path}: server name ${JSON.stringify(name)} must be words of a-z and 0-9 joined by -, such as remote-notes`,
      );
    if (Check(StdioSchema, server)) servers.set(name, server);
    else if (Check(HttpSchema, server) && isHttp(server.url)) servers.set(name, server);
    else
      throw new Error(
        `${path}: server ${name} must be {command, args?, env?, cwd?} or {url, headers?} with an http: or https: URL`,
      );
  }
  return servers;
}

function isHttp(url: string): boolean {
  return URL.canParse(url) && ["http:", "https:"].includes(new URL(url).protocol);
}

/** The transport of one connection to the server. */
export function mcpTransport(server: McpServerConfig): McpTransport {
  return "command" in server ? new StdioTransport(server) : new StreamableHttpTransport(server);
}

export interface McpServer {
  /** The extension's name, `mcp:<server>`, as settings select it. */
  readonly name: string;
  /** The extension as the server stands: its tools while connected, none otherwise. */
  extension(): Extension;
  /** Starts connecting, unless the server already is or does: an agent selected it. */
  use(): void;
  /** Calls `listener` whenever the extension changes. */
  subscribe(listener: () => void): () => void;
  /** Closes the connection; the server connects no more. */
  close(): Promise<void>;
}

type Connection =
  | { readonly state: "unused" }
  | {
      readonly state: "connecting";
      readonly client: McpClient;
      /** Settles once connecting succeeds or fails. */
      readonly connected: Promise<void>;
    }
  | { readonly state: "connected"; readonly client: McpClient; readonly since: number }
  | { readonly state: "waiting"; readonly timer: NodeJS.Timeout }
  | { readonly state: "closed" };

/** One server, connected through `transport` each time it connects. */
export function createMcpServer(server: string, transport: () => McpTransport): McpServer {
  const name = `mcp:${server}`;
  const listeners = new Set<() => void>();
  let connection: Connection = { state: "unused" };
  let extension: Extension = defineExtension({ name, tools: [] });
  let failures = 0;

  function publish(tools: readonly Tool[], client?: McpClient): void {
    extension = defineExtension({
      name,
      tools: client === undefined ? [] : toolsOf(server, tools, client),
    });
    for (const listener of listeners) listener();
  }

  function connect(): void {
    const client = new McpClient({ name: "carillon", version: packageInfo.version });
    client.onClose(() => {
      if (connection.state !== "connected" || connection.client !== client) return;
      console.error(`MCP server ${server} disconnected`);
      if (Date.now() - connection.since >= HEALTHY_MS) failures = 0;
      publish([]);
      retry();
    });
    client.onError((failure) => {
      console.error(`MCP server ${server}: ${described(failure)}`);
      if (streamGivenUp(failure)) client.close().catch(() => undefined);
      else dropIfLost(client, failure);
    });
    connection = { state: "connecting", client, connected: establish(client) };
  }

  /** Connects the client and offers the server's tools, or tries again later. */
  async function establish(client: McpClient): Promise<void> {
    try {
      await client.connect(transport());
      const tools = await client.listTools();
      // The server was closed meanwhile, and with it this client.
      if (closed()) return;
      // A close before this point reached no handler that retries.
      if (client.connectionState !== "connected") throw new Error("the connection closed");
      connection = { state: "connected", client, since: Date.now() };
      client.onNotification("notifications/tools/list_changed", () => {
        client.listTools().then(
          (changed) => {
            if (connection.state === "connected" && connection.client === client)
              publish(changed, client);
          },
          (failure: unknown) => {
            console.error(`MCP server ${server}: ${described(failure)}`);
            dropIfLost(client, failure);
          },
        );
      });
      publish(tools, client);
    } catch (failure) {
      if (closed()) return;
      console.error(`MCP server ${server}: ${described(failure)}`);
      await client.close().catch(() => undefined);
      if (!closed()) retry();
    }
  }

  /** Whether the server was closed, also while a connection was under way. */
  function closed(): boolean {
    return connection.state === "closed";
  }

  function retry(): void {
    const timer = setTimeout(connect, backoff(failures));
    timer.unref();
    failures += 1;
    connection = { state: "waiting", timer };
  }

  return {
    name,
    extension: () => extension,
    use() {
      if (connection.state === "unused") connect();
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    async close() {
      const closing = connection;
      connection = { state: "closed" };
      if (closing.state === "waiting") clearTimeout(closing.timer);
      if (closing.state === "connecting" || closing.state === "connected")
        await closing.client.close();
      if (closing.state === "connecting") await closing.connected;
    },
  };
}

/**
 * Closes the client once the connection is lost, rather than one request
 * failed: its close handler withdraws the tools and connects anew, with a
 * fresh initialization. A request the server refused, such as with HTTP 429,
 * leaves the connection to the others.
 */
function dropIfLost(client: McpClient, failure: unknown): void {
  if (
    failure instanceof McpSessionExpiredError ||
    // `fetch` reports a network failure as a `TypeError`.
    failure instanceof TypeError ||
    (failure instanceof McpError && failure.message.startsWith("MCP response stream failed"))
  )
    client.close().catch(() => undefined);
}

/**
 * Whether the HTTP transport gave its server-to-client stream up, which
 * only `onError` hears of: after retrying it, or on a status no retry
 * changes.
 */
function streamGivenUp(failure: unknown): boolean {
  return failure instanceof McpHttpError
    ? failure.status < 500 && ![408, 429].includes(failure.status)
    : failure instanceof Error &&
        failure.message === "MCP server-to-client stream dropped and could not be reopened";
}

/** A failure as the model and the log see it, its credentials redacted. */
function described(failure: unknown): string {
  return redacted(messageOf(failure));
}

function messageOf(failure: unknown): string {
  return failure instanceof Error ? failure.message : String(failure);
}

function redacted(text: string): string {
  return bounded(redactCredentials(text), LONGEST_FAILURE);
}

/** A failed call as the model sees it. */
function failed(tool: string, reason: string) {
  return {
    output: [
      {
        type: "text" as const,
        text: reason.trim() === "" ? `${tool} failed.` : `${tool} failed: ${redacted(reason)}`,
      },
    ],
    isError: true,
  };
}

/** The server's tools as deferred pi-durable tools named `mcp__<server>__<tool>`. */
function toolsOf(server: string, tools: readonly Tool[], client: McpClient): ToolRegistration[] {
  const names = new Set<string>();
  return tools.map((tool) => {
    const name = toolName(server, tool.name, names);
    names.add(name);
    return defineTool({
      name,
      description: tool.description ?? tool.title ?? tool.name,
      // Providers take an object schema, and some refuse one without properties.
      parameters: ToolType.Unsafe<Record<string, unknown>>({
        ...tool.inputSchema,
        type: "object",
        properties: tool.inputSchema["properties"] ?? {},
      }),
      deferred: true,
      async execute(args, _api, context) {
        let result: CallToolResult;
        try {
          result = await client.callTool(
            tool.name,
            args,
            context.abortSignal === undefined ? {} : { signal: context.abortSignal },
          );
        } catch (failure) {
          if (context.abortSignal?.aborted === true) throw failure;
          dropIfLost(client, failure);
          return failed(tool.name, messageOf(failure));
        }
        const output = toLlmContent(result);
        // MCP reports a failed call in its result rather than as a protocol error.
        if (result.isError !== true) return { output };
        const reason = output.flatMap((part) => (part.type === "text" ? [part.text] : []));
        return {
          output: [
            ...failed(tool.name, reason.join("\n")).output,
            ...output.filter((part) => part.type !== "text"),
          ],
          isError: true,
        };
      },
    });
  });
}

/**
 * `mcp__<server>__<tool>` with every character but A-Z, a-z, 0-9 and _ as
 * _, shortened with a hash of both names when too long or already taken.
 */
function toolName(server: string, tool: string, taken: ReadonlySet<string>): string {
  const name = `mcp__${server}__${tool}`.replace(/[^A-Za-z0-9_]/gu, "_");
  if (name.length <= LONGEST_TOOL_NAME && !taken.has(name)) return name;
  const hash = createHash("sha256").update(`${server}\0${tool}`).digest("hex").slice(0, 8);
  return `${name.slice(0, LONGEST_TOOL_NAME - hash.length - 1)}_${hash}`;
}
