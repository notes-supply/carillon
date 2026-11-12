/**
 * Settles each turn the runtime holds: a background task of the turn's
 * conversation waits for its message to be answered or given up on,
 * decides the outcome once, and posts it until core has recorded it. A
 * restart resumes either phase where it stood.
 */
import type { Context, JsonValue } from "@earendil-works/chord";
import {
  AgentDoc,
  AssistantEntry,
  defineTask,
  type ConversationId,
  type Cursor,
  type EntryId,
  type RunningTask,
  type SettledSubmissionRecord,
  type SubmissionId,
  type SubmissionRecord,
  type TaskId,
  type TaskRecord,
  type TaskRuntime,
  type Tx,
} from "@earendil-works/pi-durable";
import type { TurnActivity, TurnOutcome } from "@carillon/protocol";
import { finalReport } from "@carillon/runtime/activity";
import { CoreRefusal, type Settled } from "@carillon/runtime/core";
import { diagnostic, displayableExplanation, redactCredentials } from "@carillon/runtime/failure";
import { noModel } from "@carillon/runtime/settings";
import { Type } from "typebox";
import { Check } from "typebox/value";
import { runEnds, takeAttached, type RunEnds } from "./answers.ts";
import { ConversationInfo, MESSAGE } from "./documents.ts";

/**
 * What a turn the runtime opened runs for: it is over once `task` is
 * cancelled or the agent no longer selects `extension`.
 */
export interface Needs {
  readonly task: TaskId;
  readonly extension: string;
}

export interface DeliveryInput {
  readonly turn: string;
  readonly interface_id: string;
  readonly conversation: ConversationId;
  readonly submission: SubmissionId;
  /** The runtime opened the turn, so no event of an interface addresses it. */
  readonly opened?: true;
  /** An opened turn is answered only while what it runs for is not over. */
  readonly needs?: Needs;
}

const DeliveryInputSchema = Type.Object({
  turn: Type.String(),
  interface_id: Type.String(),
  conversation: Type.Integer(),
  submission: Type.Integer(),
  opened: Type.Optional(Type.Literal(true)),
  needs: Type.Optional(Type.Object({ task: Type.Integer(), extension: Type.String() })),
});

/** A delivery task's input, as its stored record holds it. */
export function isDeliveryInput(value: unknown): value is DeliveryInput {
  return Check(DeliveryInputSchema, value);
}

type DeliveryState =
  | { readonly phase: "wait"; readonly cutOff?: number }
  | {
      readonly phase: "post";
      readonly outcome: TurnOutcome;
      readonly attempt: number;
      /** Core refused the decided outcome for good, so this is the plain failure in its place. */
      readonly replaced?: true;
      readonly cutOff?: number;
    };

/** What the delivery task needs of the runtime that runs it. */
export interface DeliveryHost {
  /** Resolves once the turn's message is answered or given up on. */
  wait(submission: SubmissionId, context: Context): Promise<SettledSubmissionRecord>;
  /** The submission's record as it stands; undefined when there is none. */
  status(submission: SubmissionId, context: Context): Promise<SubmissionRecord | undefined>;
  /** What the turn did, as far as this process watched it. */
  report(turn: string): TurnActivity | undefined;
  /** Whether the agent selects the extension; undefined while that is unknown. */
  selects(extension: string): boolean | undefined;
  settle(turn: string, outcome: TurnOutcome): Promise<Settled>;
  /** Core has the turn's outcome: the runtime holds the turn no longer. */
  released(turn: string): void;
}

const LONGEST_BACKOFF_MS = 60_000;
/** What is posted once in place of an outcome core refused for good. */
const UNDELIVERABLE: TurnOutcome = {
  outcome: "failed",
  explanation: "Carillon could not deliver this answer.",
};

export const DELIVERY = "carillon.delivery";

