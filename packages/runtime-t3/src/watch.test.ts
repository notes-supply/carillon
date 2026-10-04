import { expect, it } from "vitest";
import { watchTurn } from "./watch.ts";

/** Synthetic projection fixtures; shapes follow the live server's protocol 2 answers. */
const run = (
  status: string,
  userMessageId = "message-1",
  ordinal = 1,
  id = `run-${String(ordinal)}`,
) => ({
  id,
  threadId: "thread",
  ordinal,
  userMessageId,
  providerThreadId: "provider-thread",
  status,
});

const item = (
  type: string,
  runId: string,
  fields: Record<string, unknown>,
  status = "completed",
) => ({
  id: typeof fields["id"] === "string" ? fields["id"] : `${type}-${runId}`,
  runId,
  type,
  status,
  ordinal: 0,
  title: null,
  startedAt: "2026-10-04T08:00:00Z",
  completedAt: status === "completed" ? "2026-10-04T08:00:01Z" : null,
  updatedAt: "2026-10-04T08:00:01Z",
  ...fields,
});

function snapshot(overrides: Record<string, unknown>, sequence = 1) {
  return {
    kind: "snapshot",
    snapshotSequence: sequence,
    projection: {
      thread: { id: "thread" },
      runs: [],
      runtimeRequests: [],
      providerThreads: [{ id: "provider-thread", providerSessionId: "session" }],
      providerSessions: [{ id: "session", status: "ready" }],
      messages: [],
      turnItems: [],
      ...overrides,
    },
  };
}

const event = (sequence: number, type: string, payload: unknown) => ({
  kind: "event",
  sequence,
  event: { type, payload },
});

it("knows the turn's run by its message, keeps the answer in message order, and ignores other turns", () => {
  const watch = watchTurn("message-1", "/workspace");
  expect(watch.synchronized()).toBe(false);
  watch.apply(
    snapshot({
      runs: [run("completed", "other", 1), run("completed", "message-1", 2)],
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
      turnItems: [item("command_execution", "run-1", { input: "old work" })],
    }),
  );
  expect(watch.synchronized()).toBe(true);
  expect(watch.sequence()).toBe(1);
  expect(watch.run()?.id).toBe("run-2");
  expect(watch.blocking()).toEqual([]);
  expect(watch.outcome()).toEqual({ kind: "completed", output: "first\n\nsecond" });
  expect(watch.retained()).toEqual({
    runs: 1,
    running: 0,
    recent: 0,
    counted: 0,
    requests: 0,
    messages: 2,
  });
});

it("retains only the turn's own work from a long history and keeps its identity through a resumed snapshot", () => {
  const watch = watchTurn("message-500", "/workspace");
  const olderRuns = Array.from({ length: 500 }, (_, index) =>
    run(index === 7 ? "running" : "completed", `message-${String(index)}`, index),
  );
  watch.apply(
    snapshot({
      runs: [...olderRuns, run("running", "message-500", 500)],
      messages: olderRuns.map((older) => ({
        id: `assistant-${older.id}`,
        runId: older.id,
        role: "assistant",
        text: "historic",
        createdAt: "2026-10-01T00:00:00Z",
      })),
      turnItems: olderRuns.flatMap((older) => [
        item("command_execution", older.id, { id: `${older.id}-a`, input: "echo old" }),
        item("command_execution", older.id, { id: `${older.id}-b`, input: "echo older" }),
      ]),
    }),
  );
  // Only the current turn's run and the one earlier run still active are kept.
  expect(watch.retained()).toEqual({
    runs: 2,
    running: 0,
    recent: 0,
    counted: 0,
    requests: 0,
    messages: 0,
  });
  expect(watch.blocking().map((blocking) => blocking.id)).toEqual(["run-7"]);
  expect(watch.activity()).toEqual({ phase: "model", running: [], recent: [], tool_calls: 0 });

  // A run evicted from a later snapshot does not lose the turn's identity.
  watch.resubscribing();
  watch.apply(snapshot({ runs: [run("completed", "message-501", 501)] }, 900));
  expect(watch.run()?.id).toBe("run-500");
  expect(watch.sequence()).toBe(900);
  // Replayed events at or below the folded sequence are ignored.
  watch.apply(event(900, "run.updated", run("running", "message-500", 500)));
  expect(watch.run()?.status).toBe("running");
  expect(watch.activeRun()).toBeUndefined();
});

it("bounds finished tools to the recent window while counting every call", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("running")] }));
  for (let index = 0; index < 40; index += 1)
    watch.apply(
      event(
        2 + index,
        "turn-item.updated",
        item("command_execution", "run-1", {
          id: `call-${String(index)}`,
          input: `echo ${String(index)}`,
        }),
      ),
    );
  watch.apply(
    event(
      50,
      "turn-item.updated",
      item(
        "dynamic_tool",
        "run-1",
        { id: "live", toolName: "Read", input: { file_path: "/workspace/a.txt" } },
        "running",
      ),
    ),
  );
  expect(watch.retained()).toEqual({
    runs: 1,
    running: 1,
    recent: 5,
    counted: 41,
    requests: 0,
    messages: 0,
  });
  const activity = watch.activity();
  expect(activity.tool_calls).toBe(41);
  expect(activity.recent.map((tool) => tool.detail)).toEqual([
    "echo 35",
    "echo 36",
    "echo 37",
    "echo 38",
    "echo 39",
  ]);
  expect(activity.running).toMatchObject([{ tool: "Read", detail: "a.txt" }]);
  expect(typeof activity.running[0]?.started_at).toBe("number");
});

