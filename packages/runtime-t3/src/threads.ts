/** Native thread identity and project ownership, read from T3 rather than stored locally. */
import { ExplainedFailure } from "@carillon/runtime/failure";
import type { TurnRequest } from "@carillon/runtime/operations";
import { Check } from "typebox/value";
import {
  RPC,
  derivedId,
  subscribeThread,
  threadCreate,
  ThreadSnapshotSchema,
  type NativeSelection,
  type Projection,
  type ThreadBinding,
} from "./contracts.ts";
import { T3Failure, type T3Connection, type T3Stream } from "./t3.ts";
import type { TurnWatch } from "./watch.ts";

type Conversation = Pick<TurnRequest, "interface_id" | "agent_id" | "audience_id">;

/** The stable queue and canonical thread identity of a channel or private conversation. */
export function conversationId(request: Conversation): string {
  return derivedId("thread", request.interface_id, request.agent_id, request.audience_id);
}

/** Resolves native project ownership; only a message supplies `start` to acknowledge a cutover. */
export async function resolveThread(
  connection: T3Connection,
  request: Conversation,
  context: Omit<ThreadBinding, "threadId">,
  options: {
    readonly timeoutMs: number;
    readonly signal?: AbortSignal;
    readonly start?: () => Promise<NativeSelection>;
  },
): Promise<ThreadBinding> {
  const { timeoutMs, signal } = options;
  const canonical = { ...context, threadId: conversationId(request) };
  const original = await readProjection(connection, canonical.threadId, timeoutMs, signal);
  if (original === undefined || original.thread.projectId === canonical.projectId) return canonical;
  const scoped = {
    ...context,
    threadId: derivedId(
      "thread",
      context.projectId,
      request.interface_id,
      request.agent_id,
      request.audience_id,
    ),
  };
  if ((await readThread(connection, scoped, timeoutMs, signal)) !== undefined) return scoped;
  if (options.start !== undefined)
    await ensureThread(connection, scoped, await options.start(), timeoutMs, signal);
  throw projectChanged();
}

function projectChanged(): ExplainedFailure {
  return new ExplainedFailure(
    409,
    "This conversation ran in a T3 project this agent no longer works in. Your next message starts a new conversation in its current project.",
  );
}

/** Refuses another thread's records or a conversation from the agent's previous project. */
function checkThread(binding: ThreadBinding, thread: Projection["thread"]): void {
  if (thread.id !== binding.threadId)
    throw new T3Failure("malformed", "T3 sent the snapshot of another thread");
  if (thread.projectId !== binding.projectId) throw projectChanged();
}

/** Reads a resolved thread, refusing a project change since it was bound. */
export async function readThread(
  connection: T3Connection,
  binding: ThreadBinding,
  timeoutMs: number,
  signal?: AbortSignal,
): Promise<Projection | undefined> {
  const projection = await readProjection(connection, binding.threadId, timeoutMs, signal);
  if (projection !== undefined) checkThread(binding, projection.thread);
  return projection;
}

/**
 * The thread's current record, read from the first item of a fresh
 * subscription; undefined when T3 has no such thread. The deadline is
 * armed once the subscription exists, so one that fails as it is set up
 * leaves nothing behind and rejects with its own failure.
 */
function readProjection(
  connection: T3Connection,
  threadId: string,
  timeoutMs: number,
  signal?: AbortSignal,
): Promise<Projection | undefined> {
  signal?.throwIfAborted();
  return new Promise((resolve, reject) => {
    const cleanup = () => {
      clearTimeout(timer);
      signal?.removeEventListener("abort", aborted);
      stream.end();
    };
    const aborted = () => {
      cleanup();
      reject(asError(signal?.reason));
    };
    const stream = connection.subscribe(
      RPC.subscribeThread,
      subscribeThread(threadId, undefined),
      (item) => {
        cleanup();
        if (!Check(ThreadSnapshotSchema, item))
          reject(
            new T3Failure(
              "malformed",
              "T3 sent a thread snapshot this runtime does not understand",
            ),
          );
        else {
          try {
            if (item.projection.thread.id !== threadId)
              throw new T3Failure("malformed", "T3 sent the snapshot of another thread");
            resolve(item.projection);
          } catch (failure) {
            reject(failure);
          }
        }
      },
    );
    const timer = setTimeout(() => {
      cleanup();
      reject(new T3Failure("timeout", "T3 did not send the thread snapshot in time"));
    }, timeoutMs);
    signal?.addEventListener("abort", aborted, { once: true });
    stream.done.then(
      () => {
        cleanup();
        reject(new T3Failure("disconnected", "the thread subscription ended"));
      },
      (failure: unknown) => {
        cleanup();
        if (missingThread(failure)) resolve(undefined);
        else reject(asError(failure));
      },
    );
  });
}

/** Opens a resolved conversation thread on its default model unless T3 already has it. */
export async function ensureThread(
  connection: T3Connection,
  binding: ThreadBinding,
  selection: NativeSelection,
  timeoutMs: number,
  signal?: AbortSignal,
): Promise<void> {
  if ((await readThread(connection, binding, timeoutMs, signal)) !== undefined) return;
  signal?.throwIfAborted();
  try {
    await connection.call(RPC.dispatchCommand, threadCreate(binding, selection));
  } catch (failure) {
    if (failure instanceof T3Failure && failure.kind === "rejected")
      throw new ExplainedFailure(
        502,
        `T3 refused to open the conversation thread: ${failure.detail ?? failure.message}`,
      );
    throw failure;
  }
}

/**
 * Subscribes to the thread from where the watch left off: a resumed
 * subscription replays only the events after the last folded sequence, so
 * neither history nor the turn's identity is reloaded; a first or too-old
 * one gets T3's bounded snapshot.
 */
export function followThread(
  connection: T3Connection,
  binding: ThreadBinding,
  watch: TurnWatch,
): T3Stream {
  watch.resubscribing();
  let failure: Error | undefined;
  const stream = connection.subscribe(
    RPC.subscribeThread,
    subscribeThread(binding.threadId, watch.sequence()),
    (item) => {
      if (failure !== undefined) return;
      try {
        if (Check(ThreadSnapshotSchema, item)) checkThread(binding, item.projection.thread);
        watch.apply(item);
      } catch (error) {
        failure = asError(error);
        stream.end();
      }
    },
  );
  return {
    done: stream.done.then(() => {
      if (failure !== undefined) throw failure;
    }),
    end: () => stream.end(),
  };
}

function asError(failure: unknown): Error {
  return failure instanceof Error ? failure : new Error(String(failure));
}

/** T3's answer to a subscription for a thread it has no record of. */
function missingThread(failure: unknown): boolean {
  return (
    failure instanceof T3Failure &&
    failure.kind === "rejected" &&
    failure.tag === "OrchestrationV2GetThreadProjectionError" &&
    (failure.detail ?? "").includes("No orchestration projection exists")
  );
}
