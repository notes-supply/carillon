import {
  defineTool,
  ModelRuntime,
  SessionManager,
  SettingsManager,
  type AgentSessionEvent,
} from "@earendil-works/pi-coding-agent";
import type { ModelThinkingLevel } from "@earendil-works/pi-ai";
import { registerBunOAuthFlows } from "@earendil-works/pi-ai/bun-oauth";
import { createHash, randomUUID, timingSafeEqual } from "node:crypto";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { mkdir, readFile } from "node:fs/promises";
import { basename, isAbsolute, join, relative, resolve } from "node:path";
import { parse as parseToml } from "smol-toml";
import { Type } from "typebox";
import { Parse } from "typebox/value";
import {
  applyPiSessionProfile,
  type ExecutionProfile,
  ExecutionProfileSchema,
  type ExecutionProfileEvidence,
  latestExecutionProfile,
  loadExecutionProfileDefault,
  persistExecutionProfile,
  profileEvidence,
  profileModel,
  reasoningForModel,
  type RuntimeExecutionProfileModel,
  saveExecutionProfileDefault,
  type ServiceTier,
} from "./profile.ts";
import { atomicJson } from "./atomic-json.ts";
import { callCapability, type CapabilitySelection } from "./capability.ts";
import { capabilityCallId, nativeSessionId } from "./namespace.ts";
import { serialize } from "./serial.ts";
import { createPiExecutionSession, recoverPiTurn } from "./session.ts";

registerBunOAuthFlows();

interface RuntimeProviderConfig {
  readonly id: string;
  readonly version: string;
  readonly url: string;
}

interface Config {
  readonly root: string;
  readonly dataDir: string;
  readonly resourceRoot: string;
  readonly workspace: string;
  readonly runtime: RuntimeProviderConfig;
}

interface ResourceSnapshot {
  readonly root: string;
  readonly system_sha256: string;
  readonly context_sha256: string;
  readonly settings_sha256: string;
  readonly extensions_sha256: string;
  readonly skills_sha256: string;
}

