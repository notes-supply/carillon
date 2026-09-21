// Model selection: what the runtime offers, what each scope has chosen, and
// the ledger that keeps those choices. The runtime default is Pi's own
// setting; the agent default and conversation overrides are Carillon's, kept
// beside Pi's private state and resolved when a turn starts.

import { open, readFile, rename, writeFile } from "node:fs/promises";
import { dirname } from "node:path";
import { isDeepStrictEqual } from "node:util";
import { Type, type Static } from "typebox";
import {
  SelectionSchema,
  type Selection,
  type AvailableModel,
  type ModelQuery,
  type ModelUpdate,
  type ModelState,
  type ScopeState,
} from "@carillon/protocol";
import { Parse } from "typebox/value";
import { Serial } from "./serial.ts";

export class ModelRejection extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

const LedgerSchema = Type.Object({
  agents: Type.Array(Type.Object({ agent_id: Type.String(), selection: SelectionSchema })),
  conversations: Type.Array(
    Type.Object({
      interface_id: Type.String(),
      agent_id: Type.String(),
      audience_id: Type.String(),
      selection: SelectionSchema,
    }),
  ),
});
type Ledger = Static<typeof LedgerSchema>;

/**
 * Selections made through Carillon, keyed by agent and by conversation.
 * Every change is written before it is confirmed. The ledger stores what was
 * chosen; what runs is `resolved` against the current catalogue on demand.
 */
export class ModelSelections {
  readonly #path: string;
  readonly #writes = new Serial();
  #ledger: Ledger;

  private constructor(path: string, ledger: Ledger) {
    this.#path = path;
    this.#ledger = ledger;
  }

  static async open(path: string): Promise<ModelSelections> {
    let ledger: Ledger = { agents: [], conversations: [] };
    try {
      ledger = Parse(LedgerSchema, JSON.parse(await readFile(path, "utf8")));
    } catch (failure: unknown) {
      if (!(failure instanceof Error && "code" in failure && failure.code === "ENOENT"))
        throw failure;
    }
    return new ModelSelections(path, ledger);
  }

  /** What a turn in this conversation runs with: the nearest scope's choice, else the runtime default. */
  effective(
    available: readonly AvailableModel[],
    runtimeDefault: Selection,
    query: ModelQuery,
  ): Selection {
    return resolved(
      available,
      this.conversationSelection(query) ?? this.agentSelection(query.agent_id) ?? runtimeDefault,
    );
  }

  state(
    available: readonly AvailableModel[],
    runtimeDefault: Selection,
    query: ModelQuery,
  ): ModelState {
    const agentSelection = this.agentSelection(query.agent_id);
    const agent: ScopeState = {
      selection: resolved(available, agentSelection ?? runtimeDefault),
      overridden: agentSelection !== null,
    };
    if (query.audience_id === undefined) return { available: [...available], agent };
    const conversationSelection = this.conversationSelection(query);
    return {
      available: [...available],
      agent,
      conversation: {
        selection:
          conversationSelection === null
            ? agent.selection
            : resolved(available, conversationSelection),
        overridden: conversationSelection !== null,
      },
    };
  }

