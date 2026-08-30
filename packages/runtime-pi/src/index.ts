import {
  createAgentSession,
  DefaultResourceLoader,
  defineTool,
  ModelRuntime,
  SessionManager,
  SettingsManager,
  type AgentSessionEvent,
} from "@earendil-works/pi-coding-agent";
import { createHash, randomUUID, timingSafeEqual } from "node:crypto";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { basename, isAbsolute, join, relative, resolve } from "node:path";
import { parse as parseToml } from "smol-toml";
import { Type } from "typebox";
import { Parse } from "typebox/value";

interface RuntimeConfig {
  readonly id: string;
  readonly version: string;
  readonly url: string;
}

interface Config {
  readonly root: string;
  readonly dataDir: string;
  readonly resourceRoot: string;
  readonly runtime: RuntimeConfig;
}

interface ResourceSnapshot {
  readonly root: string;
  readonly system_sha256: string;
  readonly context_sha256: string;
  readonly settings_sha256: string;
  readonly extensions_sha256: string;
  readonly skills_sha256: string;
}

interface SelectedComponent {
  readonly id: string;
  readonly role: string;
  readonly version: string;
  readonly url: string;
}

interface CapabilitySelection {
  readonly component: SelectedComponent;
  readonly token: string;
}

interface TurnRequest {
  readonly correlation_id: string;
  readonly agent_id: string;
  readonly audience_id: string;
  readonly input: string;
  readonly continuation_ref?: string | null;
  readonly resources: ResourceSnapshot;
  readonly capability?: CapabilitySelection | null;
}

interface TurnResponse {
  readonly correlation_id: string;
  readonly output: string;
  readonly continuation_ref: string;
  readonly native: unknown;
  readonly capability_activity: readonly unknown[];
}
interface Evidence {
  readonly correlation_id: string;
  readonly state: "processing" | "completed" | "interrupted";
  readonly engine?: "deterministic" | "pi";
  readonly continuation_ref?: string;
  readonly native?: unknown;
  readonly result?: TurnResponse;
  readonly error?: string;
}

interface CapabilityResponse {
  readonly output: string;
  readonly native: unknown;
}
interface DeterministicSession {
  readonly turns: readonly {
    readonly correlation_id: string;
    readonly input: string;
    readonly output: string;
  }[];
}

const ResourceSnapshotSchema = Type.Object({
  root: Type.String(),
  system_sha256: Type.String(),
  context_sha256: Type.String(),
  settings_sha256: Type.String(),
  extensions_sha256: Type.String(),
  skills_sha256: Type.String(),
});
const ComponentSchema = Type.Object({
  id: Type.String(),
  role: Type.String(),
  version: Type.String(),
  url: Type.String(),
});
const CapabilitySelectionSchema = Type.Object({
  component: ComponentSchema,
  token: Type.String(),
});
const TurnRequestSchema = Type.Object({
  correlation_id: Type.String(),
  agent_id: Type.String(),
  audience_id: Type.String(),
  input: Type.String(),
  continuation_ref: Type.Optional(Type.Union([Type.String(), Type.Null()])),
  resources: ResourceSnapshotSchema,
  capability: Type.Optional(Type.Union([CapabilitySelectionSchema, Type.Null()])),
});
const TurnResponseSchema = Type.Object({
  correlation_id: Type.String(),
  output: Type.String(),
  continuation_ref: Type.String(),
  native: Type.Unknown(),
  capability_activity: Type.Array(Type.Unknown()),
});
const EvidenceSchema = Type.Object({
  correlation_id: Type.String(),
  state: Type.Union([
    Type.Literal("processing"),
    Type.Literal("completed"),
    Type.Literal("interrupted"),
  ]),
  engine: Type.Optional(Type.Union([Type.Literal("deterministic"), Type.Literal("pi")])),
  continuation_ref: Type.Optional(Type.String()),
  native: Type.Optional(Type.Unknown()),
  result: Type.Optional(TurnResponseSchema),
  error: Type.Optional(Type.String()),
});
const CapabilityResponseSchema = Type.Object({
  output: Type.String(),
  native: Type.Unknown(),
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
  agent: Type.Object({ resource_root: Type.String() }),
  runtime: Type.Object({ id: Type.String(), version: Type.String(), url: Type.String() }),
});
const NativeExecutionSchema = Type.Object({ runtime_instance: Type.String() });
const SessionEntrySchema = Type.Object({
  type: Type.String(),
  customType: Type.Optional(Type.String()),
  data: Type.Optional(Type.Unknown()),
  message: Type.Optional(Type.Unknown()),
});
const CorrelationMarkerSchema = Type.Object({ correlation_id: Type.String() });
const SessionEntriesSchema = Type.Array(SessionEntrySchema);
const AssistantMessageSchema = Type.Object({
  role: Type.Literal("assistant"),
  content: Type.Array(Type.Unknown()),
  stopReason: Type.String(),
  errorMessage: Type.Optional(Type.String()),
});
const TextBlockSchema = Type.Object({ type: Type.Literal("text"), text: Type.String() });

