/**
 * What models the runtime offers and which one a conversation runs. The
 * catalogue is pi-ai's `Models`; Carillon's selections are kept in its own
 * ledger and applied to each conversation as its turns are admitted.
 */
import { ModelRuntime, SettingsManager } from "@earendil-works/pi-coding-agent";
import {
  clampThinkingLevel,
  getSupportedThinkingLevels,
  type Api,
  type Model,
  type Models,
  type ModelThinkingLevel,
} from "@earendil-works/pi-ai";
import type { ModelRef } from "@earendil-works/pi-durable";
import type {
  AvailableModel,
  ModelProvider,
  ModelQuery,
  ModelState,
  ModelUpdate,
  Selection,
} from "@carillon/protocol";
import { resolved } from "@carillon/runtime/models";
import type { ModelSelections } from "@carillon/runtime/selections";
import { join } from "node:path";

const THINKING_OPTION = "thinking";
const THINKING_NAMES: Record<ModelThinkingLevel, string> = {
  off: "Off",
  minimal: "Minimal",
  low: "Low",
  medium: "Medium",
  high: "High",
  xhigh: "Extra high",
  max: "Max",
};

/** A model catalogue with the defaults that apply where nobody chose. */
export interface Catalogue {
  readonly models: Models;
  /** What runs when neither the agent nor the conversation chose a model, if anything. */
  runtimeDefault(): Selection | undefined;
  /** The thinking level a reasoning model runs at when no choice names one. */
  defaultThinking(model: Model<Api>): ModelThinkingLevel;
}

/** The model a conversation's next requests run, as pi-durable stores it. */
export interface ModelChoice {
  readonly model: ModelRef | null;
  readonly thinkingLevel: ModelThinkingLevel | null;
}

/** Pi's own catalogue and defaults, from the state directory it signs in to. */
export async function openPiCatalogue(stateDirectory: string): Promise<Catalogue> {
  const settings = SettingsManager.create(stateDirectory, stateDirectory);
  const models = await ModelRuntime.create({
    authPath: join(stateDirectory, "auth.json"),
    modelsPath: join(stateDirectory, "models.json"),
    modelsStorePath: join(stateDirectory, "models-store.json"),
    allowModelNetwork: true,
    modelRefreshTimeoutMs: 15_000,
  });
  return {
    models,
    runtimeDefault() {
      const provider = settings.getDefaultProvider();
      const model = settings.getDefaultModel();
      return provider === undefined || model === undefined
        ? undefined
        : { provider, model, options: {} };
    },
    defaultThinking: (model) =>
      clampThinkingLevel(
        model,
        settings.getModelThinkingLevel(model.provider, model.id) ??
          settings.getDefaultThinkingLevel() ??
          "medium",
      ),
  };
}

/** Refuses a catalogue whose runtime default model cannot run. */
export async function checkDefault(catalogue: Catalogue): Promise<void> {
  const selection = catalogue.runtimeDefault();
  if (selection === undefined) throw new Error("no Pi model is selected");
  const { provider, model } = selection;
  if ((await catalogue.models.getAvailable(provider)).every((item) => item.id !== model))
    throw new Error(`selected Pi model ${provider}/${model} is not available`);
}

/** The catalogue and Carillon's selections, as the picker and admission use them. */
export function modelChoices(catalogue: Catalogue, selections: ModelSelections) {
  function runtimeDefault(): Selection {
    const selection = catalogue.runtimeDefault();
    if (selection === undefined) throw new Error("no Pi model is selected");
    return selection;
  }

  async function available() {
    const models = [...(await catalogue.models.getAvailable())];
    models.sort(
      (left, right) =>
        left.provider.localeCompare(right.provider) ||
        left.name.localeCompare(right.name) ||
        left.id.localeCompare(right.id),
    );
    return models;
  }

  /** The catalogue as the picker sees it: one thinking setting per reasoning model. */
  function offered(models: readonly Model<Api>[]): AvailableModel[] {
    return models.map((model) => ({
      provider: model.provider,
      model: model.id,
      name: model.name,
      options: model.reasoning
        ? [
            {
              id: THINKING_OPTION,
              name: "Thinking",
              choices: getSupportedThinkingLevels(model).map((level) => ({
                id: level,
                name: THINKING_NAMES[level],
              })),
              default: catalogue.defaultThinking(model),
            },
          ]
        : [],
    }));
  }

  function providers(models: readonly Model<Api>[]): ModelProvider[] {
    return [...new Set(models.map((model) => model.provider))].map((id) => ({
      id,
      name: catalogue.models.getProvider(id)?.name ?? id,
    }));
  }

  async function state(
    read: (
      available: readonly AvailableModel[],
    ) => Promise<Omit<ModelState, "providers" | "available">>,
  ): Promise<ModelState> {
    const models = await available();
    const choices = offered(models);
    return { providers: providers(models), available: choices, ...(await read(choices)) };
  }

  return {
    state: (query: ModelQuery) =>
      state((choices) => selections.state(choices, runtimeDefault(), query)),
    update: (update: ModelUpdate) =>
      state((choices) => selections.update(choices, runtimeDefault(), update)),

    /**
     * What a turn of this conversation runs: the nearest scope's choice
     * resolved against the catalogue the picker shows. A choice that cannot
     * run is passed on as it is, so the turn fails with its explanation.
     */
    async forTurn(query: ModelQuery): Promise<ModelChoice> {
      const chosen = await selections.chosen(catalogue.runtimeDefault(), query);
      if (chosen === undefined) return { model: null, thinkingLevel: null };
      const models = await available();
      const selection = resolved(offered(models), chosen);
      const model = models.find(
        (item) => item.provider === selection.provider && item.id === selection.model,
      );
      const thinking = selection.options?.[THINKING_OPTION];
      return {
        model: { provider: selection.provider, modelId: selection.model },
        thinkingLevel: model !== undefined && supports(model, thinking) ? thinking : null,
      };
    },
  };
}

export type ModelChoices = ReturnType<typeof modelChoices>;

function supports(model: Model<Api>, level: string | undefined): level is ModelThinkingLevel {
  return getSupportedThinkingLevels(model).some((supported) => supported === level);
}
