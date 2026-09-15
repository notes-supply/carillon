import {
  ModelRuntime,
  SessionManager,
  SettingsManager,
  type AgentSessionEvent,
} from "@earendil-works/pi-coding-agent";
import { registerBunOAuthFlows } from "@earendil-works/pi-ai/bun-oauth";
import { createHash, timingSafeEqual } from "node:crypto";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { mkdir, readFile, realpath, rename, stat, writeFile } from "node:fs/promises";
import { basename, isAbsolute, join, relative, resolve } from "node:path";
import { parse as parseToml } from "smol-toml";
import { Type } from "typebox";
import { Parse } from "typebox/value";
import { nativeSessionId } from "./namespace.ts";
import { createPiExecutionSession } from "./session.ts";
import { runtimeListenAddress } from "./endpoint.ts";
import packageInfo from "../package.json" with { type: "json" };

registerBunOAuthFlows();

interface RuntimeProviderConfig {
  readonly id: string;
  readonly url: string;
  readonly token_hash: string;
}

interface Config {
  readonly root: string;
  readonly resourceRoot: string;
  readonly workspace: string;
  readonly runtime: RuntimeProviderConfig;
}

interface TurnRequest {
  readonly correlation_id: string;
  readonly interface_id: string;
  readonly agent_id: string;
  readonly audience_id: string;
  readonly input: string;
  readonly continuation_ref?: string | null;
  readonly resource_root: string;
}

interface TurnResponse {
  readonly correlation_id: string;
  readonly output: string;
  readonly continuation_ref: string;
}
interface DeterministicSession {
  readonly turns: readonly {
    readonly correlation_id: string;
    readonly input: string;
    readonly output: string;
  }[];
}

const ProviderIdentitySchema = Type.String({
  minLength: 1,
  maxLength: 128,
  pattern: "^[A-Za-z0-9_-]+$",
});
const ProviderSchema = Type.Object({
  id: ProviderIdentitySchema,
  role: Type.String({ minLength: 1 }),
  version: Type.String({ minLength: 1 }),
  url: Type.String({ minLength: 1 }),
  token_hash: Type.String({ pattern: "^[a-f0-9]{64}$" }),
  enabled: Type.Boolean(),
});
const TurnRequestSchema = Type.Object({
  correlation_id: Type.String(),
  interface_id: ProviderIdentitySchema,
  agent_id: Type.String(),
  audience_id: Type.String(),
  input: Type.String(),
  continuation_ref: Type.Optional(Type.Union([Type.String(), Type.Null()])),
  resource_root: Type.String(),
});
const DeterministicSessionSchema = Type.Object({
  turns: Type.Array(
    Type.Object({
      correlation_id: Type.String(),
      input: Type.String(),
      output: Type.String(),
    }),
  ),
});
const FileConfigSchema = Type.Object({
  data_dir: Type.String(),
  agent: Type.Object({ resource_root: Type.String(), workspace: Type.String() }),
  providers: Type.Array(ProviderSchema),
});
let config: Config;
let token: string;
let deterministic: boolean;
let sessionDirectory: string;

let privateSettings: SettingsManager;
let modelRuntime: ModelRuntime | undefined;
let queue: Promise<void> = Promise.resolve();

