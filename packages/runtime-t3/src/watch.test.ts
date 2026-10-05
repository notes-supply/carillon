import { TurnActivitySchema } from "@carillon/protocol";
import { Check } from "typebox/value";
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
      thread: {
        id: "thread",
        modelSelection: { instanceId: "claudeAgent", model: "claude-sonnet-5" },
      },
      runs: [],
      runtimeRequests: [],
      providerThreads: [{ id: "provider-thread", providerSessionId: "session" }],
      providerSessions: [{ id: "session", status: "ready" }],
      messages: [],
      turnItems: [],
      subagents: [],
      contextTransfers: [],
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
  expect(watch.busy()).toBe(false);
  expect(watch.outcome()).toEqual({ kind: "completed", output: "first\n\nsecond" });
  expect(watch.retained()).toEqual({
    runs: 1,
    running: 0,
    recent: 0,
    counted: 0,
    requests: 0,
    messages: 2,
    providerThreads: 1,
    sessions: 1,
    pendingProviderRecords: 0,
    detachedSessions: 0,
    delegations: 0,
    transfers: 0,
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
    providerThreads: 1,
    sessions: 1,
    pendingProviderRecords: 0,
    detachedSessions: 0,
    delegations: 0,
    transfers: 0,
  });
  expect(watch.busy()).toBe(true);
  expect(watch.activity()).toEqual({ phase: "model", running: [], recent: [], tool_calls: 0 });

  // A snapshot lists runs whole: one that omits the observed turn run is refused
  // before it replaces anything, so the fold keeps what it knew.
  watch.resubscribing();
  expect(() =>
    watch.apply(snapshot({ runs: [run("completed", "message-501", 501)] }, 900)),
  ).toThrow("T3 sent a thread snapshot that omits run run-500 this turn observed");
  expect(watch.run()?.id).toBe("run-500");
  expect(watch.activeRun()?.id).toBe("run-500");
  expect(watch.sequence()).toBe(1);
  // A snapshot that lists the observed run resumes the fold, and replayed
  // events at or below the folded sequence are ignored.
  watch.apply(
    snapshot(
      { runs: [run("completed", "message-500", 500), run("completed", "message-501", 501)] },
      900,
    ),
  );
  expect(watch.run()?.id).toBe("run-500");
  expect(watch.sequence()).toBe(900);
  watch.apply(event(900, "run.updated", run("running", "message-500", 500)));
  expect(watch.run()?.status).toBe("completed");
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
    providerThreads: 1,
    sessions: 1,
    pendingProviderRecords: 0,
    detachedSessions: 0,
    delegations: 0,
    transfers: 0,
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
  // The owed report keeps the turn non-quiet until the wake arrives.
  expect(watch.quiet()).toBe(false);
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
  // Pending alone is unknown; the wake run's own item names the request.
  expect(watch.outcome()).toBeUndefined();
  watch.apply(
    event(
      8,
      "turn-item.updated",
      item(
        "approval_request",
        "run-2",
        { requestId: "request-1", requestKind: "tool_approval" },
        "waiting",
      ),
    ),
  );
  expect(watch.outcome()).toEqual({
    kind: "blocked",
    asked: "T3 needs an approval (tool_approval) this runtime cannot give yet",
  });
  expect(watch.requestOpen()).toBe(true);
});

