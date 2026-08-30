import {
  clampThinkingLevel,
  getSupportedThinkingLevels,
  type Api,
  type Model,
  type ModelThinkingLevel,
} from "@earendil-works/pi-ai";

export type ServiceTier = "standard" | "fast";

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
    service_tiers: [],
  };
}

export function profileEvidence<TApi extends Api>(
  model: Model<TApi>,
  requestedReasoning: ModelThinkingLevel | undefined,
): ExecutionProfileEvidence & { readonly effective: ExecutionProfile } {
  const modelReference = `${model.provider}/${model.id}`;
  const effectiveReasoning =
    requestedReasoning === undefined ? undefined : clampThinkingLevel(model, requestedReasoning);
  return {
    requested: {
      model: modelReference,
      ...(requestedReasoning === undefined ? {} : { reasoning: requestedReasoning }),
    },
    effective: {
      model: modelReference,
      ...(effectiveReasoning === undefined ? {} : { reasoning: effectiveReasoning }),
    },
  };
}
