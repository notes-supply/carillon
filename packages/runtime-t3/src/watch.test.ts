import { expect, it } from "vitest";
import { watchTurn } from "./watch.ts";

const run = (status: string, userMessageId = "message-1") => ({
  id: "run-1",
  threadId: "thread",
  ordinal: 1,
  userMessageId,
  providerThreadId: "provider-thread",
  status,
});

function snapshot(overrides: Record<string, unknown>) {
  return {
    kind: "snapshot",
    snapshotSequence: 1,
    projection: {
      thread: { id: "thread", projectId: "project" },
      runs: [],
      runtimeRequests: [],
      providerThreads: [{ id: "provider-thread" }],
      messages: [],
      turnItems: [],
      ...overrides,
    },
  };
}

const event = (type: string, payload: unknown) => ({
  kind: "event",
  sequence: 2,
  event: { type, payload },
});

it("knows the turn's run by its message and keeps the answer in message order", () => {
  const watch = watchTurn("message-1", "/workspace");
  expect(watch.synchronized()).toBe(false);
  watch.apply(
    snapshot({
      runs: [run("completed", "other"), { ...run("completed"), id: "run-2", ordinal: 2 }],
      messages: [
        {
          id: "b",
          runId: "run-2",
          role: "assistant",
          text: "second",
          createdAt: "2026-10-03T10:00:02Z",
        },
        {
          id: "a",
          runId: "run-2",
          role: "assistant",
          text: " first ",
          createdAt: "2026-10-03T10:00:01Z",
        },
        {
          id: "x",
          runId: "run-1",
          role: "assistant",
          text: "other turn",
          createdAt: "2026-10-03T09:00:00Z",
        },
      ],
    }),
  );
  expect(watch.synchronized()).toBe(true);
  expect(watch.run()?.id).toBe("run-2");
  expect(watch.blocking()).toEqual([]);
  expect(watch.outcome()).toEqual({ kind: "completed", output: "first\n\nsecond" });
});

it("is not finished while background work or a pending request remains", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("completed")],
      providerThreads: [
        { id: "provider-thread", pendingBackgroundTasks: [{ taskId: "t", kind: "monitor" }] },
      ],
      messages: [
        {
          id: "a",
          runId: "run-1",
          role: "assistant",
          text: "done",
          createdAt: "2026-10-03T10:00:01Z",
        },
      ],
    }),
  );
  expect(watch.outcome()).toBeUndefined();
  expect(watch.activity()).toMatchObject({ phase: "tools", running: [{ tool: "monitor" }] });
  watch.apply(
    event("provider-thread.updated", { id: "provider-thread", pendingBackgroundTasks: [] }),
  );
  expect(watch.outcome()).toEqual({ kind: "completed", output: "done" });

  watch.apply(event("run.updated", run("waiting")));
  watch.apply(
    event("runtime-request.updated", { id: "request-1", kind: "tool_approval", status: "pending" }),
  );
  expect(watch.outcome()).toEqual({
    kind: "blocked",
    explanation:
      "T3 needs an approval (tool_approval) this runtime cannot give yet; the run was stopped.",
  });
});

it("explains failures with T3's failure text and treats other stops as stopped", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("failed")],
      turnItems: [
        {
          id: "e",
          runId: "run-1",
          type: "error",
          status: "failed",
          ordinal: 0,
          title: null,
          startedAt: null,
          completedAt: null,
          updatedAt: "2026-10-03T10:00:00Z",
          failure: { class: "usage_limit", message: "Usage limit reached" },
        },
      ],
    }),
  );
  expect(watch.outcome()).toEqual({ kind: "failed", explanation: "Usage limit reached" });
  watch.apply(event("run.updated", run("cancelled")));
  expect(watch.outcome()).toEqual({ kind: "stopped", status: "cancelled" });
  expect(watch.blocking()).toEqual([]);
  watch.apply(event("run.created", { ...run("running", "other"), id: "run-9", ordinal: 0 }));
  expect(watch.blocking().map((blocking) => blocking.id)).toEqual(["run-9"]);
});

it("refuses items it does not understand instead of guessing", () => {
  const watch = watchTurn("message-1", "/workspace");
  expect(() => watch.apply({ kind: "snapshot", projection: { runs: "no" } })).toThrow(
    "T3 sent a thread snapshot this runtime does not understand",
  );
  watch.apply(snapshot({}));
  expect(() => watch.apply(event("run.updated", { id: "run-1", status: "sleeping" }))).toThrow(
    "T3 sent a run this runtime does not understand",
  );
  expect(() => watch.apply({ kind: "future" })).not.toThrow();
  expect(() => watch.apply(event("future.event", { anything: true }))).not.toThrow();
});