it("fails closed when the provider session that owes a wake stops or is detached, saying which", () => {
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
  watch.apply(event(3, "provider-session.updated", { id: "session", status: "stopped" }));
  expect(watch.outcome()).toEqual({
    kind: "failed",
    explanation:
      "T3's native session stopped before it reported the result of its finished background work.",
  });
  expect(watch.quiet()).toBe(true);

  const detached = watchTurn("message-1", "/workspace");
  detached.apply(
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
  detached.apply(
    event(2, "provider-thread.updated", {
      id: "provider-thread",
      providerSessionId: "session",
      pendingBackgroundTasks: [],
    }),
  );
  detached.apply(event(3, "provider-session.detached", { providerSessionId: "session" }));
  expect(detached.outcome()).toEqual({
    kind: "failed",
    explanation:
      "T3's native session was detached before it reported the result of its finished background work; T3 does not confirm whether that result will still arrive.",
  });
  // Detachment removes the record before the process closes: not a confirmed stop.
  expect(detached.quiet()).toBe(false);
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
  expect(watch.quiet()).toBe(true);
  // Work still listed for a failed or stopped run keeps the turn terminal but not quiet.
  const listed = (sequence: number, tasks: readonly { taskId: string; kind: string }[]) =>
    event(sequence, "provider-thread.updated", {
      id: "provider-thread",
      providerSessionId: "session",
      pendingBackgroundTasks: tasks,
    });
  watch.apply(listed(2, [{ taskId: "t", kind: "command" }]));
  expect(watch.outcome()).toEqual({ kind: "failed", explanation: "Usage limit reached" });
  expect(watch.quiet()).toBe(false);
  watch.apply(event(3, "run.updated", run("cancelled")));
  expect(watch.outcome()).toEqual({ kind: "stopped", status: "cancelled" });
  expect(watch.quiet()).toBe(false);
  watch.apply(listed(4, []));
  expect(watch.outcome()).toEqual({ kind: "stopped", status: "cancelled" });
  expect(watch.quiet()).toBe(true);
  watch.apply(event(5, "run.created", run("running", "other", 0, "run-9")));
  expect(watch.busy()).toBe(true);
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

it("still owes the wake after the roster clears, so a cancel in that gap is not quiet", () => {
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
  expect(watch.quiet()).toBe(false);
  expect(watch.providerSession()).toBe("session");
  // A detach removes the record; the process may still deliver, so the gap stays non-quiet.
  watch.apply(event(3, "provider-session.detached", { providerSessionId: "session" }));
  expect(watch.quiet()).toBe(false);
  expect(watch.providerSession()).toBeUndefined();
  // Only a session T3 reports stopped confirms that nothing more can come.
  watch.apply(event(4, "provider-session.attached", { id: "session", status: "stopped" }));
  expect(watch.quiet()).toBe(true);
});

it("retains no provider records before the turn's run exists and only its own afterwards", () => {
  const watch = watchTurn("message-current", "/workspace");
  const history = Array.from({ length: 500 }, (_, index) => index);
  watch.apply(
    snapshot({
      runs: history.map((index) => ({
        ...run("completed", `message-${String(index)}`, index),
        providerThreadId: `thread-${String(index)}`,
      })),
      providerThreads: history.map((index) => ({
        id: `thread-${String(index)}`,
        providerSessionId: `session-${String(index)}`,
      })),
      providerSessions: history.map((index) => ({
        id: `session-${String(index)}`,
        status: "stopped",
      })),
    }),
  );
  // The ordinary pre-dispatch snapshot: ownership unknown, nothing of the history kept.
  expect(watch.retained()).toMatchObject({ runs: 0, providerThreads: 0, sessions: 0 });
  // The turn's run starts: T3 re-sends its provider thread just before the run, then its session.
  watch.apply(
    event(2, "provider-thread.updated", {
      id: "thread-9",
      providerSessionId: "session-9",
      pendingBackgroundTasks: [{ taskId: "t", kind: "command" }],
    }),
  );
  watch.apply(
    event(3, "run.created", {
      ...run("running", "message-current", 500),
      providerThreadId: "thread-9",
    }),
  );
  watch.apply(event(4, "provider-session.updated", { id: "session-9", status: "running" }));
  watch.apply(
    event(5, "provider-thread.updated", { id: "thread-77", providerSessionId: "session-77" }),
  );
  watch.apply(event(6, "provider-session.updated", { id: "session-77", status: "running" }));
  expect(watch.retained()).toMatchObject({ runs: 1, providerThreads: 1, sessions: 1 });
  expect(watch.activity()).toMatchObject({ phase: "tools", running: [{ tool: "command" }] });
  expect(watch.providerSession()).toBe("session-9");
});

it("marks a resumed snapshot that hides how listed work ended as unresolved, not completed", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("running")],
      providerThreads: [
        {
          id: "provider-thread",
          providerSessionId: "session",
          pendingBackgroundTasks: [{ taskId: "t", kind: "command" }],
        },
      ],
    }),
  );
  watch.resubscribing();
  watch.apply(
    snapshot(
      {
        runs: [run("completed")],
        messages: [
          {
            id: "a",
            runId: "run-1",
            role: "assistant",
            text: "Started it.",
            createdAt: "2026-10-04T08:00:01Z",
          },
        ],
      },
      50,
    ),
  );
  expect(watch.outcome()).toEqual({
    kind: "unresolved",
    explanation:
      "Carillon lost its connection to T3 while this turn ran background work, and T3's state does not show whether the result was delivered",
  });
  expect(watch.quiet()).toBe(false);
  // A later run resolves it: the result was delivered.
  watch.apply(event(51, "run.created", run("completed", "wake", 2)));
  watch.apply(
    event(52, "message.updated", {
      id: "w",
      runId: "run-2",
      role: "assistant",
      text: "Done.",
      createdAt: "2026-10-04T08:00:05Z",
    }),
  );
  expect(watch.outcome()).toEqual({ kind: "completed", output: "Started it.\n\nDone." });

  // Work that visibly ended inside the running turn owes nothing.
  const inTurn = watchTurn("message-1", "/workspace");
  inTurn.apply(
    snapshot({
      runs: [run("running")],
      providerThreads: [
        {
          id: "provider-thread",
          providerSessionId: "session",
          pendingBackgroundTasks: [{ taskId: "t", kind: "command" }],
        },
      ],
    }),
  );
  inTurn.apply(
    event(2, "provider-thread.updated", {
      id: "provider-thread",
      providerSessionId: "session",
      pendingBackgroundTasks: [],
    }),
  );
  inTurn.apply(
    event(3, "message.updated", {
      id: "a",
      runId: "run-1",
      role: "assistant",
      text: "Done.",
      createdAt: "2026-10-04T08:00:01Z",
    }),
  );
  inTurn.apply(event(4, "run.updated", run("completed")));
  expect(inTurn.outcome()).toEqual({ kind: "completed", output: "Done." });
});

