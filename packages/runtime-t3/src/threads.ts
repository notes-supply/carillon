/** Native thread identity and project ownership, read from T3 rather than stored locally. */
import { ExplainedFailure } from "@carillon/runtime/failure";
import { Check } from "typebox/value";
import {
  RPC,
  subscribeThread,
  ThreadSnapshotSchema,
  type Projection,
  type ThreadBinding,
} from "./contracts.ts";
import { T3Failure, type T3Connection, type T3Stream } from "./t3.ts";
import type { TurnWatch } from "./watch.ts";

/** Refuses another thread's records or a conversation from the agent's previous project. */
function checkThread(binding: ThreadBinding, thread: Projection["thread"]): void {
  if (thread.id !== binding.threadId)
    throw new T3Failure("malformed", "T3 sent the snapshot of another thread");
  if (thread.projectId !== binding.projectId)
    throw new ExplainedFailure(
      409,
      "This conversation ran in a T3 project this agent no longer works in. Start a new conversation.",
    );
}

/**
 * The thread's current record, read from the first item of a fresh
 * subscription; undefined when T3 has no such thread. The deadline is
 * armed once the subscription exists, so one that fails as it is set up
 * leaves nothing behind and rejects with its own failure.
 */
export function readThread(
  connection: T3Connection,
  binding: ThreadBinding,
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
      subscribeThread(binding.threadId, undefined),
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
            checkThread(binding, item.projection.thread);
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