const args = parseArguments(process.argv.slice(2));
const config = await loadConfig(requiredArgument(args, "config"));
const token = (await readFile(requiredArgument(args, "token-file"), "utf8")).trim();
const deterministic = args.has("deterministic");
const provider = args.get("provider");
const providerKeyFile = args.get("provider-key-file");
if ((provider === undefined) !== (providerKeyFile === undefined))
  throw new Error("--provider and --provider-key-file must be supplied together");
const providerKey =
  providerKeyFile === undefined ? undefined : (await readFile(providerKeyFile, "utf8")).trim();
const evidenceDirectory = join(config.dataDir, "components", config.runtime.id, "evidence");
const sessionDirectory = join(config.resourceRoot, "sessions");
await mkdir(evidenceDirectory, { recursive: true });
await mkdir(sessionDirectory, { recursive: true });
const runtimeInstance = randomUUID();

let queue: Promise<void> = Promise.resolve();

const server = createServer((request, response) => {
  void route(request, response).catch((failure: unknown) => {
    const message = errorMessage(failure);
    writeJson(response, 500, { error: message });
  });
});

const listenUrl = new URL(config.runtime.url);
server.listen(Number(listenUrl.port), listenUrl.hostname, () => {
  process.stdout.write(
    `runtime-pi ready at ${config.runtime.url}${deterministic ? " (deterministic provider substitute)" : ""}\n`,
  );
});