it("keeps a known request's ownership through a bounded snapshot that omits its item", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("waiting")],
      runtimeRequests: [{ id: "request-1", kind: "tool_approval", status: "pending" }],
      turnItems: [
        item(
          "approval_request",
          "run-1",
          { requestId: "request-1", requestKind: "command" },
          "waiting",
        ),
      ],
    }),
  );
  expect(watch.outcome()).toEqual({
    kind: "blocked",
    asked: "T3 needs an approval (command) this runtime cannot give yet",
  });
  watch.resubscribing();
  watch.apply(
    snapshot(
      {
        runs: [run("completed")],
        runtimeRequests: [{ id: "request-1", kind: "tool_approval", status: "pending" }],
        messages: [
          {
            id: "a",
            runId: "run-1",
            role: "assistant",
            text: "done",
            createdAt: "2026-10-04T08:00:01Z",
          },
        ],
      },
      40,
    ),
  );
  expect(watch.outcome()).toEqual({
    kind: "blocked",
    asked: "T3 needs an approval (command) this runtime cannot give yet",
  });
  expect(watch.requestOpen()).toBe(true);
  expect(watch.retained().requests).toBe(2);
});

it("keeps only the turn's provider thread and session from a snapshot", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("running")],
      providerThreads: [
        { id: "provider-thread", providerSessionId: "session" },
        { id: "other-thread", providerSessionId: "other-session" },
      ],
      providerSessions: [
        { id: "session", status: "running" },
        { id: "other-session", status: "stopped" },
      ],
    }),
  );
  expect(watch.retained()).toMatchObject({ providerThreads: 1, sessions: 1 });
  watch.apply(event(2, "provider-thread.updated", { id: "other-thread", providerSessionId: null }));
  expect(watch.retained()).toMatchObject({ providerThreads: 1, sessions: 1 });
});

it("bounds unrelated provider updates and detaches while preserving its own detach", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("running")] }));
  let sequence = 1;
  for (let index = 0; index < 500; index++) {
    const id = `unrelated-${String(index)}`;
    watch.apply(event(++sequence, "provider-thread.updated", { id, providerSessionId: id }));
    watch.apply(event(++sequence, "provider-session.updated", { id, status: "ready" }));
    watch.apply(event(++sequence, "provider-session.detached", { providerSessionId: id }));
  }
  expect(watch.retained()).toMatchObject({
    providerThreads: 1,
    sessions: 1,
    pendingProviderRecords: 8,
    detachedSessions: 0,
    delegations: 0,
    transfers: 0,
  });
  watch.apply(event(++sequence, "provider-session.detached", { providerSessionId: "session" }));
  expect(watch.retained().detachedSessions).toBe(1);
  expect(watch.providerSession()).toBeUndefined();
  watch.apply(
    event(++sequence, "provider-thread.updated", {
      id: "provider-thread",
      providerSessionId: "replacement",
    }),
  );
  watch.apply(
    event(sequence + 1, "provider-session.updated", { id: "replacement", status: "ready" }),
  );
  expect(watch.retained().detachedSessions).toBe(0);
  expect(watch.providerSession()).toBe("replacement");
});

it("owns its provider thread and session through the serving event order, after a long pre-dispatch history", () => {
  const watch = watchTurn("message-current", "/workspace");
  const history = Array.from({ length: 500 }, (_, index) => index);
  watch.apply(
    snapshot({
      runs: history.map((index) => ({
        ...run("completed", `message-${String(index)}`, index),
        providerThreadId: `thread-${String(index)}`,
      })),
      providerThreads: history.map((index) => ({
        id: `thread-${String(index)}`,
        providerSessionId: `session-${String(index)}`,
      })),
      providerSessions: history.map((index) => ({
        id: `session-${String(index)}`,
        status: "stopped",
      })),
    }),
  );
  expect(watch.retained()).toMatchObject({ runs: 0, providerThreads: 0, sessions: 0 });
  // Dispatch: the provider thread record arrives before the run that refers to it.
  watch.apply(
    event(2, "provider-thread.updated", { id: "thread-9", providerSessionId: "session-9" }),
  );
  watch.apply(
    event(3, "run.created", {
      ...run("starting", "message-current", 500),
      providerThreadId: "thread-9",
    }),
  );
  // Turn start: the session record arrives before the thread record that names it, then the run.
  watch.apply(event(4, "provider-session.updated", { id: "session-9", status: "running" }));
  watch.apply(
    event(5, "provider-thread.updated", {
      id: "thread-9",
      providerSessionId: "session-9",
      pendingBackgroundTasks: [{ taskId: "t", kind: "command" }],
    }),
  );
  watch.apply(
    event(6, "run.updated", {
      ...run("running", "message-current", 500),
      providerThreadId: "thread-9",
    }),
  );
  expect(watch.retained()).toMatchObject({ runs: 1, providerThreads: 1, sessions: 1 });
  expect(watch.providerSession()).toBe("session-9");
  // The root completes with the roster still listed: the live session owes the report; nothing is detached.
  watch.apply(
    event(7, "run.updated", {
      ...run("completed", "message-current", 500),
      providerThreadId: "thread-9",
    }),
  );
  expect(watch.outcome()).toBeUndefined();
  expect(watch.quiet()).toBe(false);
  // Its wake completes the turn.
  watch.apply(
    event(8, "provider-thread.updated", {
      id: "thread-9",
      providerSessionId: "session-9",
      pendingBackgroundTasks: [],
    }),
  );
  watch.apply(
    event(9, "provider-thread.updated", {
      id: "thread-9",
      providerSessionId: "session-9",
      pendingBackgroundTasks: [],
    }),
  );
  watch.apply(
    event(10, "run.created", { ...run("running", "wake", 501), providerThreadId: "thread-9" }),
  );
  watch.apply(
    event(11, "message.updated", {
      id: "w",
      runId: "run-501",
      role: "assistant",
      text: "Done.",
      createdAt: "2026-10-04T10:00:00Z",
    }),
  );
  watch.apply(
    event(12, "run.updated", { ...run("completed", "wake", 501), providerThreadId: "thread-9" }),
  );
  expect(watch.outcome()).toEqual({ kind: "completed", output: "Done." });
  expect(watch.retained()).toMatchObject({ providerThreads: 1, sessions: 1 });
});

