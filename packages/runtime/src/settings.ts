/**
 * The model settings a runtime keeps for its agents and their channels. Each
 * agent and each channel stores at most one model unit, the model with the
 * options chosen for it. A channel without one inherits its agent's, and the
 * agent's is the only agent default. The ledger stores what was chosen; what
 * runs is `resolved` against the runtime's current catalogue: an option the
 * channel did not choose takes the agent's choice while the channel runs the
 * agent's model, and the model's default otherwise.
 */
import { createHash } from "node:crypto";
import { readFile, realpath } from "node:fs/promises";
import { basename, dirname, join } from "node:path";
import { isDeepStrictEqual } from "node:util";
import lockfile from "proper-lockfile";
import { Type, type Static } from "typebox";
import { Parse } from "typebox/value";
import type { Change, Setting, SettingValue, Subject } from "@carillon/protocol";
import { ExplainedFailure } from "./failure.ts";
import { publish, Unsynced } from "./publish.ts";
import { Serial } from "./serial.ts";

/** A model with values for its options: those chosen, or all of them as a runtime runs it. */
export const SelectionSchema = Type.Object({
  provider: Type.String({ minLength: 1 }),
  model: Type.String({ minLength: 1 }),
  options: Type.Record(Type.String(), Type.String()),
});
export type Selection = Static<typeof SelectionSchema>;

/** One engine setting a model supports, with the choice it takes when none is stored. */
export interface ModelOption {
  readonly id: string;
  readonly name: string;
  readonly choices: readonly { readonly id: string; readonly name: string }[];
  readonly default: string;
}

export interface CatalogueModel {
  readonly provider: string;
  readonly model: string;
  readonly name: string;
  readonly options: readonly ModelOption[];
}

/** What a runtime offers now: its providers, by the IDs models name, and their models. */
export interface Catalogue {
  readonly providers: readonly { readonly id: string; readonly name: string }[];
  readonly models: readonly CatalogueModel[];
}

/** The channel a turn runs in, as core recorded it. */
export interface Channel {
  readonly interface_id: string;
  readonly channel_id: string;
  readonly agent_id: string;
}

const MODEL = "model";
const OPTION = "model.";

/**
 * The units that decide a subject's lines: its own, and its agent's while
 * the agent's choices apply there, which is while a channel inherits or
 * runs the agent's model.
 */
interface Units {
  readonly own: Selection | null;
  readonly agent: Selection | null;
}

const LedgerSchema = Type.Object({
  agents: Type.Array(Type.Object({ agent_id: Type.String(), model: SelectionSchema })),
  channels: Type.Array(
    Type.Object({
      interface_id: Type.String(),
      channel_id: Type.String(),
      agent_id: Type.String(),
      model: SelectionSchema,
    }),
  ),
});
type Ledger = Static<typeof LedgerSchema>;

export function offeredModel(
  catalogue: Catalogue,
  selection: Pick<Selection, "provider" | "model">,
): CatalogueModel | undefined {
  return catalogue.models.find((candidate) => sameModel(candidate, selection));
}

/**
 * The selection as it runs: every option the model supports, at the choice
 * stored for it where that is still offered, else at the choice `beneath` it,
 * such as its agent's on the same model, else at the model's default. A model
 * the catalogue no longer offers is returned as stored; a turn on it fails
 * rather than another model being substituted.
 */
export function resolved(
  catalogue: Catalogue,
  selection: Selection,
  beneath: Selection["options"] = {},
): Selection {
  const model = offeredModel(catalogue, selection);
  if (model === undefined) return selection;
  const options: Record<string, string> = {};
  for (const option of model.options) {
    options[option.id] =
      [selection.options[option.id], beneath[option.id]].find(
        (choice) => choice !== undefined && offers(option, choice),
      ) ?? option.default;
  }
  return { provider: model.provider, model: model.model, options };
}

/** The ID a model's choice has on the Model line. */
export function modelChoiceId(model: Pick<Selection, "provider" | "model">): string {
  return `${model.provider}/${model.model}`;
}

/**
 * Why a turn has no model to run, naming how to give its agent one. The text
 * is the runtime's own, so a runtime settles the turn with it as written.
 */
export function noModel(agentId: string): string {
  return `No model is set for ${agentId}. Set one with \`carillon settings --agent ${agentId} model=<provider>/<model>\`.`;
}

/**
 * Model settings kept for agents and channels. Every change is written under
 * a lease, across processes, before it is confirmed, and only while the
 * revision it names is still the setting's.
 */
export class SettingsLedger {
  readonly #path: string;
  readonly #writes = new Serial();
  #ledger: Ledger;

