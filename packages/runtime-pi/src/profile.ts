import { randomUUID } from "node:crypto";
import { existsSync, writeFileSync } from "node:fs";
import { readFile, rename, rm, writeFile } from "node:fs/promises";
import type { ModelRuntime, SessionManager } from "@earendil-works/pi-coding-agent";
import {
  clampThinkingLevel,
  getSupportedThinkingLevels,
  hasApi,
  type Api,
  type Context,
  type Model,
  type ModelThinkingLevel,
  type ModelsApiStreamOptions,
  type ModelsSimpleStreamOptions,
} from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { Parse } from "typebox/value";

export type ServiceTier = "standard" | "fast";

const CODEX_API = "openai-codex-responses";
const CODEX_SERVICE_TIERS = ["standard", "fast"] as const;
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
export const ExecutionProfileSchema = Type.Object({
  model: Type.String(),
  reasoning: Type.Optional(ThinkingLevelSchema),
  service_tier: Type.Optional(ServiceTierSchema),
});

const ServiceTierDefaultSchema = Type.Object({
  model: Type.String(),
  tier: ServiceTierSchema,
});

export interface RuntimeExecutionProfileModel {
  readonly key: string;
  readonly provider: string;
  readonly provider_name: string;
  readonly id: string;
  readonly name: string;
  readonly reasoning: readonly ModelThinkingLevel[];
  readonly service_tiers: readonly ServiceTier[];
}

export interface ExecutionProfile {
  readonly model: string;
  readonly reasoning?: ModelThinkingLevel;
  readonly service_tier?: ServiceTier;
}

export interface ExecutionProfileEvidence {
  readonly requested: ExecutionProfile;
  readonly effective: ExecutionProfile | null;
}

export interface ServiceTierDefault {
  readonly model: string;
  readonly tier: ServiceTier;
}

export function serviceTierDefault(profile: ExecutionProfile): ServiceTierDefault | undefined {
  return profile.service_tier === undefined
    ? undefined
    : { model: profile.model, tier: profile.service_tier };
}

export function serviceTierForModel(
  selection: ServiceTierDefault | undefined,
  model: string,
): ServiceTier | undefined {
  return selection?.model === model ? selection.tier : undefined;
}

export async function loadServiceTierDefault(
  path: string,
): Promise<ServiceTierDefault | undefined> {
  try {
    return await Parse(ServiceTierDefaultSchema, JSON.parse(await readFile(path, "utf8")));
  } catch (failure: unknown) {
    if (isNodeError(failure) && failure.code === "ENOENT") return undefined;
    throw failure;
  }
}

export async function saveServiceTierDefault(
  path: string,
  selection: ServiceTierDefault | undefined,
): Promise<void> {
  if (selection === undefined) {
    await rm(path, { force: true });
    return;
  }
  const temporary = `${path}.${randomUUID()}.tmp`;
  try {
    await writeFile(temporary, `${JSON.stringify(selection)}\n`, { mode: 0o600 });
    await rename(temporary, path);
  } finally {
    await rm(temporary, { force: true });
  }
}

function isNodeError(value: unknown): value is NodeJS.ErrnoException {
  return value instanceof Error && "code" in value;
}

export function profileModel<TApi extends Api>(
  model: Model<TApi>,
  key: string,
  providerName: string,
): RuntimeExecutionProfileModel {
  return {
    key,
    provider: model.provider,
    provider_name: providerName,
    id: model.id,
    name: model.name,
    reasoning: model.reasoning ? getSupportedThinkingLevels(model) : [],
    service_tiers: model.api === CODEX_API ? CODEX_SERVICE_TIERS : [],
  };
}

export function profileEvidence<TApi extends Api>(
  model: Model<TApi>,
  requestedReasoning: ModelThinkingLevel | undefined,
  requestedServiceTier?: ServiceTier,
): ExecutionProfileEvidence & { readonly effective: ExecutionProfile } {
  const modelReference = `${model.provider}/${model.id}`;
  const effectiveReasoning =
    requestedReasoning === undefined ? undefined : clampThinkingLevel(model, requestedReasoning);
  const serviceTier = model.api === CODEX_API ? (requestedServiceTier ?? "standard") : undefined;
  return {
    requested: {
      model: modelReference,
      ...(requestedReasoning === undefined ? {} : { reasoning: requestedReasoning }),
      ...(serviceTier === undefined ? {} : { service_tier: serviceTier }),
    },
    effective: {
      model: modelReference,
      ...(effectiveReasoning === undefined ? {} : { reasoning: effectiveReasoning }),
      ...(serviceTier === undefined ? {} : { service_tier: serviceTier }),
    },
  };
}

export interface PiServiceTierStreams<TResult> {
  stream(
    model: Model<"openai-codex-responses">,
    context: Context,
    options?: ModelsApiStreamOptions<"openai-codex-responses">,
  ): TResult;
  streamSimple(model: Model<Api>, context: Context, options?: ModelsSimpleStreamOptions): TResult;
}

export function streamSimpleForServiceTier<TResult>(
  runtime: PiServiceTierStreams<TResult>,
  serviceTier: ServiceTier | undefined,
): PiServiceTierStreams<TResult>["streamSimple"] {
  if (serviceTier !== "fast") return runtime.streamSimple.bind(runtime);
  return (model, context, options) => {
    if (!hasApi(model, CODEX_API)) return runtime.streamSimple(model, context, options);
    const { reasoning, ...baseOptions } = options ?? {};
    const clampedReasoning =
      reasoning === undefined ? undefined : clampThinkingLevel(model, reasoning);
    const providerOptions: ModelsApiStreamOptions<"openai-codex-responses"> = {
      ...baseOptions,
      ...(clampedReasoning === undefined || clampedReasoning === "off"
        ? {}
        : { reasoningEffort: clampedReasoning }),
      serviceTier: "priority",
    };
    return runtime.stream(model, context, providerOptions);
  };
}

export function modelRuntimeForServiceTier(
  runtime: ModelRuntime,
  serviceTier: ServiceTier | undefined,
): ModelRuntime {
  if (serviceTier !== "fast") return runtime;
  const streamSimple = streamSimpleForServiceTier(runtime, serviceTier);

  return new Proxy(runtime, {
    get(target, property): unknown {
      if (property === "streamSimple") return streamSimple;
      const value: unknown = Reflect.get(target, property, target);
      return typeof value === "function"
        ? (...arguments_: unknown[]): unknown => Reflect.apply(value, target, arguments_)
        : value;
    },
  });
}

export function latestExecutionProfile(manager: SessionManager): ExecutionProfile | undefined {
  for (const entry of manager.getBranch().toReversed()) {
    if (entry.type === "custom" && entry.customType === "carillon-execution-profile")
      return Parse(ExecutionProfileSchema, entry.data);
  }
  return undefined;
}

export function persistExecutionProfile(
  manager: SessionManager,
  profile: ExecutionProfile,
): string {
  const sessionFile = manager.getSessionFile();
  if (sessionFile === undefined) throw new Error("Pi did not create durable session evidence");
  if (!existsSync(sessionFile)) {
    writeFileSync(sessionFile, "", { flag: "wx", mode: 0o600 });
    manager.setSessionFile(sessionFile);
  }
  manager.appendCustomEntry("carillon-execution-profile", profile);
  return sessionFile;
}