it("treats a session record it never saw as unknown, not as detached", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(snapshot({ runs: [], providerThreads: [], providerSessions: [] }));
  // T3 sends the run's provider thread record before the run that names it.
  watch.apply(
    event(2, "provider-thread.updated", {
      id: "provider-thread",
      providerSessionId: "never-seen",
      pendingBackgroundTasks: [{ taskId: "t", kind: "command" }],
    }),
  );
  watch.apply(event(3, "run.created", run("running")));
  watch.apply(event(4, "run.updated", run("completed")));
  expect(watch.outcome()).toBeUndefined();
  expect(watch.quiet()).toBe(false);
  expect(watch.providerSession()).toBe("never-seen");
  // A snapshot that lists the thread's session but omits that one is a detachment.
  watch.resubscribing();
  watch.apply(
    snapshot(
      {
        runs: [run("completed")],
        providerThreads: [
          {
            id: "provider-thread",
            providerSessionId: "never-seen",
            pendingBackgroundTasks: [{ taskId: "t", kind: "command" }],
          },
        ],
        providerSessions: [],
      },
      9,
    ),
  );
  expect(watch.outcome()).toEqual({
    kind: "failed",
    explanation:
      "T3's native session was detached while it still listed background work; T3 does not confirm whether that work still reports back.",
  });
  expect(watch.providerSession()).toBeUndefined();
});

it("adopts a later run only while the turn expects a follow-up", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("running")] }));
  // A predecessor's late wake queues behind the turn's run: not this turn's.
  watch.apply(event(2, "run.created", run("queued", "stale-wake", 2)));
  watch.apply(
    event(3, "message.updated", {
      id: "a",
      runId: "run-1",
      role: "assistant",
      text: "Mine.",
      createdAt: "2026-10-04T12:00:01Z",
    }),
  );
  watch.apply(event(4, "run.updated", run("completed")));
  expect(watch.outcome()).toEqual({ kind: "completed", output: "Mine." });
  expect(watch.retained().runs).toBe(1);
  // The same later run is the turn's when the turn owes a report.
  const owing = watchTurn("message-1", "/workspace");
  owing.apply(
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
  owing.apply(event(2, "run.created", run("running", "wake", 2)));
  expect(owing.activeRun()?.id).toBe("run-2");
});

it("leaves a pending request no item of the turn names unassigned, even while the run waits", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("completed", "older", 0, "run-0"), run("waiting")],
      runtimeRequests: [
        { id: "stale", kind: "tool_approval", status: "pending" },
        { id: "mine", kind: "user_input", status: "pending" },
      ],
    }),
  );
  expect(watch.outcome()).toBeUndefined();
  expect(watch.requestOpen()).toBe(false);
  watch.apply(
    event(
      2,
      "turn-item.updated",
      item(
        "user_input_request",
        "run-1",
        { requestId: "mine", questions: [{ header: "Scope", question: "Which file?" }] },
        "waiting",
      ),
    ),
  );
  expect(watch.outcome()).toEqual({
    kind: "blocked",
    asked: "T3 asked a question this runtime cannot relay yet (Scope: Which file?)",
  });
});

it("never adopts a run first seen while its own run was active, even once it owes a wake", () => {
  // The coordinator's fault probe: the successor runs with work listed when an earlier turn's wake queues behind it.
  const watch = watchTurn("current-message", "/workspace");
  const current = {
    ...run("running", "current-message", 2, "current"),
    providerThreadId: "provider",
  };
  const stale = {
    ...current,
    id: "old-wake",
    ordinal: 3,
    userMessageId: "predecessor-notification",
    status: "queued",
  };
  const provider = {
    id: "provider",
    providerSessionId: "new-session",
    pendingBackgroundTasks: [{ taskId: "current-background", kind: "command" }],
  };
  watch.apply(
    snapshot({
      runs: [current],
      providerThreads: [provider],
      providerSessions: [{ id: "new-session", status: "running" }],
    }),
  );
  watch.apply(event(2, "run.created", stale));
  expect(watch.latestRun()?.id).toBe("current");
  watch.apply(
    event(3, "message.updated", {
      id: "current-ack",
      runId: "current",
      role: "assistant",
      text: "Current acknowledgement.",
      createdAt: "2026-10-04T15:00:01Z",
    }),
  );
  watch.apply(event(4, "run.updated", { ...current, status: "completed" }));
  watch.apply(event(5, "run.updated", { ...stale, status: "running" }));
  watch.apply(
    event(6, "message.updated", {
      id: "old-output",
      runId: "old-wake",
      role: "assistant",
      text: "PREDECESSOR OUTPUT.",
      createdAt: "2026-10-04T15:00:02Z",
    }),
  );
  watch.apply(event(7, "provider-thread.updated", { ...provider, pendingBackgroundTasks: [] }));
  watch.apply(event(8, "run.updated", { ...stale, status: "completed" }));
  // The foreign run never satisfies the owed wake; the turn keeps waiting for its own.
  expect(watch.outcome()).toBeUndefined();
  expect(watch.retained().runs).toBe(1);
  const own = {
    ...current,
    id: "own-wake",
    ordinal: 4,
    userMessageId: "own-notification",
    status: "running",
  };
  watch.apply(event(9, "run.created", own));
  watch.apply(
    event(10, "message.updated", {
      id: "own-output",
      runId: "own-wake",
      role: "assistant",
      text: "OWN RESULT.",
      createdAt: "2026-10-04T15:00:03Z",
    }),
  );
  watch.apply(event(11, "run.updated", { ...own, status: "completed" }));
  expect(watch.outcome()).toEqual({
    kind: "completed",
    output: "Current acknowledgement.\n\nOWN RESULT.",
  });
});