async function route(request: IncomingMessage, response: ServerResponse): Promise<void> {
  if (request.method === "GET" && request.url === "/health") {
    writeJson(response, 200, {
      id: config.runtime.id,
      role: "runtime-adapter",
      version: config.runtime.version,
      status: "ready",
      features: [
        "native-continuations",
        "native-extensions",
        "skills",
        "capability-tools",
        "durable-evidence",
      ],
      provider: deterministic ? "deterministic-local-substitute" : "pi",
    });
    return;
  }
  if (!authorized(request)) {
    writeJson(response, 403, { error: "component authentication failed" });
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
  const evidenceMatch =
    request.method === "GET" ? /^\/v1\/evidence\/([A-Za-z0-9._-]+)$/.exec(request.url ?? "") : null;
  if (evidenceMatch?.[1] !== undefined) {
    const evidence = await reconcileEvidence(await readEvidence(evidenceMatch[1]));
    if (evidence === undefined) {
      writeJson(response, 404, { error: "evidence not found" });
    } else {
      writeJson(response, 200, evidence);
    }
    return;
  }
  writeJson(response, 404, { error: "not found" });
}

async function executeTurn(request: TurnRequest): Promise<TurnResponse> {
  const existing = await readEvidence(request.correlation_id);
  if (existing?.state === "completed" && existing.result !== undefined) return existing.result;
  if (existing !== undefined)
    throw new Error(`turn ${request.correlation_id} has non-terminal runtime evidence`);
  await writeEvidence({ correlation_id: request.correlation_id, state: "processing" });
  try {
    const result = deterministic ? await executeDeterministic(request) : await executePi(request);
    await writeEvidence({ correlation_id: request.correlation_id, state: "completed", result });
    return result;
  } catch (failure: unknown) {
    const current = await readEvidence(request.correlation_id);
    await writeEvidence({
      ...current,
      correlation_id: request.correlation_id,
      state: "interrupted",
      error: errorMessage(failure),
    });
    throw failure;
  }
}

async function executeDeterministic(request: TurnRequest): Promise<TurnResponse> {
  const sessionPath = continuationPath(request);
  await writeEvidence({
    correlation_id: request.correlation_id,
    state: "processing",
    engine: "deterministic",
    continuation_ref: sessionPath,
    native: {
      runtime_instance: runtimeInstance,
      resource_snapshot: toJsonResource(request.resources),
    },
  });
  const prior = await readDeterministicSession(sessionPath);
  const capabilityActivity: unknown[] = [];
  let capabilityText = "";
  if (
    request.capability !== null &&
    request.capability !== undefined &&
    /capability|additions|echo/iu.test(request.input)
  ) {
    const capability = await callCapability(
      request.capability,
      request.correlation_id,
      request.input,
    );
    capabilityText = ` ${capability.output}.`;
    capabilityActivity.push(capability.native);
  }
  const extensionActive = await exists(
    join(request.resources.root, "extensions", "carillon-context.js"),
  );
  const skillActive = await exists(
    join(request.resources.root, "skills", "carillon-status", "SKILL.md"),
  );
  const markers = [
    extensionActive ? "native extension active" : "native extension inactive",
    skillActive ? "skill active" : "skill inactive",
  ];
  const output = `Local provider response ${String(prior.turns.length + 1)}: ${request.input}. ${markers.join("; ")}.${capabilityText}`;
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
    native: {
      owner: "runtime-pi",
      provider: "deterministic-local-substitute",
      message_count: next.turns.length * 2,
      resource_snapshot: toJsonResource(request.resources),
    },
    capability_activity: capabilityActivity,
  };
}