  private constructor(path: string, ledger: Ledger) {
    this.#path = path;
    this.#ledger = ledger;
  }

  static async open(path: string): Promise<SettingsLedger> {
    path = join(await realpath(dirname(path)), basename(path));
    return new SettingsLedger(path, await readLedger(path));
  }

  /** The subject's model lines as the catalogue offers them now. */
  async settings(catalogue: Catalogue, subject: Subject): Promise<Setting[]> {
    return this.#writes.run(async () => {
      await this.refresh();
      return this.lines(catalogue, subject);
    });
  }

  /**
   * Applies `change` while the line it names still has the revision it
   * expects, and returns the subject's lines after it. A stale change gets
   * 409 and writes nothing, as does a change that leaves the subject's unit
   * as it was.
   */
  async change(catalogue: Catalogue, subject: Subject, change: Change): Promise<Setting[]> {
    return this.#writes.run(async () => {
      // The file is the authority. A short lease covers refresh, revision
      // check, replacement and fsync, across processes.
      let compromised: Error | undefined;
      const checkLease = () => {
        if (compromised) throw compromised;
      };
      const release = await lockfile.lock(this.#path, {
        realpath: false,
        retries: { retries: 8, minTimeout: 10, maxTimeout: 100 },
        onCompromised(error) {
          compromised = error;
        },
      });
      try {
        await this.refresh();
        const line = this.lines(catalogue, subject).find(({ id }) => id === change.setting);
        if (line === undefined || line.revision !== change.expected)
          throw new ExplainedFailure(409, "This setting changed meanwhile.");
        const units = this.units(subject);
        const unit = changed(catalogue, units, change);
        if (!isDeepStrictEqual(unit, units.own)) {
          checkLease();
          await this.write(this.stored(subject, unit));
          checkLease();
        }
        return this.lines(catalogue, subject);
      } finally {
        await release();
      }
    });
  }

  /**
   * What a turn in `channel` runs: the channel's unit, else its agent's,
   * resolved against the catalogue, or `undefined` when neither has one. Read
   * in order with changes, so a change made after the call never alters it.
   */
  async selection(catalogue: Catalogue, channel: Channel): Promise<Selection | undefined> {
    return this.#writes.run(async () => {
      await this.refresh();
      const { own, agent } = this.units({ kind: "channel", ...channel });
      const unit = own ?? agent;
      return unit === null ? undefined : resolved(catalogue, unit, agent?.options);
    });
  }

  private async refresh(): Promise<void> {
    this.#ledger = await readLedger(this.#path);
  }

  private agentUnit(agentId: string): Selection | null {
    return this.#ledger.agents.find((entry) => entry.agent_id === agentId)?.model ?? null;
  }

  private channelUnit(channel: Channel): Selection | null {
    return this.#ledger.channels.find((entry) => sameChannel(entry, channel))?.model ?? null;
  }

  private units(subject: Subject): Units {
    if (subject.kind === "agent") return { own: this.agentUnit(subject.agent_id), agent: null };
    const own = this.channelUnit(subject);
    const agent = this.agentUnit(subject.agent_id);
    const applies = agent !== null && (own === null || sameModel(own, agent));
    return { own, agent: applies ? agent : null };
  }

  private lines(catalogue: Catalogue, subject: Subject): Setting[] {
    const { own, agent } = this.units(subject);
    const unit = own ?? agent;
    const running = unit === null ? null : resolved(catalogue, unit, agent?.options);
    const entry = running === null ? undefined : offeredModel(catalogue, running);
    const revision = (setting: string) =>
      digest([setting, subjectKey(subject), own, agent, entry ?? null]);
    const groups = new Map(catalogue.providers.map(({ id, name }) => [id, name]));
    const model: Setting = {
      id: MODEL,
      name: "Model",
      choices: catalogue.models.map((candidate) => ({
        id: modelChoiceId(candidate),
        name: candidate.name,
        group: groups.get(candidate.provider) ?? candidate.provider,
      })),
      value: running === null ? null : one(modelChoiceId(running)),
      inherited: subject.kind === "channel" && own === null,
      revision: revision(MODEL),
    };
    const options = (entry?.options ?? []).map((option): Setting => {
      const id = `${OPTION}${option.id}`;
      const value = running?.options[option.id] ?? option.default;
      const chosen = own?.options[option.id];
      return {
        id,
        name: option.name,
        choices: option.choices.map((choice) => ({ id: choice.id, name: choice.name })),
        value: one(value),
        inherited: chosen === undefined || !offers(option, chosen),
        revision: revision(id),
      };
    });
    return [model, ...options];
  }

  private stored(subject: Subject, unit: Selection | null): Ledger {
    if (subject.kind === "agent")
      return {
        agents: [
          ...this.#ledger.agents.filter((entry) => entry.agent_id !== subject.agent_id),
          ...(unit === null ? [] : [{ agent_id: subject.agent_id, model: unit }]),
        ],
        channels: this.#ledger.channels,
      };
    const { interface_id, channel_id, agent_id } = subject;
    return {
      agents: this.#ledger.agents,
      channels: [
        ...this.#ledger.channels.filter((entry) => !sameChannel(entry, subject)),
        ...(unit === null ? [] : [{ interface_id, channel_id, agent_id, model: unit }]),
      ],
    };
  }

  private async write(next: Ledger): Promise<void> {
    await publish(this.#path, `${JSON.stringify(next, null, 2)}\n`).catch((failure: unknown) => {
      // The rename committed the visible state even if syncing its directory
      // fails. Keep memory aligned with disk, but do not confirm durability.
      if (failure instanceof Unsynced) this.#ledger = next;
      throw failure;
    });
    this.#ledger = next;
  }
}

