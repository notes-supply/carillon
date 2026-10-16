/**
 * A T3 conversation's model, as T3 holds it. The thread's own selection is
 * the conversation's while it is on an instance the agent runs on: every run
 * of the thread uses it, and a change made here or in T3's own UI is the
 * thread's next selection, the last change winning. One on another
 * instance, as T3's UI may move a thread to, gives way to the agent default
 * at the next run. The catalogue is T3's, for the provider instances the
 * agent runs on. The agent default is the one `/model` chose for it, while
 * the agent runs on its instance, else the one its settings name.
 */
import type {
  AvailableModel,
  ModelState,
  ModelUpdate,
  ScopeState,
  Selection,
} from "@carillon/protocol";
import { ExplainedFailure } from "@carillon/runtime/failure";
import { resolved } from "@carillon/runtime/models";
import { Check } from "typebox/value";
import {
  OptionDescriptorSchema,
  ProviderSchema,
  ServerConfigSchema,
  type NativeSelection,
  type OptionDescriptor,
  type Provider,
} from "./contracts.ts";
import { T3Failure } from "./t3.ts";

const ON = "on";
const OFF = "off";

/** T3's catalogue: the models Carillon offers, and which of their settings are booleans. */
export interface Catalog {
  /** The instances, by T3's display name for each. */
  readonly providers: ModelState["providers"];
  readonly available: readonly AvailableModel[];
  /** Boolean settings by model, keyed by `optionKey`. */
  readonly booleans: ReadonlySet<string>;
  /** Which provider instances the agent runs on: the catalogue lists theirs. */
  readonly runsOn: (instanceId: string) => boolean;
}

/**
 * The models of the provider instances T3 reports enabled, installed and
 * usable, among those the agent `runsOn`.
 */
export function catalog(config: unknown, runsOn: Catalog["runsOn"] = () => true): Catalog {
  if (!Check(ServerConfigSchema, config))
    throw new T3Failure(
      "malformed",
      "T3 described its providers in a form this runtime does not understand",
    );
  const providers: ModelState["providers"] = [];
  const available: AvailableModel[] = [];
  const booleans = new Set<string>();
  for (const provider of config.providers) {
    if (!Check(ProviderSchema, provider) || !usable(provider) || !runsOn(provider.instanceId))
      continue;
    providers.push({ id: provider.instanceId, name: provider.displayName ?? provider.instanceId });
    for (const model of provider.models) {
      const options = (model.capabilities?.optionDescriptors ?? []).filter(
        (descriptor): descriptor is OptionDescriptor => Check(OptionDescriptorSchema, descriptor),
      );
      for (const descriptor of options)
        if (descriptor.type === "boolean")
          booleans.add(optionKey(provider.instanceId, model.slug, descriptor.id));
      available.push({
        provider: provider.instanceId,
        model: model.slug,
        name: model.name,
        options: options.flatMap((descriptor) => {
          if (descriptor.type === "boolean")
            return [
              {
                id: descriptor.id,
                name: descriptor.label,
                choices: [
                  { id: OFF, name: "Off" },
                  { id: ON, name: "On" },
                ],
                default: descriptor.currentValue === true ? ON : OFF,
              },
            ];
          const choices = descriptor.options.map(({ id, label }) => ({ id, name: label }));
          const fallback =
            descriptor.currentValue ??
            descriptor.options.find((choice) => choice.isDefault === true)?.id ??
            choices[0]?.id;
          return fallback === undefined
            ? []
            : [{ id: descriptor.id, name: descriptor.label, choices, default: fallback }];
        }),
      });
    }
  }
  return { providers, available, booleans, runsOn };
}

function usable(provider: Provider): boolean {
  return (
    provider.enabled &&
    provider.installed &&
    provider.availability !== "unavailable" &&
    (provider.status === "ready" || provider.status === "warning")
  );
}

function optionKey(instanceId: string, model: string, option: string): string {
  return JSON.stringify([instanceId, model, option]);
}

/** A native selection as Carillon names it: a boolean setting is `on` or `off`. */
export function fromNative(selection: NativeSelection): Selection {
  return {
    provider: selection.instanceId,
    model: selection.model,
    options: Object.fromEntries(
      (selection.options ?? []).map(({ id, value }) => [
        id,
        typeof value === "boolean" ? (value ? ON : OFF) : value,
      ]),
    ),
  };
}

