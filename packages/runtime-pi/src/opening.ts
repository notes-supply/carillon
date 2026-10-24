/**
 * Turns the runtime opens for input no message brought, such as a
 * schedule's run. Each opening is a background task of its conversation
 * that opens the turn with core and places its input, or settles the turn
 * stopped once the work it was for is over. Core's key and the input's
 * request ID make each step happen once, so a restart at any point opens,
 * places and settles every turn once.
 */
import type { Context } from "@earendil-works/chord";
import {
  defineTask,
  type ConversationId,
  type SubmissionId,
  type TaskRuntime,
  type UserInput,
} from "@earendil-works/pi-durable";
import type { CoreClient } from "@carillon/runtime/core";
import { diagnostic } from "@carillon/runtime/failure";
import { backoff, over, refusedForGood, type DeliveryInput, type Needs } from "./delivery.ts";
import { ConversationInfo } from "./documents.ts";

export const OPENING = "carillon.opening";

/** A turn to open in the opening task's conversation. */
export interface Opening {
  /** Tells it apart from the agent's other openings, such as `schedule:<task>:<run>`. */
  readonly key: string;
  /** What the turn is for, as core records it. */
  readonly label: string;
  readonly content: UserInput;
  /** Its input runs only while `task` is not cancelled and the agent selects `extension`. */
  readonly needs?: Needs;
}

export interface OpeningHost {
  readonly core: Pick<CoreClient, "open" | "settle">;
  /**
   * The key core knows an opening by: the agent's own, and its store's, so
   * neither another agent nor a store recreated in its place reuses it.
   */
  key(opening: string): string;
  /** Whether the agent selects the extension; undefined while that is unknown. */
  selects(extension: string): boolean | undefined;
  /** The input placed in the conversation under the request ID, once it is. */
  placed(conversation: ConversationId, requestId: string): Promise<SubmissionId | undefined>;
  /** Places the opened turn's input in the conversation, its key as its request ID. */
  place(conversation: ConversationId, turn: string, opening: Opening): Promise<SubmissionId>;
  /** Holds the turn until its delivery task settles it like any turn. */
  track(input: DeliveryInput): Promise<void>;
  /** Stops the placed input as a person's stop does, so its turn settles stopped. */
  stop(conversation: ConversationId, submission: SubmissionId): Promise<void>;
}

type OpeningState = { readonly phase: "open"; readonly attempt: number };
type Runtime = TaskRuntime<Opening, OpeningState, null, object>;

export type OpeningTask = ReturnType<typeof openingTask>;

const COMPLETED = { status: "terminal", outcome: { status: "completed", result: null } } as const;
/** The memo saying core was asked to open the turn, which it then may have. */
const ASKED = "asked";

export function openingTask(host: OpeningHost) {
  /**
   * Opens the turn, or finds the one the key opened, and places its input
   * or stops it. Work already over opens nothing, unless an earlier attempt
   * asked core, which may have opened the turn. A turn core settled
   * meanwhile needs nothing more.
   */
  async function open(opening: Opening, runtime: Runtime, context: Context): Promise<void> {
    if ((await runtime.memo<true>(ASKED, context)) === undefined) {
      if (!(await runs(opening, runtime, context))) return;
      await runtime.memo(ASKED, true, context);
    }
    const conversation = runtime.conversationId;
    const info = await runtime.snapshot(ConversationInfo, conversation, context);
    if (info === undefined)
      throw new Error(`conversation ${String(conversation)} serves no Carillon conversation`);
    const { interface_id, agent_id, audience_id } = info;
    const { turn_id: turn, state } = await host.core.open(
      { interface_id, agent_id, audience_id, key: host.key(opening.key), label: opening.label },
      runtime.signal,
    );
    if (state !== "running") return;
    let submission = await host.placed(conversation, opening.key);
    if (submission === undefined) {
      if (!(await runs(opening, runtime, context))) {
        await host.core.settle(turn, { outcome: "stopped" });
        return;
      }
      submission = await host.place(conversation, turn, opening);
    }
    await host.track({
      turn,
      interface_id,
      conversation,
      submission,
      ...(opening.needs === undefined ? {} : { needs: opening.needs }),
    });
    // The work may have ended while the input was being placed.
    if (!(await runs(opening, runtime, context))) await host.stop(conversation, submission);
  }

  async function runs(opening: Opening, runtime: Runtime, context: Context): Promise<boolean> {
    if (opening.needs === undefined) return true;
    const selected = host.selects(opening.needs.extension);
    if (selected === undefined)
      throw new Error("the agent's extensions are unknown until resources that load arrive");
    return !over(selected, await runtime.getTask(opening.needs.task, context));
  }

  return defineTask<Opening, OpeningState, null>({
    name: OPENING,
    version: 1,
    initial: () => ({ phase: "open", attempt: 0 }),
    phases: {
      open: async (task, runtime, context) => {
        const { attempt } = task.state.checkpoint;
        try {
          await open(task.input, runtime, context);
        } catch (failure) {
          // The harness is closing: the turn is opened once it reopens.
          if (runtime.signal.aborted) throw failure;
          if (!refusedForGood(failure)) {
            const delay = backoff(attempt);
            console.error(
              `opening ${task.input.key}: ${diagnostic(failure)}; trying again in ${String(delay / 1_000)} s`,
            );
            const checkpoint = { phase: "open", attempt: attempt + 1 } as const;
            await runtime.sleepUntil(runtime.now() + delay, checkpoint, context);
            return;
          }
          console.error(`opening ${task.input.key}: ${diagnostic(failure)}; giving it up`);
        }
        await runtime.commit(() => COMPLETED, context);
      },
    },
    abort: (_task, runtime, context) =>
      runtime.commit(() => ({ status: "terminal", outcome: { status: "aborted" } }), context),
  });
}