async function startRuntime(values: readonly string[]): Promise<void> {
  const args = parseArguments(values);
  const providerId = requiredArgument(args, "provider");
  config = await loadConfig(requiredArgument(args, "config"), providerId);
  if (!(await stat(config.workspace)).isDirectory())
    throw new Error("workspace must be an existing directory");
  if (!(await stat(config.resourceRoot)).isDirectory())
    throw new Error("resource root must be an existing directory");
  const listenAddress = runtimeListenAddress(config.runtime.url);
  token = (await readFile(requiredArgument(args, "token-file"), "utf8")).trim();
  if (
    token.length === 0 ||
    createHash("sha256").update(token).digest("hex") !== config.runtime.token_hash
  )
    throw new Error("runtime credential does not match configuration");
  deterministic = args.has("deterministic");
  const privateStateDirectory = resolve(requiredArgument(args, "state-dir"));
  sessionDirectory = join(config.resourceRoot, "sessions");

  await mkdir(sessionDirectory, { recursive: true });
  await mkdir(privateStateDirectory, { recursive: true, mode: 0o700 });
  privateSettings = SettingsManager.create(privateStateDirectory, privateStateDirectory);
  modelRuntime = deterministic
    ? undefined
    : await ModelRuntime.create({
        authPath: join(privateStateDirectory, "auth.json"),
        modelsPath: join(privateStateDirectory, "models.json"),
        modelsStorePath: join(privateStateDirectory, "models-store.json"),
        allowModelNetwork: true,
        modelRefreshTimeoutMs: 15_000,
      });
  if (!deterministic) await selectedPiModel();

  const server = createServer((request, response) => {
    void route(request, response).catch((failure: unknown) => {
      const message = errorMessage(failure);
      writeJson(response, 500, { error: message });
    });
  });
  server.listen(listenAddress, () => {
    process.stdout.write(
      `runtime-pi ready at ${config.runtime.url}${deterministic ? " (deterministic provider substitute)" : ""}\n`,
    );
  });
}

async function route(request: IncomingMessage, response: ServerResponse): Promise<void> {
  if (request.method === "GET" && request.url === "/health") {
    writeJson(response, 200, {
      id: config.runtime.id,
      role: "runtime",
      version: packageInfo.version,
      status: "ready",
      features: ["native-continuations", "native-extensions", "skills"],
      provider: deterministic ? "deterministic-local-substitute" : "pi",
    });
    return;
  }
  if (!authorized(request)) {
    writeJson(response, 403, { error: "component authentication failed" });
    return;
  }
  if (request.method === "GET" && request.url === "/v1/environment") {
    const identity = unixIdentity();
    writeJson(response, 200, {
      ...identity,
      user: process.env["USER"],
      home: process.env["HOME"],
      shell: process.env["SHELL"],
      path: process.env["PATH"],
      workspace: config.workspace,
    });
    return;
  }

  if (request.method === "POST" && request.url === "/v1/turns") {
    let parsed: TurnRequest;
    try {
      parsed = Parse(TurnRequestSchema, await readJson(request));
    } catch {
      writeJson(response, 400, { error: "invalid runtime turn request" });
      return;
    }
    const result = await serialize(() => executeTurn(parsed));
    writeJson(response, 200, result);
    return;
  }

  writeJson(response, 404, { error: "not found" });
}

async function availablePiModels() {
  if (modelRuntime === undefined) throw new Error("Pi model runtime is unavailable");
  const models = [...(await modelRuntime.getAvailable())];
  models.sort(
    (left, right) =>
      left.provider.localeCompare(right.provider) ||
      left.name.localeCompare(right.name) ||
      left.id.localeCompare(right.id),
  );
  return models;
}

async function selectedPiModel() {
  const provider = privateSettings.getDefaultProvider();
  const id = privateSettings.getDefaultModel();
  if (provider === undefined || id === undefined) throw new Error("no Pi model is selected");
  return piModelForReference(`${provider}/${id}`);
}

async function piModelForReference(reference: string) {
  const model = (await availablePiModels()).find(
    (candidate) => `${candidate.provider}/${candidate.id}` === reference,
  );
  if (model === undefined) throw new Error(`selected Pi model ${reference} is not available`);
  return model;
}
async function executeTurn(request: TurnRequest): Promise<TurnResponse> {
  return deterministic ? executeDeterministic(request) : executePi(request);
}