export function deliveryTask(host: DeliveryHost) {
  /** Whether an opened turn's work is over; an unknown selection keeps it. */
  async function ended(tx: Tx, needs: Needs | undefined): Promise<boolean> {
    if (needs === undefined) return false;
    return over(host.selects(needs.extension) ?? true, await tx.task(needs.task));
  }

  return defineTask<DeliveryInput, DeliveryState, null>({
    name: DELIVERY,
    version: 1,
    initial: () => ({ phase: "wait" }),
    phases: {
      wait: async (task, runtime, context) => {
        await recordCutOffs(task, runtime, context);
        const settled = await host.wait(task.input.submission, context);
        if (settled.status === "unanswered" && settled.detail !== undefined)
          console.error(
            `turn ${task.input.turn} failed: ${redactCredentials(textOf(settled.detail))}`,
          );
        const ends = await runEnds(runtime, host, task.input.conversation, context);
        await runtime.commit(async (tx) => {
          const decided = await decide(tx, task.input, settled, ends, host.report(task.input.turn));
          // An answer can come before the opening stops a turn whose work ended.
          const outcome: TurnOutcome =
            decided.outcome === "answered" && (await ended(tx, task.input.needs))
              ? { outcome: "stopped" }
              : decided;
          return { status: "running", checkpoint: { phase: "post", outcome, attempt: 0 } };
        }, context);
      },
      post: async (task, runtime, context) => {
        const { turn } = task.input;
        const checkpoint = await recordCutOffs(task, runtime, context);
        const { outcome, attempt, replaced } = checkpoint;
        try {
          await host.settle(turn, outcome);
        } catch (failure) {
          if (!refusedForGood(failure)) {
            const delay = backoff(attempt);
            console.error(
              `turn ${turn}: ${diagnostic(failure)}; posting again in ${String(delay / 1_000)} s`,
            );
            const next = { ...checkpoint, attempt: attempt + 1 };
            await runtime.sleepUntil(runtime.now() + delay, next, context);
            return;
          }
          if (replaced === undefined) {
            console.error(`turn ${turn}: ${diagnostic(failure)}; posting a plain failure instead`);
            const checkpoint = {
              phase: "post",
              outcome: UNDELIVERABLE,
              attempt: 0,
              replaced: true,
            } satisfies DeliveryState;
            await runtime.commit(() => ({ status: "running", checkpoint }), context);
            return;
          }
          console.error(`turn ${turn}: ${diagnostic(failure)}; giving its outcome up`);
        }
        await runtime.commit(
          () => ({ status: "terminal", outcome: { status: "completed", result: null } }),
          context,
        );
        host.released(turn);
      },
    },
    abort: (_task, runtime, context) =>
      runtime.commit(() => ({ status: "terminal", outcome: { status: "aborted" } }), context),
  });
}

/**
 * The turn's outcome from how its message settled. Steers placed into one
 * run settle with that run's answer; the newest of them carries it, and
 * the others were steered into it. The message that carries a run's
 * answer takes the files the run attached, which go with it only when it
 * is answered; a run that ended without an answer drops its files.
 */