/**
 * The subject's own unit after `change`: none on a reset of the model; a new
 * model with no options chosen, so option values never outlive their model;
 * or its unit with one option chosen, or no longer chosen on its reset. A
 * channel that inherits gets its agent's model with that one option chosen,
 * and keeps inheriting on an option's reset.
 */
function changed(catalogue: Catalogue, { own, agent }: Units, change: Change): Selection | null {
  if (change.setting === MODEL) {
    if (change.value === null) return null;
    const id = single(change.value);
    const model = catalogue.models.find((candidate) => modelChoiceId(candidate) === id);
    if (model === undefined) throw new ExplainedFailure(422, `${id} is not a model offered here.`);
    return { provider: model.provider, model: model.model, options: {} };
  }
  const running = own ?? agent;
  const option = (running === null ? undefined : offeredModel(catalogue, running))?.options.find(
    (candidate) => `${OPTION}${candidate.id}` === change.setting,
  );
  if (running === null || option === undefined)
    throw new ExplainedFailure(409, "This setting changed meanwhile.");
  if (change.value === null) {
    if (own === null) return null;
    const options = Object.entries(own.options).filter(([chosen]) => chosen !== option.id);
    return { ...own, options: Object.fromEntries(options) };
  }
  const id = single(change.value);
  if (!offers(option, id))
    throw new ExplainedFailure(422, `${id} is not a ${option.name} choice here.`);
  const base = own ?? { provider: running.provider, model: running.model, options: {} };
  return { ...base, options: { ...base.options, [option.id]: id } };
}

function single(value: SettingValue): string {
  if (value.kind !== "one") throw new ExplainedFailure(422, "A model setting takes one choice.");
  return value.id;
}

function one(id: string): SettingValue {
  return { kind: "one", id };
}

function offers(option: ModelOption, choice: string): boolean {
  return option.choices.some((candidate) => candidate.id === choice);
}

function subjectKey(subject: Subject): readonly string[] {
  return subject.kind === "agent"
    ? ["agent", subject.agent_id]
    : ["channel", subject.interface_id, subject.channel_id, subject.agent_id];
}

/** SHA-256 of a canonical JSON encoding, with object keys sorted. */
function digest(value: unknown): string {
  return createHash("sha256")
    .update(JSON.stringify(canonical(value)))
    .digest("hex");
}

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (typeof value !== "object" || value === null) return value;
  return Object.fromEntries(
    Object.entries(value)
      .toSorted(([left], [right]) => (left < right ? -1 : left > right ? 1 : 0))
      .map(([key, entry]) => [key, canonical(entry)]),
  );
}

function sameModel(
  left: Pick<Selection, "provider" | "model">,
  right: Pick<Selection, "provider" | "model">,
): boolean {
  return left.provider === right.provider && left.model === right.model;
}

function sameChannel(entry: Channel, channel: Channel): boolean {
  return (
    entry.interface_id === channel.interface_id &&
    entry.channel_id === channel.channel_id &&
    entry.agent_id === channel.agent_id
  );
}

async function readLedger(path: string): Promise<Ledger> {
  try {
    return Parse(LedgerSchema, JSON.parse(await readFile(path, "utf8")));
  } catch (failure: unknown) {
    if (failure instanceof Error && "code" in failure && failure.code === "ENOENT")
      return { agents: [], channels: [] };
    throw failure;
  }
}