async function executeDeterministic(request: TurnRequest): Promise<TurnResponse> {
  const sessionPath = continuationPath(request);
  const prior = await readDeterministicSession(sessionPath);
  const extensionActive = await exists(
    join(request.resource_root, "extensions", "carillon-context.js"),
  );
  const skillActive = await exists(
    join(request.resource_root, "skills", "carillon-status", "SKILL.md"),
  );
  const markers = [
    extensionActive ? "native extension active" : "native extension inactive",
    skillActive ? "skill active" : "skill inactive",
  ];
  const output = `Local provider response ${String(prior.turns.length + 1)}: ${request.input}. ${markers.join("; ")}.`;
  const next: DeterministicSession = {
    turns: [
      ...prior.turns,
      { correlation_id: request.correlation_id, input: request.input, output },
    ],
  };
  await atomicJson(sessionPath, next);
  return {
    correlation_id: request.correlation_id,
    output,
    continuation_ref: sessionPath,
  };
}

async function executePi(request: TurnRequest): Promise<TurnResponse> {
  const root = resolve(request.resource_root);
  const isNew = request.continuation_ref === null || request.continuation_ref === undefined;
  const manager = isNew
    ? SessionManager.create(config.workspace, sessionDirectory, {
        id: nativeSessionId(request.interface_id, request.agent_id, request.audience_id),
      })
    : SessionManager.open(
        validateContinuation(request.continuation_ref),
        sessionDirectory,
        config.workspace,
      );
  const defaultModel = isNew ? await selectedPiModel() : undefined;
  const defaultReasoning =
    defaultModel?.reasoning === true
      ? (privateSettings.getModelThinkingLevel(defaultModel.provider, defaultModel.id) ??
        privateSettings.getDefaultThinkingLevel() ??
        "medium")
      : undefined;
  const runtime = modelRuntime;
  if (runtime === undefined) throw new Error("Pi model runtime is unavailable");
  const created = await createPiExecutionSession({
    resourceRoot: root,
    workspace: config.workspace,
    manager,
    modelRuntime: runtime,
    ...(defaultModel === undefined ? {} : { model: defaultModel }),
    ...(defaultReasoning === undefined ? {} : { thinkingLevel: defaultReasoning }),
  });
  const { session, extensionsResult } = created;
  try {
    assertExtensionsLoaded(extensionsResult.errors);
    const sessionFile = session.sessionFile;
    if (sessionFile === undefined) throw new Error("Pi did not create durable session evidence");
    let output = "";
    const unsubscribe = session.subscribe((event: AgentSessionEvent) => {
      if (event.type === "message_update" && event.assistantMessageEvent.type === "text_delta")
        output += event.assistantMessageEvent.delta;
    });
    try {
      await session.prompt(request.input);
      return {
        correlation_id: request.correlation_id,
        output,
        continuation_ref: sessionFile,
      };
    } finally {
      unsubscribe();
    }
  } finally {
    session.dispose();
  }
}

function assertExtensionsLoaded(errors: readonly { readonly error: string }[]): void {
  if (errors.length > 0)
    throw new Error(
      `Pi extension loading failed: ${errors.map((entry) => entry.error).join("; ")}`,
    );
}

function continuationPath(request: TurnRequest): string {
  if (request.continuation_ref !== null && request.continuation_ref !== undefined)
    return validateContinuation(request.continuation_ref);
  return join(
    sessionDirectory,
    `${safeName(request.interface_id)}--${safeName(request.agent_id)}--${safeName(request.audience_id)}.json`,
  );
}

function validateContinuation(value: string): string {
  const path = resolve(value);
  const child = relative(resolve(sessionDirectory), path);
  if (child.startsWith("..") || isAbsolute(child))
    return join(sessionDirectory, safeName(basename(path)));
  return path;
}

async function readDeterministicSession(path: string): Promise<DeterministicSession> {
  try {
    const content = await readFile(path, "utf8");
    return await Parse(DeterministicSessionSchema, JSON.parse(content));
  } catch (failure: unknown) {
    if (isNodeError(failure) && failure.code === "ENOENT") return { turns: [] };
    throw failure;
  }
}

