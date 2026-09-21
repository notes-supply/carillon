import {
  ModelRuntime,
  SettingsManager,
  type AgentSessionEvent,
} from "@earendil-works/pi-coding-agent";
import {
  clampThinkingLevel,
  getSupportedThinkingLevels,
  type Api,
  type Model,
  type ModelThinkingLevel,
} from "@earendil-works/pi-ai";
import { registerBunOAuthFlows } from "@earendil-works/pi-ai/bun-oauth";
import { createHash, timingSafeEqual } from "node:crypto";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { mkdir, readFile, realpath, rename, stat, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { parse as parseToml } from "smol-toml";
import { Type } from "typebox";
import { Parse } from "typebox/value";
import {
  DETERMINISTIC_DEFAULT,
  DETERMINISTIC_MODELS,
  ModelQuerySchema,
  ModelRejection,
  ModelSelections,
  ModelUpdateSchema,
  describeSelection,
  type AvailableModel,
  type ModelQuery,
  type ModelUpdate,
  type Selection,
} from "./model.ts";
import { conversationDirectory, openConversation } from "./namespace.ts";
import { Serial } from "./serial.ts";
import { createPiExecutionSession, promptSession } from "./session.ts";
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
  readonly resource_root: string;
}

interface TurnResponse {
  readonly correlation_id: string;
  readonly output: string;
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
let selections: ModelSelections;
const turns = new Serial();

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
  selections = await ModelSelections.open(join(privateStateDirectory, "model-selection.json"));

  const server = createServer((request, response) => {
    void route(request, response).catch((failure: unknown) => {
      if (failure instanceof ModelRejection) {
        writeJson(response, failure.status, { error: failure.message });
        return;
      }
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
    const controller = new AbortController();
    const disconnect = () => {
      if (!response.writableFinished) controller.abort();
    };
    response.on("close", disconnect);
    try {
      if (response.destroyed) controller.abort();
      const result = await turns.run(() => executeTurn(parsed, controller.signal));
      writeJson(response, 200, result);
    } finally {
      response.off("close", disconnect);
    }
    return;
  }

  const url = new URL(request.url ?? "/", "http://localhost");
  if (request.method === "GET" && url.pathname === "/v1/model") {
    let query: ModelQuery;
    try {
      query = Parse(ModelQuerySchema, Object.fromEntries(url.searchParams));
    } catch {
      writeJson(response, 400, { error: "invalid model query" });
      return;
    }
    writeJson(response, 200, selections.state(await availableModels(), runtimeDefault(), query));
    return;
  }
  if (request.method === "PUT" && url.pathname === "/v1/model") {
    let update: ModelUpdate;
    try {
      update = Parse(ModelUpdateSchema, await readJson(request));
    } catch {
      writeJson(response, 400, { error: "invalid model update" });
      return;
    }
    // Not queued behind turns: a running turn resolved its selection when it
    // started, and the next turn resolves after this change is written.
    writeJson(
      response,
      200,
      await selections.update(await availableModels(), runtimeDefault(), update),
    );
    return;
  }

  writeJson(response, 404, { error: "not found" });
}

/** The thinking levels a Pi model accepts, offered as one option. */
const THINKING_OPTION = "thinking";

async function availableModels(): Promise<readonly AvailableModel[]> {
  return deterministic ? DETERMINISTIC_MODELS : offeredPiModels(await availablePiModels());
}

/** Pi's catalogue as the picker sees it: one thinking setting per reasoning model. */
function offeredPiModels(models: readonly Model<Api>[]): readonly AvailableModel[] {
  return models.map((model) => ({
    provider: model.provider,
    model: model.id,
    name: model.name,
    options: model.reasoning
      ? [
          {
            id: THINKING_OPTION,
            name: "Thinking",
            choices: getSupportedThinkingLevels(model),
            default: defaultThinkingLevel(model),
          },
        ]
      : [],
  }));
}

/** What runs when neither the agent nor the conversation chose a model: Pi's own setting. */
function runtimeDefault(): Selection {
  if (deterministic) return DETERMINISTIC_DEFAULT;
  const provider = privateSettings.getDefaultProvider();
  const model = privateSettings.getDefaultModel();
  if (provider === undefined || model === undefined) throw new Error("no Pi model is selected");
  return { provider, model, options: {} };
}

function defaultThinkingLevel(model: Model<Api>): ModelThinkingLevel {
  return clampThinkingLevel(
    model,
    privateSettings.getModelThinkingLevel(model.provider, model.id) ??
      privateSettings.getDefaultThinkingLevel() ??
      "medium",
  );
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
  return piModel(await availablePiModels(), runtimeDefault());
}

function piModel(models: readonly Model<Api>[], selection: Selection): Model<Api> {
  const model = models.find(
    (candidate) => candidate.provider === selection.provider && candidate.id === selection.model,
  );
  if (model === undefined)
    throw new Error(`selected Pi model ${selection.provider}/${selection.model} is not available`);
  return model;
}

async function executeTurn(request: TurnRequest, signal: AbortSignal): Promise<TurnResponse> {
  signal.throwIfAborted();
  return deterministic ? executeDeterministic(request) : executePi(request, signal);
}

async function executeDeterministic(request: TurnRequest): Promise<TurnResponse> {
  // Resolved first, as in `executePi`: a change made while the turn runs
  // applies to the next one.
  const selection = selections.effective(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, request);
  const directory = conversationDirectory(sessionDirectory, request);
  await mkdir(directory, { recursive: true });
  const sessionPath = join(directory, "deterministic.json");
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
    `model ${describeSelection(selection)}`,
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
  };
}

async function executePi(request: TurnRequest, signal: AbortSignal): Promise<TurnResponse> {
  const root = resolve(request.resource_root);
  const manager = openConversation(config.workspace, sessionDirectory, request);
  // The effective selection is applied to every turn, so a continued session
  // follows the current choice rather than the model it began with. It is
  // resolved against the same catalogue the picker reports, so the thinking
  // level that runs is the one shown.
  const models = await availablePiModels();
  const selection = selections.effective(offeredPiModels(models), runtimeDefault(), request);
  const model = piModel(models, selection);
  const thinking = selection.options?.[THINKING_OPTION];
  const runtime = modelRuntime;
  if (runtime === undefined) throw new Error("Pi model runtime is unavailable");
  const created = await createPiExecutionSession({
    resourceRoot: root,
    workspace: config.workspace,
    manager,
    modelRuntime: runtime,
    model,
    ...(isThinkingLevel(model, thinking) ? { thinkingLevel: thinking } : {}),
  });
  const { session, extensionsResult } = created;
  try {
    assertExtensionsLoaded(extensionsResult.errors);
    let output = "";
    const unsubscribe = session.subscribe((event: AgentSessionEvent) => {
      if (event.type === "message_update" && event.assistantMessageEvent.type === "text_delta")
        output += event.assistantMessageEvent.delta;
    });
    try {
      await promptSession(session, request.input, signal);
      return {
        correlation_id: request.correlation_id,
        output,
      };
    } finally {
      unsubscribe();
    }
  } finally {
    session.dispose();
  }
}

function isThinkingLevel(
  model: Model<Api>,
  value: string | undefined,
): value is ModelThinkingLevel {
  return (
    value !== undefined &&
    getSupportedThinkingLevels(model).some((level: string) => level === value)
  );
}

function assertExtensionsLoaded(errors: readonly { readonly error: string }[]): void {
  if (errors.length > 0)
    throw new Error(
      `Pi extension loading failed: ${errors.map((entry) => entry.error).join("; ")}`,
    );
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

await startRuntime(process.argv.slice(2));