it("ends unresolved on a fresh snapshot whose follow-up no fact of the turn accounts for, and completes with the follow-up when the facts are known", () => {
  // The coordinator's snapshot probe: completed root, completed follow-up, empty roster, no background fact seen.
  const root = {
    ...run("completed", "current-message", 2, "current"),
    providerThreadId: "provider",
  };
  const wake = { ...root, id: "owed-wake", ordinal: 3, userMessageId: "native-notification" };
  const projection = {
    runs: [root, wake],
    providerThreads: [{ id: "provider", providerSessionId: "session", pendingBackgroundTasks: [] }],
    providerSessions: [{ id: "session", status: "ready" }],
    messages: [
      {
        id: "ack",
        runId: "current",
        role: "assistant",
        text: "Working in background.",
        createdAt: "2026-10-04T15:00:01Z",
      },
      {
        id: "result",
        runId: "owed-wake",
        role: "assistant",
        text: "OWN BACKGROUND RESULT.",
        createdAt: "2026-10-04T15:00:02Z",
      },
    ],
  };
  const fresh = watchTurn("current-message", "/workspace");
  fresh.apply(snapshot(projection, 42));
  expect(fresh.outcome()).toEqual({
    kind: "unresolved",
    explanation:
      "A run followed this turn's while Carillon was not watching, and T3's state cannot show whether it reports this turn's background work",
  });
  expect(fresh.quiet()).toBe(true);
  // A fold that had seen the run's listed work adopts the follow-up and answers with its result.
  const informed = watchTurn("current-message", "/workspace");
  informed.apply(
    snapshot({
      runs: [{ ...root, status: "running" }],
      providerThreads: [
        {
          id: "provider",
          providerSessionId: "session",
          pendingBackgroundTasks: [{ taskId: "t", kind: "command" }],
        },
      ],
      providerSessions: [{ id: "session", status: "running" }],
    }),
  );
  informed.resubscribing();
  informed.apply(snapshot(projection, 42));
  expect(informed.outcome()).toEqual({
    kind: "completed",
    output: "Working in background.\n\nOWN BACKGROUND RESULT.",
  });
});

it("ends unresolved on a follow-up after an unconfirmed earlier stop, and completes without it", () => {
  const owedWake = (options: { uncertainPredecessor?: boolean }) => {
    const watch = watchTurn("current-message", "/workspace", options);
    const root = {
      ...run("running", "current-message", 3, "current"),
      providerThreadId: "provider",
    };
    watch.apply(
      snapshot({
        runs: [run("interrupted", "earlier", 1, "earlier"), root],
        providerThreads: [
          {
            id: "provider",
            providerSessionId: "session",
            pendingBackgroundTasks: [{ taskId: "t", kind: "command" }],
          },
        ],
        providerSessions: [{ id: "session", status: "running" }],
      }),
    );
    watch.apply(event(2, "run.updated", { ...root, status: "completed" }));
    watch.apply(
      event(3, "provider-thread.updated", {
        id: "provider",
        providerSessionId: "session",
        pendingBackgroundTasks: [],
      }),
    );
    const wake = {
      ...root,
      id: "wake",
      ordinal: 4,
      userMessageId: "notification",
      status: "running",
    };
    watch.apply(event(4, "run.created", wake));
    watch.apply(
      event(5, "message.updated", {
        id: "w",
        runId: "wake",
        role: "assistant",
        text: "Result.",
        createdAt: "2026-10-04T15:00:03Z",
      }),
    );
    watch.apply(event(6, "run.updated", { ...wake, status: "completed" }));
    return watch;
  };
  expect(owedWake({}).outcome()?.kind).toBe("completed");
  expect(owedWake({ uncertainPredecessor: true }).outcome()).toEqual({
    kind: "unresolved",
    explanation:
      "An earlier turn on this conversation was not confirmed stopped, and T3 cannot show whether the run that followed this turn's background work reports this turn's work or the earlier one's",
  });
});