async function atomicJson(path: string, value: unknown): Promise<void> {
  const temporary = `${path}.${String(process.pid)}.tmp`;
  await writeFile(temporary, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
  await rename(temporary, path);
}

async function loadConfig(path: string, providerId: string): Promise<Config> {
  Parse(ProviderIdentitySchema, providerId);
  const configPath = await realpath(path);
  const root = resolve(configPath, "..");
  const parsed = Parse(FileConfigSchema, parseToml(await readFile(configPath, "utf8")));
  const reserved = parsed.providers.find(
    (provider) => provider.id === "operator" || provider.id === "discord-bot",
  );
  if (reserved !== undefined)
    throw new Error(`provider ID ${reserved.id} is reserved for installation credentials`);
  const runtime = parsed.providers.find(
    (provider) => provider.id === providerId && provider.role === "runtime",
  );
  if (runtime === undefined) throw new Error(`runtime provider ${providerId} is not configured`);
  if (!runtime.enabled) throw new Error(`runtime provider ${providerId} is disabled`);
  return {
    root,
    resourceRoot: resolve(root, parsed.agent.resource_root),
    workspace: resolve(root, parsed.agent.workspace),
    runtime,
  };
}

function parseArguments(values: readonly string[]): ReadonlyMap<string, string> {
  const result = new Map<string, string>();
  for (let index = 0; index < values.length; index += 1) {
    const value = values[index];
    if (typeof value !== "string" || !value.startsWith("--"))
      throw new Error(`unexpected argument ${value ?? ""}`);
    const name = value.slice(2);
    if (name === "deterministic") {
      result.set(name, "true");
      continue;
    }
    const argument = values[index + 1];
    if (argument === undefined || argument.startsWith("--"))
      throw new Error(`missing value for --${name}`);
    result.set(name, argument);
    index += 1;
  }
  return result;
}

function requiredArgument(values: ReadonlyMap<string, string>, name: string): string {
  const value = values.get(name);
  if (value === undefined) throw new Error(`--${name} is required`);
  return value;
}

function authorized(request: IncomingMessage): boolean {
  const candidate = request.headers.authorization?.replace(/^Bearer /u, "");
  if (candidate === undefined) return false;
  const expected = createHash("sha256").update(token).digest();
  const actual = createHash("sha256").update(candidate).digest();
  return timingSafeEqual(expected, actual);
}

async function serialize<T>(operation: () => Promise<T>): Promise<T> {
  const prior = queue;
  let release = (): void => undefined;
  queue = new Promise<void>((resolveQueue) => {
    release = resolveQueue;
  });
  await prior;
  try {
    return await operation();
  } finally {
    release();
  }
}

async function readJson(request: IncomingMessage): Promise<unknown> {
  request.setEncoding("utf8");
  let body = "";
  for await (const chunk of request) {
    if (typeof chunk !== "string") throw new Error("request body is not UTF-8 text");
    body += chunk;
    if (Buffer.byteLength(body) > 1_048_576) throw new Error("request body exceeds 1 MiB");
  }
  return JSON.parse(body) as unknown;
}

function writeJson(response: ServerResponse, status: number, value: unknown): void {
  const body = JSON.stringify(value);
  response.writeHead(status, {
    "content-type": "application/json",
    "content-length": Buffer.byteLength(body),
  });
  response.end(body);
}

async function exists(path: string): Promise<boolean> {
  try {
    await readFile(path);
    return true;
  } catch (failure: unknown) {
    if (isNodeError(failure) && failure.code === "ENOENT") return false;
    throw failure;
  }
}

function unixIdentity(): { readonly uid: number; readonly groups: number[] } {
  if (process.getuid === undefined || process.getgroups === undefined)
    throw new Error("Carillon Pi runtime requires Unix process identity APIs");
  return { uid: process.getuid(), groups: process.getgroups() };
}

function isNodeError(value: unknown): value is NodeJS.ErrnoException {
  return value instanceof Error && "code" in value;
}

function errorMessage(value: unknown): string {
  return value instanceof Error ? value.message : String(value);
}

function safeName(value: string): string {
  if (!/^[A-Za-z0-9._-]+$/u.test(value))
    throw new Error("identifier contains unsupported characters");
  return value;
}

await startRuntime(process.argv.slice(2));
