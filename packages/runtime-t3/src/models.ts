/**
 * T3's models as Carillon offers them. The catalogue is T3's; the ledger
 * holds what each agent and channel chose. A turn's message names the
 * selection its run takes, which T3 makes its thread's own, so a model
 * picked in T3's own UI lasts until Carillon's next turn on the thread.
 */
import {
  noModel,
  type Catalogue,
  type CatalogueModel,
  type Channel,
  type Selection,
  type SettingsLedger,
} from "@carillon/runtime/settings";
import { Check } from "typebox/value";
import {
  OptionDescriptorSchema,
  ProviderSchema,
  RPC,
  ServerConfigSchema,
  type NativeSelection,
  type OptionDescriptor,
  type Provider,
} from "./contracts.ts";
import { T3Failure, type T3Connection } from "./t3.ts";

const ON = "on";
const OFF = "off";

/** T3's catalogue: the models Carillon offers, and which of their settings are booleans. */
export interface Catalog extends Catalogue {
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
  const providers: { id: string; name: string }[] = [];
  const models: CatalogueModel[] = [];
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
      models.push({
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
  return { providers, models, booleans };
}

export async function offered(connection: T3Connection): Promise<Catalog> {
  return catalog(await connection.call(RPC.getConfig, {}));
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

/** A selection as T3 names it: a boolean setting is true or false. */
function toNative(catalog: Catalog, selection: Selection): NativeSelection {
  return {
    instanceId: selection.provider,
    model: selection.model,
    options: Object.entries(selection.options).map(([id, value]) => ({
      id,
      value: catalog.booleans.has(optionKey(selection.provider, selection.model, id))
        ? value === ON
        : value,
    })),
  };
}

/** A turn neither its channel nor its agent has a model for. */
export class NoModel extends Error {
  constructor(agentId: string) {
    super(noModel(agentId));
  }
}

/**
 * What a turn in `channel` runs, as T3 names it: the channel's model, else
 * its agent's, against T3's catalogue now. A model T3 no longer lists is
 * kept, and its run fails rather than another model being substituted.
 */
export async function turnSelection(
  connection: T3Connection,
  ledger: SettingsLedger,
  channel: Channel,
): Promise<NativeSelection> {
  const models = await offered(connection);
  const selection = await ledger.selection(models, channel);
  if (selection === undefined) throw new NoModel(channel.agent_id);
  return toNative(models, selection);
}

/** The provider instance of the agent's own model, among those T3 lists, if it has one. */
export async function agentInstance(
  connection: T3Connection,
  ledger: SettingsLedger,
  agentId: string,
): Promise<string | undefined> {
  const models = await offered(connection);
  const lines = await ledger.settings(models, { kind: "agent", agent_id: agentId });
  const value = lines.find(({ id }) => id === "model")?.value;
  if (value?.kind !== "one") return undefined;
  return models.providers.find(({ id }) => value.id.startsWith(`${id}/`))?.id;
}