async function executePi(request: TurnRequest): Promise<TurnResponse> {
  const root = resolve(request.resources.root);
  const systemPrompt = await readFile(join(root, "SYSTEM.md"), "utf8");
  const settingsManager = SettingsManager.create(root, root);
  const loader = new DefaultResourceLoader({
    cwd: root,
    agentDir: root,
    settingsManager,
    systemPromptOverride: () => systemPrompt,
  });
  await loader.reload();
  const modelRuntime = await ModelRuntime.create({
    authPath: join(root, "auth.json"),
    modelsPath: join(root, "models.json"),
  });
  if (provider !== undefined && providerKey !== undefined)
    await modelRuntime.setRuntimeApiKey(provider, providerKey);
  const capabilityActivity: unknown[] = [];
  const capability = request.capability;
  const customTools =
    capability === null || capability === undefined
      ? []
      : [
          defineTool({
            name: "carillon_echo",
            label: "Carillon Echo",
            description: "Use the independently supervised harmless echo capability.",
            parameters: Type.Object({ input: Type.String() }),
            execute: async (_toolCallId, parameters) => {
              const result = await callCapability(
                capability,
                request.correlation_id,
                parameters.input,
              );
              capabilityActivity.push(result.native);
              return {
                content: [{ type: "text" as const, text: result.output }],
                details: result.native,
              };
            },
          }),
        ];
  const manager =
    request.continuation_ref === null || request.continuation_ref === undefined
      ? SessionManager.create(root, sessionDirectory)
      : SessionManager.open(validateContinuation(request.continuation_ref));
  const { session, extensionsResult } = await createAgentSession({
    cwd: root,
    agentDir: root,
    modelRuntime,
    resourceLoader: loader,
    sessionManager: manager,
    settingsManager,
    customTools,
  });
  if (extensionsResult.errors.length > 0) {
    session.dispose();
    throw new Error(
      `Pi extension loading failed: ${extensionsResult.errors.map((entry) => entry.error).join("; ")}`,
    );
  }
  session.sessionManager.appendCustomEntry("carillon-turn", {
    correlation_id: request.correlation_id,
    resource_snapshot: toJsonResource(request.resources),
  });
  const sessionFile = session.sessionFile;
  if (sessionFile === undefined) throw new Error("Pi did not create durable session evidence");
  await writeEvidence({
    correlation_id: request.correlation_id,
    state: "processing",
    engine: "pi",
    continuation_ref: sessionFile,
    native: {
      runtime_instance: runtimeInstance,
      session_id: session.sessionId,
      resource_snapshot: toJsonResource(request.resources),
    },
  });
  let output = "";
  const unsubscribe = session.subscribe((event: AgentSessionEvent) => {
    if (event.type === "message_update" && event.assistantMessageEvent.type === "text_delta")
      output += event.assistantMessageEvent.delta;
  });
  try {
    await session.prompt(request.input);
    session.sessionManager.appendCustomEntry("carillon-turn-complete", {
      correlation_id: request.correlation_id,
    });
    return {
      correlation_id: request.correlation_id,
      output,
      continuation_ref: sessionFile,
      native: {
        owner: "runtime-pi",
        provider: "pi",
        session_id: session.sessionId,
        session_file: sessionFile,
        message_count: session.messages.length,
        resource_snapshot: toJsonResource(request.resources),
      },
      capability_activity: capabilityActivity,
    };
  } finally {
    unsubscribe();
    session.dispose();
  }
}

async function callCapability(
  selection: CapabilitySelection,
  correlationId: string,
  input: string,
): Promise<CapabilityResponse> {
  const response = await fetch(`${selection.component.url}/v1/execute`, {
    method: "POST",
    headers: { authorization: `Bearer ${selection.token}`, "content-type": "application/json" },
    body: JSON.stringify({ correlation_id: correlationId, operation: "echo", input }),
  });
  if (!response.ok)
    throw new Error(
      `capability ${selection.component.id} returned HTTP ${String(response.status)}`,
    );
  return Parse(CapabilityResponseSchema, await response.json());
}