  /**
   * Applies a validated change and returns the state it produced. Changes are
   * written one at a time; they never wait for a turn, which keeps the
   * selection it started with.
   */
  async update(
    available: readonly AvailableModel[],
    runtimeDefault: Selection,
    update: ModelUpdate,
  ): Promise<ModelState> {
    if (update.scope === "conversation" && update.audience_id === undefined)
      throw new ModelRejection(400, "There is no conversation here to set a model for.");
    const selection = update.selection === null ? null : validated(available, update.selection);
    return this.#writes.run(async () => {
      if (update.expected !== undefined) {
        const state = this.state(available, runtimeDefault, update);
        const current = state[update.scope]?.selection;
        const expected = { ...update.expected, options: update.expected.options ?? {} };
        if (!isDeepStrictEqual(current, expected))
          throw new ModelRejection(
            409,
            "The model settings changed; refresh the model state and try again.",
          );
      }
      await this.write(this.next(update, selection));
      return this.state(available, runtimeDefault, update);
    });
  }

  private next(update: ModelUpdate, selection: Selection | null): Ledger {
    return update.scope === "agent"
      ? {
          agents: [
            ...this.#ledger.agents.filter((entry) => entry.agent_id !== update.agent_id),
            ...(selection === null ? [] : [{ agent_id: update.agent_id, selection }]),
          ],
          conversations: this.#ledger.conversations,
        }
      : {
          agents: this.#ledger.agents,
          conversations: [
            ...this.#ledger.conversations.filter((entry) => !sameConversation(entry, update)),
            ...(selection === null || update.audience_id === undefined
              ? []
              : [
                  {
                    interface_id: update.interface_id,
                    agent_id: update.agent_id,
                    audience_id: update.audience_id,
                    selection,
                  },
                ]),
          ],
        };
  }

  private async write(next: Ledger): Promise<void> {
    const temporary = `${this.#path}.${String(process.pid)}.tmp`;
    await writeFile(temporary, `${JSON.stringify(next, null, 2)}\n`, { mode: 0o600, flush: true });
    await rename(temporary, this.#path);
    // The rename committed the visible state even if syncing its directory
    // fails. Keep memory aligned with disk, but do not confirm durability.
    this.#ledger = next;
    const directory = await open(dirname(this.#path), "r");
    try {
      await directory.sync();
    } finally {
      await directory.close();
    }
  }

  private agentSelection(agentId: string): Selection | null {
    return this.#ledger.agents.find((entry) => entry.agent_id === agentId)?.selection ?? null;
  }

  private conversationSelection(query: ModelQuery): Selection | null {
    if (query.audience_id === undefined) return null;
    return (
      this.#ledger.conversations.find((entry) => sameConversation(entry, query))?.selection ?? null
    );
  }
}

function sameConversation(
  entry: { interface_id: string; agent_id: string; audience_id: string },
  query: ModelQuery,
): boolean {
  return (
    entry.interface_id === query.interface_id &&
    entry.agent_id === query.agent_id &&
    entry.audience_id === query.audience_id
  );
}

function offered(available: readonly AvailableModel[], selection: Selection) {
  return available.find(
    (candidate) => candidate.provider === selection.provider && candidate.model === selection.model,
  );
}

/**
 * The selection as it runs: every option the model supports, with the chosen
 * value where it is still supported and the model's default elsewhere. A
 * model the catalogue no longer offers is returned as chosen; a turn on it
 * fails rather than substituting another model.
 */
export function resolved(available: readonly AvailableModel[], selection: Selection): Selection {
  const model = offered(available, selection);
  if (model === undefined) return selection;
  const options: Record<string, string> = {};
  for (const option of model.options) {
    const choice = selection.options?.[option.id];
    options[option.id] =
      choice !== undefined && option.choices.includes(choice) ? choice : option.default;
  }
  return { provider: model.provider, model: model.model, options };
}

/** A selection the catalog supports, with unset options omitted. */
function validated(available: readonly AvailableModel[], selection: Selection): Selection {
  const model = offered(available, selection);
  if (model === undefined) throw new ModelRejection(422, "That model is not available.");
  const options: Record<string, string> = {};
  for (const [id, choice] of Object.entries(selection.options ?? {})) {
    const option = model.options.find((candidate) => candidate.id === id);
    if (option === undefined) throw new ModelRejection(422, `${model.name} has no ${id} setting.`);
    if (!option.choices.includes(choice))
      throw new ModelRejection(
        422,
        `${choice} is not a ${option.name} setting ${model.name} supports.`,
      );
    options[id] = choice;
  }
  return { provider: model.provider, model: model.model, options };
}

/** The catalog the deterministic provider substitute offers. */
export const DETERMINISTIC_MODELS: readonly AvailableModel[] = [
  { provider: "local", model: "echo", name: "Local echo", options: [] },
  {
    provider: "local",
    model: "reasoner",
    name: "Local reasoner",
    options: [
      {
        id: "thinking",
        name: "Thinking",
        choices: ["off", "low", "medium", "high"],
        default: "medium",
      },
    ],
  },
  { provider: "substitute", model: "fixed", name: "Substitute fixed", options: [] },
];

export const DETERMINISTIC_DEFAULT: Selection = { provider: "local", model: "echo", options: {} };

/** `provider/model` with any chosen options, for output that names the model. */
export function describeSelection(selection: Selection): string {
  const options = Object.entries(selection.options ?? {})
    .map(([id, choice]) => `${id}=${choice}`)
    .join(", ");
  return `${selection.provider}/${selection.model}${options.length === 0 ? "" : ` (${options})`}`;
}
