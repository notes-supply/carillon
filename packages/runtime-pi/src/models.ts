/**
 * What models the runtime offers and which one a conversation runs. The
 * catalogue is pi-ai's `Models`; Carillon's settings are kept in its own
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
import type { RuntimeSettings, SettingsRequest } from "@carillon/protocol";
import { ExplainedFailure } from "@carillon/runtime/failure";
import type { Catalogue, Channel, SettingsLedger } from "@carillon/runtime/settings";
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

/** Pi's models, with the thinking level each reasoning model runs at when nobody chose one. */
export interface PiCatalogue {
  readonly models: Models;
  defaultThinking(model: Model<Api>): ModelThinkingLevel;
}

/** The model a conversation's next requests run, as pi-durable stores it; none when nothing applies. */
export interface ModelChoice {
  readonly model: ModelRef | null;
  readonly thinkingLevel: ModelThinkingLevel | null;
}

/** Pi's own catalogue and thinking defaults, from the state directory it signs in to. */
export async function openPiCatalogue(stateDirectory: string): Promise<PiCatalogue> {
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
    defaultThinking: (model) =>
      clampThinkingLevel(
        model,
        settings.getModelThinkingLevel(model.provider, model.id) ??
          settings.getDefaultThinkingLevel() ??
          "medium",
      ),
  };
}

/** The catalogue and Carillon's settings ledger, as settings pages and admission use them. */
export function modelSettings(pi: PiCatalogue, ledger: SettingsLedger) {
  async function available(): Promise<Model<Api>[]> {
    const models = [...(await pi.models.getAvailable())];
    models.sort(
      (left, right) =>
        left.provider.localeCompare(right.provider) ||
        left.name.localeCompare(right.name) ||
        left.id.localeCompare(right.id),
    );
    return models;
  }

  function offered(models: readonly Model<Api>[]): Catalogue {
    return {
      providers: [...new Set(models.map((model) => model.provider))].map((id) => ({
        id,
        name: pi.models.getProvider(id)?.name ?? id,
      })),
      models: models.map((model) => ({
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
                default: pi.defaultThinking(model),
              },
            ]
          : [],
      })),
    };
  }

  return {
    catalogue: async () => offered(await available()),

    async settings({ subject, change }: SettingsRequest): Promise<RuntimeSettings> {
      const catalogue = offered(await available());
      return {
        settings:
          change === undefined
            ? await ledger.settings(catalogue, subject)
            : await ledger.change(catalogue, subject, change),
      };
    },

    /**
     * What a turn in the channel runs: its channel's model, else its
     * agent's, with thinking only at a level the model supports. A model
     * that cannot run is passed on as it is, and none when neither has one,
     * so the turn fails with its explanation.
     */
    async forTurn(channel: Channel): Promise<ModelChoice> {
      const models = await available();
      const selection = await ledger.selection(offered(models), channel).catch((failure) => {
        if (failure instanceof ExplainedFailure) return undefined;
        throw failure;
      });
      if (selection === undefined) return { model: null, thinkingLevel: null };
      const model = models.find(
        (item) => item.provider === selection.provider && item.id === selection.model,
      );
      const thinking = selection.options[THINKING_OPTION];
      return {
        model: { provider: selection.provider, modelId: selection.model },
        thinkingLevel: model !== undefined && supports(model, thinking) ? thinking : null,
      };
    },
  };
}

export type ModelSettings = ReturnType<typeof modelSettings>;

function supports(model: Model<Api>, level: string | undefined): level is ModelThinkingLevel {
  return getSupportedThinkingLevels(model).some((supported) => supported === level);
}