/** A selection the catalogue validated, as T3 names it. */
export function toNative(catalog: Catalog, selection: Selection): NativeSelection {
  return {
    instanceId: selection.provider,
    model: selection.model,
    options: Object.entries(selection.options ?? {}).map(([id, value]) => ({
      id,
      value: catalog.booleans.has(optionKey(selection.provider, selection.model, id))
        ? value === ON
        : value,
    })),
  };
}

/** An agent's models, as one read of T3's catalogue and the ledger found them. */
export interface AgentModels {
  /** T3's catalogue of the instances the agent runs on. */
  readonly offered: Catalog;
  /** The agent default, resolved against the catalogue, and whether `/model` chose it. */
  readonly agent: { readonly selection: Selection; readonly overridden: boolean };
  /** The agent default as the ledger holds it, set aside while the agent may not run on it. */
  readonly chosen: ScopeState & { readonly selection: Selection };
}

/**
 * An agent's models: its catalogue, and its default. That is the one
 * `/model` chose, as the ledger resolved it (`chosen`), while it is on an
 * instance the agent runs on, else the settings' default: a profile's model
 * chosen while the agent was the assistant does not outlive its resources.
 * A chosen model of such an instance the catalogue no longer lists stays
 * the default, and its run fails rather than another model being substituted.
 */
export function agentModels(
  offered: Catalog,
  chosen: AgentModels["chosen"],
  settings: Selection,
): AgentModels {
  return {
    offered,
    agent: offered.runsOn(chosen.selection.provider)
      ? { selection: chosen.selection, overridden: chosen.overridden }
      : { selection: resolved(offered.available, settings), overridden: false },
    chosen,
  };
}

/**
 * A change of the agent default as the ledger is to make it. Its `expected`
 * is the default the person saw, the one the agent runs with, but the ledger
 * compares the one it holds, which differs while that is set aside. So the
 * person's view is checked here, and the ledger is handed the default it
 * held when it was read, refusing the change if another came first.
 */
export function ledgerUpdate(models: AgentModels, update: ModelUpdate): ModelUpdate {
  const { expected } = update;
  if (expected === undefined) return update;
  if (!sameSelection(models.agent.selection, { ...expected, options: expected.options ?? {} }))
    throw new ExplainedFailure(409, "The model settings changed.");
  return { ...update, expected: models.chosen.selection };
}

/**
 * The selection a conversation's next run takes: its thread's, while it is
 * on an instance the agent runs on, else the agent default. A model of such
 * an instance the catalogue no longer lists stays the thread's, and its run
 * fails rather than another model being substituted.
 */
export function routed(models: AgentModels, thread?: NativeSelection): NativeSelection {
  if (thread !== undefined && models.offered.runsOn(thread.instanceId)) return thread;
  return toNative(models.offered, models.agent.selection);
}

/**
 * The model state of an agent and, with a conversation, of its thread. A
 * conversation whose thread does not exist yet, or is on an instance the
 * agent does not run on, runs with the agent default.
 */
export function modelState(
  { offered, agent }: AgentModels,
  conversation: { readonly thread: NativeSelection | undefined } | undefined,
): ModelState {
  const providers = [...offered.providers];
  const available = [...offered.available];
  if (conversation === undefined) return { providers, available, agent };
  const thread = conversation.thread;
  if (thread === undefined || !offered.runsOn(thread.instanceId))
    return {
      providers,
      available,
      agent,
      conversation: { selection: agent.selection, overridden: false },
    };
  const selection = resolved(offered.available, fromNative(thread));
  return {
    providers,
    available,
    agent,
    conversation: { selection, overridden: !sameSelection(selection, agent.selection) },
  };
}

export function sameSelection(left: Selection, right: Selection): boolean {
  const leftOptions = Object.entries(left.options ?? {});
  return (
    left.provider === right.provider &&
    left.model === right.model &&
    leftOptions.length === Object.keys(right.options ?? {}).length &&
    leftOptions.every(([id, value]) => right.options?.[id] === value)
  );
}
