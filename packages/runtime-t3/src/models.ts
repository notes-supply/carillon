/**
 * A T3 conversation's model, as T3 holds it. The thread's own selection is
 * the conversation's: every run of the thread uses it, and a change made
 * here or in T3's own UI is the thread's next selection, the last change
 * winning. The catalogue is T3's. The agent default, what a new thread
 * starts with, is the one `/model` chose for it, else the one its settings
 * name.
 */
import type { AvailableModel, ModelState, ScopeState, Selection } from "@carillon/protocol";
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
}

/** The models of the provider instances T3 reports enabled, installed and usable. */
export function catalog(config: unknown): Catalog {
  if (!Check(ServerConfigSchema, config))
    throw new T3Failure(
      "malformed",
      "T3 described its providers in a form this runtime does not understand",
    );
  const providers: ModelState["providers"] = [];
  const available: AvailableModel[] = [];
  const booleans = new Set<string>();
  for (const provider of config.providers) {
    if (!Check(ProviderSchema, provider) || !usable(provider)) continue;
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
  return { providers, available, booleans };
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
  readonly offered: Catalog;
  /** The agent default, resolved against the catalogue, and whether `/model` chose it. */
  readonly agent: ScopeState & { readonly selection: Selection };
}

/**
 * The selection a conversation's next run takes: its thread's, else the
 * agent default. A model the catalogue no longer lists stays the thread's,
 * and its run fails rather than another model being substituted.
 */
export function routed(models: AgentModels, thread?: NativeSelection): NativeSelection {
  return thread ?? toNative(models.offered, models.agent.selection);
}

/**
 * The model state of an agent and, with a conversation, of its thread. A
 * conversation whose thread does not exist yet runs with the agent default.
 */
export function modelState(
  { offered, agent }: AgentModels,
  conversation: { readonly thread: NativeSelection | undefined } | undefined,
): ModelState {
  const providers = [...offered.providers];
  const available = [...offered.available];
  if (conversation === undefined) return { providers, available, agent };
  const thread = conversation.thread;
  if (thread === undefined)
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
