/** A native Pi runtime with instance-owned execution and model state. */
import { ModelRuntime, SettingsManager } from "@earendil-works/pi-coding-agent";
import type { AgentSessionEvent } from "@earendil-works/pi-coding-agent";
import {
  clampThinkingLevel,
  getSupportedThinkingLevels,
  type Api,
  type Model,
  type ModelThinkingLevel,
} from "@earendil-works/pi-ai";
import type {
  AvailableModel,
  Selection,
  ModelQuery,
  ModelUpdate,
  ModelState,
} from "@carillon/protocol";
import { mkdir, stat } from "node:fs/promises";
import { join, resolve } from "node:path";
import { ModelSelections } from "./model.ts";
import { openConversation } from "./namespace.ts";
import { Serial } from "./serial.ts";
import { createPiExecutionSession, promptSession } from "./session.ts";
import type { RuntimeOperations, TurnRequest, TurnResponse } from "./operations.ts";

export interface PiRuntimeOptions {
  readonly workspace: string;
  readonly stateDirectory: string;
  /** Defaults to sessions under stateDirectory. The service supplies its installation path. */
  readonly sessionDirectory?: string;
  readonly modelRuntime?: ModelRuntime;
  readonly settings?: SettingsManager;
  readonly selections?: ModelSelections;
  /** Explicitly opt into catalogue network discovery during construction. */
  readonly allowModelNetwork?: boolean;
}

/** Construction is explicit; importing this module does not discover providers or open files.
 * Supplied collaborators remain caller-owned. close() cancels/settles work, but does not destroy them.
 */
export async function createPiRuntime(options: PiRuntimeOptions): Promise<RuntimeOperations> {
  const workspace = resolve(options.workspace);
  if (!(await stat(workspace)).isDirectory())
    throw new Error("workspace must be an existing directory");
  const stateDirectory = resolve(options.stateDirectory);
  const sessionDirectory = resolve(options.sessionDirectory ?? join(stateDirectory, "sessions"));
  await mkdir(stateDirectory, { recursive: true, mode: 0o700 });
  await mkdir(sessionDirectory, { recursive: true, mode: 0o700 });
  const settings = options.settings ?? SettingsManager.create(stateDirectory, stateDirectory);
  const modelRuntime =
    options.modelRuntime ??
    (await ModelRuntime.create({
      authPath: join(stateDirectory, "auth.json"),
      modelsPath: join(stateDirectory, "models.json"),
      modelsStorePath: join(stateDirectory, "models-store.json"),
      allowModelNetwork: options.allowModelNetwork ?? false,
      modelRefreshTimeoutMs: 15_000,
    }));
  const selections =
    options.selections ??
    (await ModelSelections.open(join(stateDirectory, "model-selection.json")));
  const turns = new Serial();
  const shutdown = new AbortController();
  const pending = new Set<Promise<unknown>>();
  const THINKING_OPTION = "thinking";
  await selectedPiModel();

  async function track<T>(operation: () => Promise<T>): Promise<T> {
    shutdown.signal.throwIfAborted();
    const promise = operation();
    pending.add(promise);
    try {
      return await promise;
    } finally {
      pending.delete(promise);
    }
  }

  return {
    executeTurn(request, signal) {
      return track(() =>
        turns.run(async () => {
          const combined =
            signal === undefined ? shutdown.signal : AbortSignal.any([signal, shutdown.signal]);
          combined.throwIfAborted();
          return executePi(request, combined);
        }),
      );
    },
    modelState(query: ModelQuery): Promise<ModelState> {
      return track(async () =>
        selections.state(offeredPiModels(await availablePiModels()), runtimeDefault(), query),
      );
    },
    setModel(update: ModelUpdate): Promise<ModelState> {
      return track(async () =>
        selections.update(offeredPiModels(await availablePiModels()), runtimeDefault(), update),
      );
    },
    async close() {
      shutdown.abort(new Error("Pi runtime is closed"));
      await Promise.allSettled([...pending]);
    },
  };

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
    const provider = settings.getDefaultProvider();
    const model = settings.getDefaultModel();
    if (provider === undefined || model === undefined) throw new Error("no Pi model is selected");
    return { provider, model, options: {} };
  }

  function defaultThinkingLevel(model: Model<Api>): ModelThinkingLevel {
    return clampThinkingLevel(
      model,
      settings.getModelThinkingLevel(model.provider, model.id) ??
        settings.getDefaultThinkingLevel() ??
        "medium",
    );
  }

  async function availablePiModels() {
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
      throw new Error(
        `selected Pi model ${selection.provider}/${selection.model} is not available`,
      );
    return model;
  }

  async function executePi(request: TurnRequest, signal: AbortSignal): Promise<TurnResponse> {
    const root = resolve(request.resource_root);
    const manager = openConversation(workspace, sessionDirectory, request);
    // The effective selection is applied to every turn, so a continued session
    // follows the current choice rather than the model it began with. It is
    // resolved against the same catalogue the picker reports, so the thinking
    // level that runs is the one shown.
    const models = await availablePiModels();
    const selection = selections.effective(offeredPiModels(models), runtimeDefault(), request);
    const model = piModel(models, selection);
    const thinking = selection.options?.[THINKING_OPTION];
    const runtime = modelRuntime;
    const created = await createPiExecutionSession({
      resourceRoot: root,
      workspace: workspace,
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
}
