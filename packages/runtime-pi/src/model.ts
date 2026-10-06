// Model selection: what the runtime offers, what each scope has chosen, and
// the ledger that keeps those choices. The runtime default is Pi's own
// setting; the agent default and conversation overrides are Carillon's, kept
// beside Pi's private state and resolved when a turn starts.

import { open, readFile, realpath, rename, writeFile } from "node:fs/promises";
import { basename, dirname, join } from "node:path";
import lockfile from "proper-lockfile";
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
import { ExplainedFailure } from "@carillon/runtime/failure";
import { resolved, validated } from "@carillon/runtime/models";
import { Serial } from "@carillon/runtime/serial";

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
    path = join(await realpath(dirname(path)), basename(path));
    const ledger = await readLedger(path);
    return new ModelSelections(path, ledger);
  }

  private async refresh(): Promise<void> {
    this.#ledger = await readLedger(this.#path);
  }

  /** What a turn in this conversation runs with: the nearest scope's choice, else the runtime default. */
  async effective(
    available: readonly AvailableModel[],
    runtimeDefault: Selection,
    query: ModelQuery,
  ): Promise<Selection> {
    return resolved(available, await this.chosen(runtimeDefault, query));
  }

  /**
   * The nearest scope's choice, else the runtime default, before it is
   * resolved against a catalogue. It is read in order with updates, from the
   * call on, so an update made later never changes it.
   */
  async chosen(runtimeDefault: Selection, query: ModelQuery): Promise<Selection> {
    return this.#writes.run(async () => {
      await this.refresh();
      return (
        this.conversationSelection(query) ?? this.agentSelection(query.agent_id) ?? runtimeDefault
      );
    });
  }

  async state(
    available: readonly AvailableModel[],
    runtimeDefault: Selection,
    query: ModelQuery,
  ): Promise<ModelState> {
    return this.#writes.run(async () => {
      await this.refresh();
      return this.snapshotState(available, runtimeDefault, query);
    });
  }

  private snapshotState(
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
      throw new ExplainedFailure(400, "There is no conversation here to set a model for.");
    const selection = update.selection === null ? null : validated(available, update.selection);
    return this.#writes.run(async () => {
      // The file is the authority. A short lease covers refresh, precondition,
      // replacement and fsync, across independently opened objects/processes.
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
        if (update.expected !== undefined) {
          const state = this.snapshotState(available, runtimeDefault, update);
          const current = state[update.scope]?.selection;
          const expected = { ...update.expected, options: update.expected.options ?? {} };
          if (!isDeepStrictEqual(current, expected))
            throw new ExplainedFailure(409, "The model settings changed.");
        }
        checkLease();
        await this.write(this.next(update, selection));
        checkLease();
        return this.snapshotState(available, runtimeDefault, update);
      } finally {
        await release();
      }
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

async function readLedger(path: string): Promise<Ledger> {
  try {
    return await Parse(LedgerSchema, JSON.parse(await readFile(path, "utf8")));
  } catch (failure: unknown) {
    if (failure instanceof Error && "code" in failure && failure.code === "ENOENT")
      return { agents: [], conversations: [] };
    throw failure;
  }
}
