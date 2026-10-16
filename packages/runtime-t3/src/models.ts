/**
 * A T3 conversation's model, as T3 holds it. The thread's own selection is
 * the conversation's while it is on an instance the agent runs on: every run
 * of the thread uses it, and a change made here or in T3's own UI is the
 * thread's next selection, the last change winning. One on another
 * instance, as T3's UI may move a thread to, gives way to the agent default
 * at the next run. The catalogue is T3's, for the provider instances the
 * agent runs on. The agent default is the one `/model` chose for it, else the
 * one its settings name.
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
  const available: AvailableModel[] = [];
  const booleans = new Set<string>();
  for (const provider of config.providers) {
    if (!Check(ProviderSchema, provider) || !usable(provider) || !runsOn(provider.instanceId))
      continue;
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
                choices: [OFF, ON],
                default: descriptor.currentValue === true ? ON : OFF,
              },
            ];
          const choices = descriptor.options.map((choice) => choice.id);
          const fallback =
            descriptor.currentValue ??
            descriptor.options.find((choice) => choice.isDefault === true)?.id ??
            choices[0];
          return fallback === undefined
            ? []
            : [{ id: descriptor.id, name: descriptor.label, choices, default: fallback }];
        }),
      });
    }
  }
  return { available, booleans, runsOn };
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

/**
 * The selection a conversation's next run takes: its thread's, while it is
 * on an instance the agent runs on, else the agent default. A model of such
 * an instance the catalogue no longer lists stays the thread's, and its run
 * fails rather than another model being substituted.
 */
export function routed(
  catalog: Catalog,
  agentDefault: Selection,
  thread: NativeSelection | undefined,
): NativeSelection {
  if (thread !== undefined && catalog.runsOn(thread.instanceId)) return thread;
  return toNative(catalog, resolved(catalog.available, agentDefault));
}

/**
 * The model state of an agent, whose default `agent` the ledger resolved,
 * and, with a conversation, of its thread. A conversation whose thread does
 * not exist yet, or is on an instance the agent does not run on, runs with
 * the agent default.
 */
export function modelState(
  catalog: Catalog,
  agent: ScopeState,
  conversation: { readonly thread: NativeSelection | undefined } | undefined,
): ModelState {
  if (conversation === undefined) return { available: [...catalog.available], agent };
  const thread = conversation.thread;
  if (thread === undefined || !catalog.runsOn(thread.instanceId))
    return {
      available: [...catalog.available],
      agent,
      conversation: { selection: agent.selection, overridden: false },
    };
  const selection = resolved(catalog.available, fromNative(thread));
  return {
    available: [...catalog.available],
    agent,
    conversation: {
      selection,
      overridden: agent.selection === null || !sameSelection(selection, agent.selection),
    },
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