it("refuses a provider-session detach without a session identity instead of dropping it", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("completed")] }));
  let sequence = 1;
  for (const payload of [{}, { providerSessionId: 7 }, { providerSessionId: "" }, null, "session"])
    expect(() => watch.apply(event(++sequence, "provider-session.detached", payload))).toThrow(
      "T3 sent a provider session detach this runtime does not understand",
    );
  // A well-formed detach of the turn's session is still applied.
  watch.apply(event(++sequence, "provider-session.detached", { providerSessionId: "session" }));
  expect(watch.providerSession()).toBeUndefined();
  expect(watch.retained().detachedSessions).toBe(1);
});

it("is busy only while runs it did not start are active, never judging finished ones", () => {
  const watch = watchTurn("message-9", "/workspace");
  // A finished earlier run whose provider thread still lists work is not in the
  // way: the message runs now, and that work stays whoever's it is.
  watch.apply(
    snapshot({
      runs: [run("completed", "old-1", 1), run("completed", "old-2", 2)],
      providerThreads: [
        {
          id: "provider-thread",
          providerSessionId: "session",
          pendingBackgroundTasks: [{ taskId: "old", kind: "command" }],
        },
      ],
    }),
  );
  expect(watch.busy()).toBe(false);
  // Someone's active run (a person's in the native UI, or a late wake) makes it busy.
  watch.apply(event(2, "run.created", run("running", "old-3", 3)));
  expect(watch.busy()).toBe(true);
  watch.apply(event(3, "run.updated", run("waiting", "old-3", 3)));
  expect(watch.busy()).toBe(true);
  watch.apply(event(4, "run.updated", run("interrupted", "old-3", 3)));
  expect(watch.busy()).toBe(false);
  // Once the turn's own run exists, later runs are judged as follow-ups, not as busy work.
  watch.apply(event(5, "run.created", run("queued", "message-9", 4)));
  expect(watch.run()?.id).toBe("run-4");
  expect(watch.busy()).toBe(false);
  expect(watch.activity().phase).toBe("queued");
});

it("refuses a run that names a provider thread T3 never sent, instead of assuming no background work", () => {
  // The turn's own run, completed, with its provider thread record missing from a snapshot.
  const current = watchTurn("message-1", "/workspace");
  expect(() =>
    current.apply(
      snapshot({ runs: [run("completed")], providerThreads: [], providerSessions: [] }),
    ),
  ).toThrow("T3 named provider thread provider-thread for run run-1 without sending its record");
  // A run created without its record having arrived first is the same broken reference.
  const created = watchTurn("message-1", "/workspace");
  created.apply(snapshot({ runs: [], providerThreads: [], providerSessions: [] }));
  expect(() => created.apply(event(2, "run.created", run("running")))).toThrow(
    "T3 named provider thread provider-thread for run run-1 without sending its record",
  );
  // A run T3 has not attached to a provider thread yet has nothing listed.
  const unattached = watchTurn("message-1", "/workspace");
  unattached.apply(
    snapshot({
      runs: [{ ...run("completed"), providerThreadId: null }],
      providerThreads: [],
      providerSessions: [],
    }),
  );
  expect(unattached.quiet()).toBe(true);
});

it("serves background task kinds through the shared tool-name normalization", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("running")],
      providerThreads: [
        {
          id: "provider-thread",
          providerSessionId: "session",
          pendingBackgroundTasks: [
            { taskId: "empty", kind: "" },
            { taskId: "control", kind: "sub\u0000agent\n\tjob" },
            { taskId: "long", kind: "k".repeat(100) },
            { taskId: "plain", kind: "command", description: "sleep 300" },
            { taskId: "unknown" },
          ],
        },
      ],
    }),
  );
  const activity = watch.activity();
  expect(Check(TurnActivitySchema, activity)).toBe(true);
  expect(activity.running.map((tool) => tool.tool)).toEqual([
    "tool",
    "sub agent job",
    `${"k".repeat(63)}…`,
    "command",
    "background_task",
  ]);
  expect(activity.running[3]).toMatchObject({ detail: "sleep 300" });
});

it("keeps the answer text it observed when a bounded snapshot omits an earlier message", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("running")] }));
  watch.apply(
    event(2, "message.updated", {
      id: "a",
      runId: "run-1",
      role: "assistant",
      text: "Part one.",
      createdAt: "2026-10-05T08:00:00Z",
    }),
  );
  // The connection resumes with a bounded snapshot whose window starts after that message.
  watch.resubscribing();
  watch.apply(
    snapshot(
      {
        runs: [run("completed")],
        messages: [
          {
            id: "b",
            runId: "run-1",
            role: "assistant",
            text: "Part two.",
            createdAt: "2026-10-05T08:00:05Z",
          },
        ],
      },
      9,
    ),
  );
  expect(watch.outcome()).toEqual({ kind: "completed", output: "Part one.\n\nPart two." });
  // A message the snapshot does list replaces the observed one by ID.
  watch.apply(
    event(10, "message.updated", {
      id: "a",
      runId: "run-1",
      role: "assistant",
      text: "Part one, revised.",
      createdAt: "2026-10-05T08:00:00Z",
    }),
  );
  expect(watch.outcome()).toEqual({ kind: "completed", output: "Part one, revised.\n\nPart two." });
  expect(watch.retained().messages).toBe(2);
});