function continuationPath(request: TurnRequest): string {
  if (request.continuation_ref !== null && request.continuation_ref !== undefined)
    return validateContinuation(request.continuation_ref);
  return join(
    sessionDirectory,
    `${safeName(request.agent_id)}--${safeName(request.audience_id)}.json`,
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

async function readEvidence(correlationId: string): Promise<Evidence | undefined> {
  try {
    const content = await readFile(
      join(evidenceDirectory, `${safeName(correlationId)}.json`),
      "utf8",
    );
    return await Parse(EvidenceSchema, JSON.parse(content));
  } catch (failure: unknown) {
    if (isNodeError(failure) && failure.code === "ENOENT") return undefined;
    throw failure;
  }
}

async function reconcileEvidence(evidence: Evidence | undefined): Promise<Evidence | undefined> {
  if (
    evidence?.state !== "processing" ||
    evidence.engine === undefined ||
    evidence.continuation_ref === undefined
  )
    return evidence;

  let owner: string | undefined;
  try {
    owner = Parse(NativeExecutionSchema, evidence.native).runtime_instance;
  } catch {
    owner = undefined;
  }
  const currentExecution = owner === runtimeInstance;
  if (evidence.engine === "deterministic") {
    const session = await readDeterministicSession(validateContinuation(evidence.continuation_ref));
    const turn = session.turns.find(
      (candidate) => candidate.correlation_id === evidence.correlation_id,
    );
    if (turn !== undefined) {
      const completed: Evidence = {
        ...evidence,
        state: "completed",
        result: {
          correlation_id: evidence.correlation_id,
          output: turn.output,
          continuation_ref: validateContinuation(evidence.continuation_ref),
          native: {
            owner: "runtime-pi",
            provider: "deterministic-local-substitute",
            reconciled: true,
            prior_evidence: evidence.native,
          },
          capability_activity: [],
        },
      };
      await writeEvidence(completed);
      return completed;
    }
  } else {
    const manager = SessionManager.open(validateContinuation(evidence.continuation_ref));
    const entries = Parse(SessionEntriesSchema, manager.getEntries());
    let markerIndex = -1;
    for (const [index, entry] of entries.entries()) {
      if (entry.type !== "custom" || entry.customType !== "carillon-turn") continue;
      try {
        const marker = Parse(CorrelationMarkerSchema, entry.data);
        if (marker.correlation_id === evidence.correlation_id) markerIndex = index;
      } catch {
        // Ignore another extension's malformed custom entry.
      }
    }
    if (markerIndex >= 0) {
      let output = "";
      let stopReason: string | undefined;
      let error: string | undefined;
      for (const entry of entries.slice(markerIndex + 1)) {
        if (entry.type !== "message" || entry.message === undefined) continue;
        try {
          const message = Parse(AssistantMessageSchema, entry.message);
          stopReason = message.stopReason;
          error = message.errorMessage;
          for (const content of message.content) {
            try {
              output += Parse(TextBlockSchema, content).text;
            } catch {
              // Non-text assistant content is native evidence, not Discord output.
            }
          }
        } catch {
          // User, tool, and custom messages are not terminal assistant evidence.
        }
      }
      if (stopReason !== undefined && stopReason !== "toolUse") {
        if (stopReason === "error" || stopReason === "aborted") {
          const interrupted: Evidence = {
            ...evidence,
            state: "interrupted",
            error: error ?? `Pi stopped with ${stopReason}`,
          };
          await writeEvidence(interrupted);
          return interrupted;
        }
        const completed: Evidence = {
          ...evidence,
          state: "completed",
          result: {
            correlation_id: evidence.correlation_id,
            output,
            continuation_ref: validateContinuation(evidence.continuation_ref),
            native: {
              owner: "runtime-pi",
              provider: "pi",
              reconciled: true,
              message_count: entries.length,
              prior_evidence: evidence.native,
            },
            capability_activity: [],
          },
        };
        await writeEvidence(completed);
        return completed;
      }
    }
  }

  if (currentExecution) return evidence;
  const interrupted: Evidence = {
    ...evidence,
    state: "interrupted",
    error: "runtime restarted without terminal native evidence",
  };
  await writeEvidence(interrupted);
  return interrupted;
}

async function writeEvidence(evidence: Evidence): Promise<void> {
  await atomicJson(join(evidenceDirectory, `${safeName(evidence.correlation_id)}.json`), evidence);
}

async function atomicJson(path: string, value: unknown): Promise<void> {
  const temporary = `${path}.${String(process.pid)}.tmp`;
  await writeFile(temporary, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
  await rename(temporary, path);
}

async function loadConfig(path: string): Promise<Config> {
  const root = resolve(path, "..");
  const parsed = Parse(FileConfigSchema, parseToml(await readFile(path, "utf8")));
  return {
    root,
    dataDir: resolve(root, parsed.data_dir),
    resourceRoot: resolve(root, parsed.agent.resource_root),
    runtime: parsed.runtime,
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

function toJsonResource(value: ResourceSnapshot): Readonly<Record<string, string>> {
  return {
    root: value.root,
    system_sha256: value.system_sha256,
    context_sha256: value.context_sha256,
    settings_sha256: value.settings_sha256,
    extensions_sha256: value.extensions_sha256,
    skills_sha256: value.skills_sha256,
  };
}