interface TurnRequest {
  readonly correlation_id: string;
  readonly interface_id: string;
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
interface RuntimeExecutionProfiles {
  readonly models: readonly RuntimeExecutionProfileModel[];
  readonly selected?: ExecutionProfileEvidence;
}
interface RuntimeExecutionProfileSelection {
  readonly key: string;
  readonly reasoning?: ModelThinkingLevel;
  readonly service_tier?: ServiceTier;
  readonly scope: "current_conversation" | "new_conversations";
  readonly continuation_ref?: string | null;
  readonly interface_id?: string | null;
  readonly agent_id: string;
  readonly audience_id?: string | null;
  readonly resources?: ResourceSnapshot | null;
}
interface ExecutionProfileApplication {
  readonly scope: RuntimeExecutionProfileSelection["scope"];
  readonly requested: ExecutionProfile;
  readonly effective: ExecutionProfile;
  readonly continuation_ref?: string;
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

interface DeterministicSession {
  readonly profile?: ExecutionProfile;
  readonly turns: readonly {
    readonly correlation_id: string;
    readonly input: string;
    readonly output: string;
  }[];
}

const DETERMINISTIC_MODELS: readonly RuntimeExecutionProfileModel[] = [
  {
    key: modelKey("deterministic", "steady"),
    provider: "deterministic",
    provider_name: "Deterministic",
    id: "steady",
    name: "Steady",
    reasoning: [],
    service_tiers: [],
  },
  {
    key: modelKey("deterministic", "concise"),
    provider: "deterministic",
    provider_name: "Deterministic",
    id: "concise",
    name: "Concise",
    reasoning: [],
    service_tiers: [],
  },
];

const ResourceSnapshotSchema = Type.Object({
  root: Type.String(),
  system_sha256: Type.String(),
  context_sha256: Type.String(),
  settings_sha256: Type.String(),
  extensions_sha256: Type.String(),
  skills_sha256: Type.String(),
});
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
});
const CapabilitySelectionSchema = Type.Object({
  provider: ProviderSchema,
  token: Type.String(),
});
const TurnRequestSchema = Type.Object({
  correlation_id: Type.String(),
  interface_id: ProviderIdentitySchema,
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
const ThinkingLevelSchema = Type.Union([
  Type.Literal("off"),
  Type.Literal("minimal"),
  Type.Literal("low"),
  Type.Literal("medium"),
  Type.Literal("high"),
  Type.Literal("xhigh"),
  Type.Literal("max"),
]);
const ServiceTierSchema = Type.Union([Type.Literal("standard"), Type.Literal("fast")]);
const RuntimeExecutionProfileSelectionSchema = Type.Object({
  key: Type.String(),
  reasoning: Type.Optional(ThinkingLevelSchema),
  service_tier: Type.Optional(ServiceTierSchema),
  scope: Type.Union([Type.Literal("current_conversation"), Type.Literal("new_conversations")]),
  continuation_ref: Type.Optional(Type.Union([Type.String(), Type.Null()])),
  interface_id: Type.Optional(Type.Union([ProviderIdentitySchema, Type.Null()])),
  agent_id: Type.String(),
  audience_id: Type.Optional(Type.Union([Type.String(), Type.Null()])),
  resources: Type.Optional(Type.Union([ResourceSnapshotSchema, Type.Null()])),
});

const DeterministicSessionSchema = Type.Object({
  profile: Type.Optional(ExecutionProfileSchema),
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
const NativeExecutionSchema = Type.Object({ runtime_instance: Type.String() });
const CorrelationMarkerSchema = Type.Object({ correlation_id: Type.String() });

let config: Config;
let token: string;
let deterministic: boolean;
let evidenceDirectory: string;
let sessionDirectory: string;
let executionProfileDefaultPath: string;

let privateSettings: SettingsManager;
let modelRuntime: ModelRuntime | undefined;
let runtimeInstance: string;
let defaultExecutionProfile: ExecutionProfile | undefined;
const activeTurns = new Set<string>();

async function startRuntime(values: readonly string[]): Promise<void> {
  const args = parseArguments(values);
  const providerId = requiredArgument(args, "provider");
  config = await loadConfig(requiredArgument(args, "config"), providerId);
  token = (await readFile(requiredArgument(args, "token-file"), "utf8")).trim();
  deterministic = args.has("deterministic");
  const privateStateDirectory = resolve(requiredArgument(args, "state-dir"));
  executionProfileDefaultPath = join(privateStateDirectory, "execution-profile.json");
  evidenceDirectory = join(config.dataDir, "components", config.runtime.id, "evidence");
  sessionDirectory = join(config.resourceRoot, "sessions");

  await mkdir(evidenceDirectory, { recursive: true });
  await mkdir(sessionDirectory, { recursive: true });
  await mkdir(privateStateDirectory, { recursive: true, mode: 0o700 });
  defaultExecutionProfile = await loadExecutionProfileDefault(executionProfileDefaultPath);
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
  runtimeInstance = randomUUID();

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
}

async function route(request: IncomingMessage, response: ServerResponse): Promise<void> {
  if (request.method === "GET" && request.url === "/health") {
    writeJson(response, 200, {
      id: config.runtime.id,
      role: "runtime",
      version: config.runtime.version,
      status: "ready",
      features: [
        "native-continuations",
        "native-extensions",
        "skills",
        "capability-tools",
        "durable-evidence",
        "execution-profile-discovery",
        "execution-profile-selection",
      ],
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
  if (request.method === "GET" && request.url === "/v1/execution-profiles") {
    const signal = requestSignal(response);
    writeJson(response, 200, await serialize(() => listExecutionProfiles(signal), signal));
    return;
  }
  if (request.method === "POST" && request.url === "/v1/execution-profiles/refresh") {
    const signal = requestSignal(response);
    const result = await serialize(() => refreshExecutionProfiles(signal), signal);
    writeJson(response, 200, result);
    return;
  }
  if (request.method === "PUT" && request.url === "/v1/execution-profile") {
    let selection: RuntimeExecutionProfileSelection;
    try {
      selection = Parse(RuntimeExecutionProfileSelectionSchema, await readJson(request));
    } catch {
      writeJson(response, 400, { error: "invalid execution-profile selection" });
      return;
    }
    try {
      const signal = requestSignal(response);
      const result = await serialize(() => applyExecutionProfile(selection, signal), signal);
      if (result === undefined) {
        writeJson(response, 400, { error: "model is not available" });
      } else {
        writeJson(response, 200, result);
      }
    } catch (failure: unknown) {
      writeJson(response, 400, { error: errorMessage(failure) });
    }
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
    const result = await queueTurn(parsed);
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

async function listExecutionProfiles(signal?: AbortSignal): Promise<RuntimeExecutionProfiles> {
  signal?.throwIfAborted();
  if (deterministic) {
    const selectedModel = DETERMINISTIC_MODELS.find((model) => model.key === selectedKey());
    const selected =
      selectedModel === undefined
        ? unavailableProfileEvidence()
        : {
            requested: { model: `${selectedModel.provider}/${selectedModel.id}` },
            effective: { model: `${selectedModel.provider}/${selectedModel.id}` },
          };
    return { models: DETERMINISTIC_MODELS, ...(selected === undefined ? {} : { selected }) };
  }
  const piModels = await availablePiModels();
  signal?.throwIfAborted();
  const models = piModels.map((model) =>
    profileModel(model, modelKey(model.provider, model.id), providerName(model.provider)),
  );
  const selectedModel = piModels.find(
    (model) => modelKey(model.provider, model.id) === selectedKey(),
  );
  if (selectedModel === undefined) {
    const selected = unavailableProfileEvidence();
    return { models, ...(selected === undefined ? {} : { selected }) };
  }
  const modelReference = `${selectedModel.provider}/${selectedModel.id}`;
  const persistedDefault =
    defaultExecutionProfile?.model === modelReference ? defaultExecutionProfile : undefined;
  const requestedReasoning =
    persistedDefault?.reasoning ?? reasoningForModel(privateSettings, selectedModel, undefined);
  return {
    models,
    selected: profileEvidence(selectedModel, requestedReasoning, persistedDefault?.service_tier),
  };
}

async function refreshExecutionProfiles(signal: AbortSignal): Promise<RuntimeExecutionProfiles> {
  if (modelRuntime !== undefined) {
    await modelRuntime.refresh({
      allowNetwork: true,
      force: true,
      signal: AbortSignal.any([signal, AbortSignal.timeout(15_000)]),
    });
  }
  return listExecutionProfiles(signal);
}

async function applyExecutionProfile(
  selection: RuntimeExecutionProfileSelection,
  signal: AbortSignal,
): Promise<ExecutionProfileApplication | undefined> {
  const profiles = await listExecutionProfiles(signal);
  signal.throwIfAborted();
  const selected = profiles.models.find((model) => model.key === selection.key);
  if (selected === undefined) return undefined;
  if (selection.reasoning !== undefined && !selected.reasoning.includes(selection.reasoning))
    throw new Error(
      `${selected.provider}/${selected.id} does not support ${selection.reasoning} reasoning`,
    );
  if (
    selection.service_tier !== undefined &&
    !selected.service_tiers.includes(selection.service_tier)
  )
    throw new Error(`${selected.provider}/${selected.id} does not support service tiers`);

  if (selection.scope === "current_conversation")
    return deterministic
      ? applyDeterministicConversationProfile(selection, selected, signal)
      : applyPiConversationProfile(selection, selected, signal);

  signal.throwIfAborted();
  const selectedPi = deterministic
    ? undefined
    : await piModelForReference(`${selected.provider}/${selected.id}`);
  const evidence =
    selectedPi === undefined
      ? {
          requested: { model: `${selected.provider}/${selected.id}` },
          effective: { model: `${selected.provider}/${selected.id}` },
        }
      : profileEvidence(
          selectedPi,
          reasoningForModel(privateSettings, selectedPi, selection.reasoning),
          selection.service_tier,
        );
  await saveExecutionProfileDefault(executionProfileDefaultPath, evidence.requested, signal);
  defaultExecutionProfile = evidence.requested;
  return { scope: selection.scope, ...evidence };
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

function providerName(providerId: string): string {
  const provider = modelRuntime?.getProvider(providerId);
  return provider?.auth.oauth?.name ?? provider?.name ?? providerId;
}

async function selectedPiModel() {
  if (defaultExecutionProfile !== undefined)
    return piModelForReference(defaultExecutionProfile.model);
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
function unavailableProfileEvidence(): ExecutionProfileEvidence | undefined {
  if (defaultExecutionProfile !== undefined)
    return { requested: defaultExecutionProfile, effective: null };
  const provider = privateSettings.getDefaultProvider();
  const id = privateSettings.getDefaultModel();
  if (provider === undefined || id === undefined) return undefined;
  const reasoning =
    privateSettings.getModelThinkingLevel(provider, id) ??
    privateSettings.getDefaultThinkingLevel();
  return {
    requested: {
      model: `${provider}/${id}`,
      ...(reasoning === undefined ? {} : { reasoning }),
    },
    effective: null,
  };
}

function selectedKey(): string | undefined {
  if (defaultExecutionProfile !== undefined) {
    const separator = defaultExecutionProfile.model.indexOf("/");
    if (separator > 0)
      return modelKey(
        defaultExecutionProfile.model.slice(0, separator),
        defaultExecutionProfile.model.slice(separator + 1),
      );
  }
  const provider = privateSettings.getDefaultProvider();
  const id = privateSettings.getDefaultModel();
  return provider === undefined || id === undefined ? undefined : modelKey(provider, id);
}

async function applyDeterministicConversationProfile(
  selection: RuntimeExecutionProfileSelection,
  model: RuntimeExecutionProfileModel,
  signal: AbortSignal,
): Promise<ExecutionProfileApplication> {
  const sessionPath = profileContinuationPath(selection);
  const session = await readDeterministicSession(sessionPath);
  const profile = { model: `${model.provider}/${model.id}` };
  signal.throwIfAborted();
  await atomicJson(sessionPath, { ...session, profile });
  return {
    scope: selection.scope,
    requested: profile,
    effective: profile,
    continuation_ref: sessionPath,
  };
}

async function applyPiConversationProfile(
  selection: RuntimeExecutionProfileSelection,
  selected: RuntimeExecutionProfileModel,
  signal: AbortSignal,
): Promise<ExecutionProfileApplication> {
  const resources = selection.resources;
  if (resources === null || resources === undefined)
    throw new Error("current conversation scope needs resources");
  const model = (await availablePiModels()).find(
    (candidate) => candidate.provider === selected.provider && candidate.id === selected.id,
  );
  if (model === undefined) throw new Error("selected Pi model is unavailable");
  const requested = profileEvidence(model, selection.reasoning, selection.service_tier).requested;
  if (selection.continuation_ref === null || selection.continuation_ref === undefined) {
    signal.throwIfAborted();
    const manager = SessionManager.create(config.workspace, sessionDirectory, {
      id: profileNativeSessionId(selection),
    });
    const continuation = persistExecutionProfile(manager, requested);
    return {
      scope: selection.scope,
      requested,
      effective: profileEvidence(model, selection.reasoning, requested.service_tier).effective,
      continuation_ref: continuation,
    };
  }
  const manager = SessionManager.open(
    validateContinuation(selection.continuation_ref),
    sessionDirectory,
    config.workspace,
  );
  const runtime = modelRuntime;
  if (runtime === undefined) throw new Error("Pi model runtime is unavailable");
  const created = await createPiExecutionSession({
    resourceRoot: resolve(resources.root),
    workspace: config.workspace,
    manager,
    ...(requested.service_tier === undefined ? {} : { serviceTier: requested.service_tier }),
    modelRuntime: runtime,
  });
  const { session, extensionsResult } = created;
  try {
    assertExtensionsLoaded(extensionsResult.errors);
    const reasoning = reasoningForModel(privateSettings, model, selection.reasoning);
    await applyPiSessionProfile(session, model, reasoning, signal);
    signal.throwIfAborted();
    const sessionFile = persistExecutionProfile(session.sessionManager, requested);
    const effective = profileEvidence(
      session.model ?? model,
      session.supportsThinking() ? session.thinkingLevel : undefined,
      requested.service_tier,
    ).effective;
    return {
      scope: selection.scope,
      requested,
      effective,
      continuation_ref: sessionFile,
    };
  } finally {
    session.dispose();
  }
}

function profileContinuationPath(selection: RuntimeExecutionProfileSelection): string {
  if (selection.continuation_ref !== null && selection.continuation_ref !== undefined)
    return validateContinuation(selection.continuation_ref);
  return join(
    sessionDirectory,
    `${safeName(requiredProfileInterface(selection))}--${safeName(selection.agent_id)}--${safeName(requiredProfileAudience(selection))}.json`,
  );
}

function profileNativeSessionId(selection: RuntimeExecutionProfileSelection): string {
  return nativeSessionId(
    requiredProfileInterface(selection),
    selection.agent_id,
    requiredProfileAudience(selection),
  );
}

function requiredProfileInterface(selection: RuntimeExecutionProfileSelection): string {
  const interfaceId = selection.interface_id;
  if (interfaceId === null || interfaceId === undefined)
    throw new Error("current conversation scope needs an interface");
  return interfaceId;
}

function requiredProfileAudience(selection: RuntimeExecutionProfileSelection): string {
  const audience = selection.audience_id;
  if (audience === null || audience === undefined || audience.length === 0)
    throw new Error("current conversation scope needs an audience");
  return audience;
}

async function queueTurn(request: TurnRequest): Promise<TurnResponse> {
  if (activeTurns.has(request.correlation_id))
    throw new Error(`turn ${request.correlation_id} is already queued`);
  activeTurns.add(request.correlation_id);
  try {
    const existing = await readEvidence(request.correlation_id);
    if (existing?.state === "completed" && existing.result !== undefined) return existing.result;
    if (existing !== undefined)
      throw new Error(`turn ${request.correlation_id} has non-terminal runtime evidence`);
    await writeEvidence({
      correlation_id: request.correlation_id,
      state: "processing",
      native: { runtime_instance: runtimeInstance, phase: "queued" },
    });
    return await serialize(() => executeTurn(request));
  } finally {
    activeTurns.delete(request.correlation_id);
  }
}

async function executeTurn(request: TurnRequest): Promise<TurnResponse> {
  let result: TurnResponse;
  try {
    result = deterministic ? await executeDeterministic(request) : await executePi(request);
  } catch (failure: unknown) {
    const current = await readEvidence(request.correlation_id);
    if (current?.state === "completed" && current.result !== undefined) return current.result;
    await writeEvidence({
      ...current,
      correlation_id: request.correlation_id,
      state: "interrupted",
      error: errorMessage(failure),
    });
    throw failure;
  }
  await writeEvidence({ correlation_id: request.correlation_id, state: "completed", result });
  return result;
}

async function executeDeterministic(request: TurnRequest): Promise<TurnResponse> {
  const sessionPath = continuationPath(request);
  const prior = await readDeterministicSession(sessionPath);
  const selectedModel = DETERMINISTIC_MODELS.find((model) => {
    const reference = prior.profile?.model;
    return reference === undefined
      ? model.key === selectedKey()
      : `${model.provider}/${model.id}` === reference;
  });
  if (selectedModel === undefined) throw new Error("deterministic model selection is unavailable");
  const executionProfile = {
    requested: { model: `${selectedModel.provider}/${selectedModel.id}` },
    effective: { model: `${selectedModel.provider}/${selectedModel.id}` },
  };
  await writeEvidence({
    correlation_id: request.correlation_id,
    state: "processing",
    engine: "deterministic",
    continuation_ref: sessionPath,
    native: {
      runtime_instance: runtimeInstance,
      execution_profile: executionProfile,
    },
  });
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
    profile: executionProfile.requested,
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
      execution_profile: executionProfile,
      message_count: next.turns.length * 2,
    },
    capability_activity: capabilityActivity,
  };
}

async function executePi(request: TurnRequest): Promise<TurnResponse> {
  const root = resolve(request.resources.root);
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
  const persistedProfile = latestExecutionProfile(manager);
  const defaultModel = isNew
    ? await selectedPiModel()
    : persistedProfile !== undefined && manager.buildSessionContext().model === null
      ? await piModelForReference(persistedProfile.model)
      : undefined;
  const defaultReasoning =
    persistedProfile?.reasoning ??
    (defaultModel === undefined
      ? undefined
      : (defaultExecutionProfile?.reasoning ??
        reasoningForModel(privateSettings, defaultModel, undefined)));
  const selectedDefaultTier = isNew ? defaultExecutionProfile?.service_tier : undefined;
  const initialProfile =
    isNew && defaultModel !== undefined
      ? profileEvidence(defaultModel, defaultReasoning, selectedDefaultTier).requested
      : undefined;
  if (initialProfile !== undefined) persistExecutionProfile(manager, initialProfile);
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
            execute: async (toolCallId, parameters) => {
              const result = await callCapability(
                capability,
                capabilityCallId(request.correlation_id, toolCallId),
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
  const serviceTier = persistedProfile?.service_tier ?? selectedDefaultTier;
  const runtime = modelRuntime;
  if (runtime === undefined) throw new Error("Pi model runtime is unavailable");
  const created = await createPiExecutionSession({
    resourceRoot: root,
    workspace: config.workspace,
    manager,
    modelRuntime: runtime,
    ...(defaultModel === undefined ? {} : { model: defaultModel }),
    ...(defaultReasoning === undefined ? {} : { thinkingLevel: defaultReasoning }),
    ...(serviceTier === undefined ? {} : { serviceTier }),
    customTools,
  });
  const { session, extensionsResult } = created;
  try {
    assertExtensionsLoaded(extensionsResult.errors);
    const model = session.model;
    if (model === undefined) throw new Error("Pi did not resolve a session model");
    const requested =
      persistedProfile ??
      initialProfile ??
      profileEvidence(
        model,
        model.reasoning ? (defaultReasoning ?? session.thinkingLevel) : undefined,
        selectedDefaultTier,
      ).requested;
    const executionProfile = {
      requested,
      effective: profileEvidence(
        model,
        model.reasoning ? session.thinkingLevel : undefined,
        requested.service_tier,
      ).effective,
    };
    if (persistedProfile === undefined && initialProfile === undefined)
      persistExecutionProfile(session.sessionManager, requested);
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
        execution_profile: executionProfile,
      },
    });
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
        native: {
          owner: "runtime-pi",
          provider: "pi",
          execution_profile: executionProfile,
          session_id: session.sessionId,
          session_file: sessionFile,
          message_count: session.messages.length,
        },
        capability_activity: capabilityActivity,
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
  if (evidence?.state !== "processing") return evidence;

  let owner: string | undefined;
  try {
    owner = Parse(NativeExecutionSchema, evidence.native).runtime_instance;
  } catch {
    owner = undefined;
  }
  const currentExecution = owner === runtimeInstance;
  if (currentExecution && activeTurns.has(evidence.correlation_id)) return evidence;
  if (evidence.engine === undefined || evidence.continuation_ref === undefined) {
    if (currentExecution) return evidence;
    const interrupted: Evidence = {
      ...evidence,
      state: "interrupted",
      error: "runtime restarted before queued work began",
    };
    await writeEvidence(interrupted);
    return interrupted;
  }

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
    const entries = manager.getEntries();
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
      const { output, stopReason, error, capabilityActivity } = recoverPiTurn(
        entries.slice(markerIndex + 1),
      );
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
            capability_activity: capabilityActivity,
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

async function loadConfig(path: string, providerId: string): Promise<Config> {
  Parse(ProviderIdentitySchema, providerId);
  const root = resolve(path, "..");
  const parsed = Parse(FileConfigSchema, parseToml(await readFile(path, "utf8")));
  const runtime = parsed.providers.find(
    (provider) => provider.id === providerId && provider.role === "runtime",
  );
  if (runtime === undefined) throw new Error(`runtime provider ${providerId} is not configured`);
  return {
    root,
    dataDir: resolve(root, parsed.data_dir),
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

function requestSignal(response: ServerResponse): AbortSignal {
  const controller = new AbortController();
  response.once("close", () => controller.abort());
  return controller.signal;
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

function modelKey(provider: string, id: string): string {
  return createHash("sha256").update(provider).update("\0").update(id).digest("hex");
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

await startRuntime(process.argv.slice(2));