const task = (
  id: string,
  status: string,
  runId = "run-1",
  fields: Record<string, unknown> = {},
) => ({
  id,
  runId,
  parentNodeId: `node:run:${runId}:root`,
  origin: "app_owned",
  driver: "claudeAgent",
  status,
  title: "pong child",
  model: "claude-sonnet-5",
  childThreadId: `thread:${id}`,
  startedAt: "2026-10-05T11:14:52Z",
  completionWake: "always",
  ...fields,
});
const reply = (id: string, runId: string, text: string, at: string) =>
  event(0, "message.updated", { id, runId, role: "assistant", text, createdAt: at }).event.payload;

it("answers when its run completes and hands delegated work still owed to a follow-up, in the native order", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("running")] }));
  watch.apply(event(2, "subagent.updated", task("task-1", "running")));
  watch.apply(
    event(3, "message.updated", reply("a", "run-1", "DELEGATED", "2026-10-05T11:14:53Z")),
  );
  expect(watch.activity().work).toEqual({
    items: [
      {
        id: "task-1",
        kind: "delegation",
        label: "pong child",
        state: "running",
        actor: "claude-sonnet-5",
        started_at: 1_791_198_892,
      },
      {
        id: "report:message-1",
        kind: "task",
        label: "report back to this conversation",
        state: "queued",
      },
    ],
    partial: false,
  });
  watch.apply(event(4, "run.updated", run("completed")));
  // The parent's answer is complete; the child still owes the conversation a report.
  expect(watch.outcome()).toEqual({
    kind: "completed",
    output: "DELEGATED",
    followUp: { message: "message-1", after: 1 },
  });
  expect(watch.quiet()).toBe(true);
  // T3 publishes the result, announces the continuation, then starts it.
  watch.apply(
    event(
      5,
      "subagent.updated",
      task("task-1", "completed", "run-1", { completionDelivery: { state: "claimed" } }),
    ),
  );
  watch.apply(
    event(6, "run.updated", {
      ...run("completed"),
      delegatedCompletion: { disposition: "open", delivery: { messageId: "continuation-1" } },
    }),
  );
  watch.apply(event(7, "run.created", run("running", "continuation-1", 2)));
  watch.apply(
    event(8, "message.updated", {
      id: "continuation-1",
      runId: "run-2",
      role: "user",
      text: "Delegated task task-1 reached a terminal state.",
      createdAt: "2026-10-05T11:14:54Z",
      delegatedCompletion: { parentRunId: "run-1" },
    }),
  );
  watch.apply(
    event(9, "message.updated", reply("b", "run-2", "PONG reported.", "2026-10-05T11:14:59Z")),
  );
  // The continuation is neither this turn's answer nor a stray that makes it unresolved.
  expect(watch.outcome()).toEqual({
    kind: "completed",
    output: "DELEGATED",
    followUp: { message: "message-1", after: 1 },
  });
  expect(watch.activity().work?.items).toEqual([
    {
      id: "report:message-1",
      kind: "task",
      label: "report back to this conversation",
      state: "running",
    },
    expect.objectContaining({ id: "task-1", state: "succeeded" }),
  ]);
});

it("reports the continuations of a turn as a follow-up, waiting while the work still owes one", () => {
  const delegated = {
    runs: [run("completed"), run("completed", "someone-else", 2)],
    subagents: [task("task-1", "running")],
  };
  // The child still works: nothing to answer yet, and nothing of the thread is the follow-up's own.
  const watch = watchTurn("message-1", "/workspace", { followUp: { after: 1 } });
  watch.apply(snapshot(delegated));
  expect(watch.outcome()).toBeUndefined();
  expect(watch.activity()).toMatchObject({
    phase: "waiting",
    work: { items: [{ id: "task-1", state: "running" }] },
  });
  // Another run on the thread is someone else's, never a stray of the follow-up.
  watch.apply(event(2, "run.updated", run("completed", "someone-else", 2)));
  expect(watch.outcome()).toBeUndefined();
  watch.apply(
    event(
      3,
      "subagent.updated",
      task("task-1", "completed", "run-1", { completionDelivery: { state: "claimed" } }),
    ),
  );
  expect(watch.outcome()).toBeUndefined();
  watch.apply(
    event(4, "run.updated", {
      ...run("completed"),
      delegatedCompletion: { disposition: "open", delivery: { messageId: "continuation-1" } },
    }),
  );
  watch.apply(event(5, "run.created", run("running", "continuation-1", 3)));
  // The continuation delegates again before it answers.
  watch.apply(
    event(6, "subagent.updated", task("task-2", "running", "run-3", { title: "grandchild" })),
  );
  watch.apply(
    event(7, "message.updated", reply("c", "run-3", "First result in.", "2026-10-05T11:15:00Z")),
  );
  expect(watch.outcome()).toBeUndefined();
  watch.apply(event(8, "run.updated", run("completed", "continuation-1", 3)));
  // Its text is the follow-up's answer; the new child owes a further follow-up, after this run.
  expect(watch.outcome()).toEqual({
    kind: "completed",
    output: "First result in.",
    followUp: { message: "message-1", after: 3 },
  });
});