async function decide(
  tx: Tx,
  input: DeliveryInput,
  settled: SettledSubmissionRecord,
  ends: RunEnds,
  watched: TurnActivity | undefined,
): Promise<TurnOutcome> {
  const activity = watched === undefined ? {} : { activity: finalReport(watched) };
  if (settled.status === "unanswered") {
    await takeAttached(tx, input.conversation, ends, null);
    switch (settled.reason) {
      case "aborted":
      case "reset":
        return { outcome: "stopped" };
      case "no_model":
        return { outcome: "failed", explanation: await modelMissing(tx, input), ...activity };
      case "restart_limit":
      case "interrupted":
        return {
          outcome: "failed",
          explanation:
            "This message stopped the agent's runtime several times, so it was given up.",
          ...activity,
        };
      default: {
        const explanation =
          settled.detail === undefined ? undefined : displayableExplanation(textOf(settled.detail));
        return {
          outcome: "failed",
          ...(explanation === undefined ? {} : { explanation }),
          ...activity,
        };
      }
    }
  }
  if (settled.type !== "input") return { outcome: "failed", ...activity };
  if (await carriedLater(tx, input.conversation, settled.entry, settled.answer))
    return { outcome: "steered" };
  const files = await takeAttached(tx, input.conversation, ends, settled.answer);
  const message = (await tx.entry(AssistantEntry, settled.answer))?.model?.[0];
  if (message?.role !== "assistant") return { outcome: "failed", ...activity };
  const text = message.content
    .flatMap((part) => (part.type === "text" ? [part.text] : []))
    .join("");
  const output =
    message.stopReason === "length"
      ? `${text}\n\n[The answer was cut off at the model's output limit.]`
      : text;
  if (output.trim().length === 0)
    return { outcome: "failed", explanation: "The model gave no answer.", ...activity };
  return { outcome: "answered", output, ...(files.length === 0 ? {} : { files }), ...activity };
}

/** Whether a later message was placed into the run before its answer. */
async function carriedLater(
  tx: Tx,
  conversationId: ConversationId,
  entry: EntryId,
  answer: EntryId,
): Promise<boolean> {
  let cursor: Cursor | undefined;
  do {
    const page = await tx.scanEntries(
      { conversationId, minEntryId: entry, maxEntryId: answer, order: "ascending" },
      100,
      cursor,
    );
    if (page.items.some((item) => item.id !== entry && item.kind === MESSAGE)) return true;
    cursor = page.next;
  } while (cursor !== undefined);
  return false;
}

/** Whether the work is over: its extension deselected, or its task cancelled or gone. */
export function over(
  selected: boolean,
  task: TaskRecord<JsonValue, JsonValue, unknown> | undefined,
): boolean {
  return (
    !selected ||
    task === undefined ||
    task.abortRequested ||
    task.state.outcome?.status === "aborted"
  );
}

/**
 * The phase's checkpoint, recording how often crashes cut the phase off.
 * Pi ends a task cut off `restartLimit` times without progress, and a
 * changed checkpoint is progress, so crashes while a phase waits on a run
 * or on core never end its task.
 */
export async function recordCutOffs<
  I,
  P extends S & { readonly cutOff?: number },
  S,
  R,
  H extends object,
>(task: RunningTask<I, P, R>, runtime: TaskRuntime<I, S, R, H>, context: Context): Promise<P> {
  const { checkpoint } = task.state;
  const restarts = task.restarts ?? 0;
  if (restarts === 0) return checkpoint;
  const recorded = { ...checkpoint, cutOff: (checkpoint.cutOff ?? 0) + restarts };
  await runtime.commit(() => ({ status: "running", checkpoint: recorded }), context);
  return recorded;
}

/** How long to wait before asking core again once `attempt` earlier tries failed. */
export function backoff(attempt: number): number {
  return Math.min(1_000 * 2 ** attempt, LONGEST_BACKOFF_MS);
}

/**
 * Whether asking core again cannot change its answer: a client error other
 * than an unready credential, a timeout or a rate limit. An unreachable or
 * failing core may record the outcome later.
 */
export function refusedForGood(failure: unknown): boolean {
  return (
    failure instanceof CoreRefusal &&
    failure.status >= 400 &&
    failure.status < 500 &&
    ![401, 403, 408, 429].includes(failure.status)
  );
}

async function modelMissing(tx: Tx, input: DeliveryInput): Promise<string> {
  const { model } = await tx.doc(AgentDoc, input.conversation);
  if (model !== undefined)
    return `${model.provider}/${model.modelId} is not available. Choose another model with /settings.`;
  return noModel((await tx.doc(ConversationInfo, input.conversation)).agent_id);
}

function textOf(detail: JsonValue): string {
  return typeof detail === "string" ? detail : JSON.stringify(detail);
}