it("is not finished while background work, an owed wake or a pending request remains", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("completed")],
      providerThreads: [
        {
          id: "provider-thread",
          providerSessionId: "session",
          pendingBackgroundTasks: [{ taskId: "t", kind: "monitor" }],
        },
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
  expect(watch.quiet()).toBe(false);
  expect(watch.activity()).toMatchObject({ phase: "tools", running: [{ tool: "monitor" }] });
  // The roster empties: the process owes a wake, so the turn still waits.
  watch.apply(
    event(2, "provider-thread.updated", {
      id: "provider-thread",
      providerSessionId: "session",
      pendingBackgroundTasks: [],
    }),
  );
  expect(watch.outcome()).toBeUndefined();
  expect(watch.quiet()).toBe(true);
  // The wake arrives and completes.
  watch.apply(event(3, "run.created", run("running", "wake", 2)));
  expect(watch.activeRun()?.id).toBe("run-2");
  watch.apply(
    event(4, "message.updated", {
      id: "w",
      runId: "run-2",
      role: "assistant",
      text: "result",
      createdAt: "2026-10-03T10:00:05Z",
    }),
  );
  watch.apply(event(5, "run.updated", run("completed", "wake", 2)));
  expect(watch.outcome()).toEqual({ kind: "completed", output: "done\n\nresult" });

  watch.apply(event(6, "run.updated", run("waiting", "wake", 2)));
  watch.apply(
    event(7, "runtime-request.updated", {
      id: "request-1",
      kind: "tool_approval",
      status: "pending",
    }),
  );
  expect(watch.outcome()).toEqual({
    kind: "blocked",
    asked: "T3 needs an approval (tool_approval) this runtime cannot give yet",
  });
  expect(watch.requestOpen()).toBe(true);
});

it("fails closed when the provider session that owes a wake is gone", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("completed")],
      providerThreads: [
        {
          id: "provider-thread",
          providerSessionId: "session",
          pendingBackgroundTasks: [{ taskId: "t", kind: "command" }],
        },
      ],
    }),
  );
  watch.apply(
    event(2, "provider-thread.updated", {
      id: "provider-thread",
      providerSessionId: "session",
      pendingBackgroundTasks: [],
    }),
  );
  expect(watch.outcome()).toBeUndefined();
  watch.apply(event(3, "provider-session.detached", { providerSessionId: "session" }));
  expect(watch.outcome()).toEqual({
    kind: "failed",
    explanation:
      "T3's native session ended before it reported the result of its finished background work.",
  });
});

it("gates completion on the turn's own open request even when its run is terminal", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("completed")],
      runtimeRequests: [{ id: "request-1", kind: "tool_approval", status: "pending" }],
      turnItems: [
        item(
          "approval_request",
          "run-1",
          { requestId: "request-1", requestKind: "command" },
          "waiting",
        ),
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
  expect(watch.outcome()).toEqual({
    kind: "blocked",
    asked: "T3 needs an approval (command) this runtime cannot give yet",
  });
  expect(watch.quiet()).toBe(true);
  expect(watch.requestOpen()).toBe(true);
  // Another thread's pending request does not block a turn whose run is not waiting.
  const other = watchTurn("message-1", "/workspace");
  other.apply(
    snapshot({
      runs: [run("completed")],
      runtimeRequests: [{ id: "request-9", kind: "tool_approval", status: "pending" }],
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
  expect(other.outcome()).toEqual({ kind: "completed", output: "done" });
});

it("explains failures with T3's failure text and treats other stops as stopped", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("failed")],
      turnItems: [
        item(
          "error",
          "run-1",
          { failure: { class: "usage_limit", message: "Usage limit reached" } },
          "failed",
        ),
      ],
    }),
  );
  expect(watch.outcome()).toEqual({ kind: "failed", explanation: "Usage limit reached" });
  watch.apply(event(2, "run.updated", run("cancelled")));
  expect(watch.outcome()).toEqual({ kind: "stopped", status: "cancelled" });
  watch.apply(event(3, "run.created", run("running", "other", 0, "run-9")));
  expect(watch.blocking().map((blocking) => blocking.id)).toEqual(["run-9"]);
});

it("refuses known kinds it cannot read, ignores unknown kinds and malformed stream items it does not need", () => {
  const watch = watchTurn("message-1", "/workspace");
  expect(() =>
    watch.apply({ kind: "snapshot", snapshotSequence: 1, projection: { runs: "no" } }),
  ).toThrow("T3 sent a thread snapshot this runtime does not understand");
  watch.apply(snapshot({ runs: [run("running")] }));
  expect(() => watch.apply(event(2, "run.updated", { id: "run-1", status: "sleeping" }))).toThrow(
    "T3 sent a run this runtime does not understand",
  );
  expect(() =>
    watch.apply(
      event(
        3,
        "turn-item.updated",
        item("user_input_request", "run-1", { requestId: "r" }, "waiting"),
      ),
    ),
  ).toThrow("T3 sent a user_input_request item this runtime does not understand");
  expect(() =>
    watch.apply(event(4, "turn-item.updated", item("command_execution", "run-1", {}))),
  ).toThrow("T3 sent a command_execution item this runtime does not understand");
  expect(() =>
    watch.apply(event(5, "turn-item.updated", item("reasoning", "run-1", { text: "x" }))),
  ).not.toThrow();
  expect(() =>
    watch.apply(event(6, "turn-item.updated", item("hologram", "run-1", {}))),
  ).not.toThrow();
  expect(() => watch.apply({ kind: "future" })).not.toThrow();
  expect(() => watch.apply(event(7, "future.event", { anything: true }))).not.toThrow();
  expect(watch.retained().counted).toBe(0);
});