it("follows a resumed snapshot's continuation by its message link, and fails when the work ends without a report", () => {
  const watch = watchTurn("message-1", "/workspace", { followUp: { after: 1 } });
  watch.apply(
    snapshot({
      runs: [run("completed"), run("completed", "continuation-1", 2)],
      subagents: [
        task("task-1", "completed", "run-1", { completionDelivery: { state: "delivered" } }),
      ],
      messages: [
        {
          id: "continuation-1",
          runId: "run-2",
          role: "user",
          text: "report",
          createdAt: "2026-10-05T11:14:54Z",
          delegatedCompletion: { parentRunId: "run-1" },
        },
        {
          id: "b",
          runId: "run-2",
          role: "assistant",
          text: "PONG.",
          createdAt: "2026-10-05T11:14:59Z",
        },
      ],
    }),
  );
  expect(watch.outcome()).toEqual({ kind: "completed", output: "PONG." });

  // A child stopped before reporting, with nothing left to deliver.
  const ended = watchTurn("message-1", "/workspace", { followUp: { after: 1 } });
  ended.apply(
    snapshot({
      runs: [run("completed")],
      subagents: [
        task("task-1", "cancelled", "run-1", { completionDelivery: { state: "disposed" } }),
      ],
    }),
  );
  expect(ended.outcome()).toEqual({
    kind: "failed",
    explanation: "T3's delegated work ended without reporting back to this conversation",
  });
  // A follow-up whose message T3 no longer lists cannot be followed.
  const lost = watchTurn("message-1", "/workspace", { followUp: { after: 1 } });
  lost.apply(snapshot({ runs: [] }));
  expect(lost.outcome()).toMatchObject({ kind: "failed" });
});

it("links a continuation queued behind its active parent, or announced only after it was seen", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("running")] }));
  watch.apply(event(2, "subagent.updated", task("task-1", "running")));
  // A fast child: its continuation is created queued while the parent still runs, and
  // its link arrives after the run.
  watch.apply(event(3, "run.created", run("queued", "continuation-1", 2)));
  watch.apply(
    event(4, "message.updated", {
      id: "continuation-1",
      runId: "run-2",
      role: "user",
      text: "report",
      createdAt: "2026-10-05T11:14:54Z",
      delegatedCompletion: { parentRunId: "run-1" },
    }),
  );
  watch.apply(
    event(5, "message.updated", reply("a", "run-1", "Delegated.", "2026-10-05T11:14:53Z")),
  );
  watch.apply(event(6, "run.updated", run("completed")));
  watch.apply(event(7, "run.updated", run("running", "continuation-1", 2)));
  // Not foreign, not this turn's answer: the follow-up owes it.
  expect(watch.outcome()).toEqual({
    kind: "completed",
    output: "Delegated.",
    followUp: { message: "message-1", after: 1 },
  });
});

it("marks delegations a resumed snapshot no longer lists unknown, and shows nesting, transfers and each child once", () => {
  const watch = watchTurn("message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("running")],
      providerThreads: [
        {
          id: "provider-thread",
          providerSessionId: "session",
          pendingBackgroundTasks: [
            { taskId: "bg-child", kind: "subagent", childThreadId: "thread:native-1" },
            { taskId: "bg-shell", kind: "command", description: "sleep 300" },
          ],
        },
      ],
      subagents: [
        task("native-1", "running", "run-1", {
          origin: "provider_native",
          title: "Explore",
          model: null,
        }),
        task("native-2", "waiting", "run-1", {
          origin: "provider_native",
          title: "Nested",
          parentNodeId: "native-1",
        }),
        task("other", "running", "run-9"),
      ],
      turnItems: [
        item("subagent", "run-1", { id: "item-native-1", subagentId: "native-1" }, "running"),
      ],
      contextTransfers: [
        {
          id: "handoff-1",
          type: "provider_handoff",
          status: "resolved_portable",
          sourceThreadId: "thread",
          targetThreadId: "thread",
          sourcePoint: { threadId: "thread" },
          targetRunId: "run-1",
          targetProviderInstanceId: "codex",
        },
        {
          id: "result-1",
          type: "subagent_result",
          status: "consumed",
          sourceThreadId: "child",
          targetThreadId: "thread",
          sourcePoint: { threadId: "child" },
          targetRunId: "run-1",
          targetProviderInstanceId: null,
        },
      ],
    }),
  );
  const activity = watch.activity();
  // The child shows once, as its delegation: not as its item, nor as its parent's background task.
  expect(activity.running.map((tool) => tool.tool)).toEqual(["command"]);
  expect(activity.work?.items).toEqual([
    expect.objectContaining({ id: "native-1", state: "running", actor: "claudeAgent" }),
    expect.objectContaining({ id: "native-2", state: "waiting", parent: "native-1" }),
    { id: "handoff-1", kind: "handoff", label: "context handoff to codex", state: "waiting" },
  ]);
  expect(activity.work?.partial).toBe(false);
  // A resumed snapshot that omits a known child does not make it finished.
  watch.resubscribing();
  watch.apply(
    snapshot(
      {
        runs: [run("running")],
        subagents: [
          task("native-1", "running", "run-1", {
            origin: "provider_native",
            title: "Explore",
            model: null,
          }),
        ],
      },
      9,
    ),
  );
  const resumed = watch.activity().work;
  expect(resumed?.items.find((work) => work.id === "native-2")?.state).toBe("unknown");
  expect(resumed?.partial).toBe(true);
  expect(watch.retained()).toMatchObject({ delegations: 2, transfers: 1 });
});
