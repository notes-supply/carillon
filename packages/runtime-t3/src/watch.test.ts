import { TurnActivitySchema } from "@carillon/protocol";
import { Check } from "typebox/value";
import { expect, it, vi } from "vitest";
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

/** The item T3 opens a provider's wake run with: the notification of its background work. */
const wakeOpening = (runId: string) =>
  item("notification", runId, {
    source: { kind: "background_task" },
    outcome: "updated",
    summary: "Background activity updated",
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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
    links: 0,
  });
});

it("retains only the turn's own work from a long history and keeps its identity through a resumed snapshot", () => {
  const watch = watchTurn("thread", "message-500", "/workspace");
  const olderRuns = Array.from({ length: 500 }, (_, index) =>
    run(
      index === 7 ? "running" : "completed",
      `message-${String(index)}`,
      index + 1,
      `run-${String(index)}`,
    ),
  );
  watch.apply(
    snapshot({
      runs: [...olderRuns, run("running", "message-500", 501, "run-500")],
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
    links: 0,
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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
    links: 0,
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

it("does not take over background work listed before its run started, but waits for its own", () => {
  const watch = watchTurn("thread", "message-2", "/workspace");
  const thread = (taskIds: string[]) => ({
    id: "provider-thread",
    providerSessionId: "session",
    pendingBackgroundTasks: taskIds.map((taskId) => ({
      taskId,
      kind: "command",
      description: taskId,
    })),
  });
  // A run started in T3's own UI completed and left background work listed.
  watch.apply(
    snapshot({ runs: [run("completed", "ui", 1)], providerThreads: [thread(["ui-task"])] }),
  );
  watch.apply(event(2, "run.created", run("queued", "message-2", 2)));
  watch.apply(event(3, "run.updated", run("running", "message-2", 2)));
  expect(watch.activity().running).toEqual([]);
  watch.apply(
    event(4, "message.updated", {
      id: "a",
      runId: "run-2",
      role: "assistant",
      text: "CARILLON-AFTER",
      createdAt: "2026-10-05T12:34:00Z",
    }),
  );
  watch.apply(event(5, "run.updated", run("completed", "message-2", 2)));
  // The UI's work is still listed, and its report is the UI's, not this turn's.
  expect(watch.outcome()).toEqual({ kind: "completed", output: "CARILLON-AFTER" });
  expect(watch.quiet()).toBe(true);

  // Work the turn's own run starts on the same thread is the turn's.
  const own = watchTurn("thread", "message-3", "/workspace");
  own.apply(
    snapshot({ runs: [run("completed", "ui", 1)], providerThreads: [thread(["ui-task"])] }),
  );
  own.apply(event(2, "run.created", run("starting", "message-3", 2)));
  own.apply(event(3, "run.updated", run("running", "message-3", 2)));
  own.apply(
    event(
      4,
      "turn-item.updated",
      item("command_execution", "run-2", { input: "sleep 30 &" }, "running"),
    ),
  );
  own.apply(event(5, "provider-thread.updated", thread(["ui-task", "own-task"])));
  own.apply(event(6, "run.updated", run("completed", "message-3", 2)));
  expect(own.outcome()).toBeUndefined();
  expect(own.activity().running.map((tool) => tool.detail)).toEqual(["sleep 30 &", "own-task"]);
  // A run first seen already running cannot tell what was listed before it: all of it is its own.
  const resumed = watchTurn("thread", "message-4", "/workspace");
  resumed.apply(
    snapshot({ runs: [run("completed", "message-4", 1)], providerThreads: [thread(["unknown"])] }),
  );
  expect(resumed.outcome()).toBeUndefined();
});

it("shows a provider handoff T3 records before the run it hands over to", () => {
  const watch = watchTurn("thread", "message-2", "/workspace");
  watch.apply(snapshot({ runs: [run("completed", "earlier", 1)] }));
  const handoff = (status: string) => ({
    id: "handoff",
    type: "provider_handoff",
    status,
    sourceThreadId: "thread",
    targetThreadId: "thread",
    sourcePoint: { threadId: "thread" },
    targetRunId: "run-2",
    targetProviderInstanceId: "codex",
  });
  const held = watch.retained().pendingProviderRecords;
  watch.apply(event(2, "context-transfer.created", handoff("pending")));
  expect(watch.retained().pendingProviderRecords).toBe(held + 1);
  watch.apply(event(3, "run.created", run("running", "message-2", 2)));
  watch.apply(event(4, "context-transfer.updated", handoff("consumed")));
  expect(watch.activity().work?.items).toEqual([
    { id: "handoff", kind: "handoff", label: "context handoff to codex", state: "succeeded" },
  ]);
  expect(watch.retained().pendingProviderRecords).toBe(0);
});

it("is not finished while background work, an owed wake or a pending request remains", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
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
  watch.apply(event(4, "turn-item.updated", wakeOpening("run-2")));
  expect(watch.activeRun()?.id).toBe("run-2");
  watch.apply(
    event(5, "message.updated", {
      id: "w",
      runId: "run-2",
      role: "assistant",
      text: "result",
      createdAt: "2026-10-03T10:00:05Z",
    }),
  );
  watch.apply(event(6, "run.updated", run("completed", "wake", 2)));
  expect(watch.outcome()).toEqual({ kind: "completed", output: "done\n\nresult" });

  watch.apply(event(7, "run.updated", run("waiting", "wake", 2)));
  watch.apply(
    event(8, "runtime-request.updated", {
      id: "request-1",
      kind: "tool_approval",
      status: "pending",
    }),
  );
  // Pending alone is unknown; the wake run's own item names the request.
  expect(watch.outcome()).toBeUndefined();
  watch.apply(
    event(
      9,
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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

  const detached = watchTurn("thread", "message-1", "/workspace");
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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
  const other = watchTurn("thread", "message-1", "/workspace");
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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
  // Work the run started and still lists keeps the turn terminal but not quiet.
  const listed = (sequence: number, tasks: readonly { taskId: string; kind: string }[]) =>
    event(sequence, "provider-thread.updated", {
      id: "provider-thread",
      providerSessionId: "session",
      pendingBackgroundTasks: tasks,
    });
  const stopped = watchTurn("thread", "message-1", "/workspace");
  stopped.apply(snapshot({ runs: [run("running")] }));
  stopped.apply(listed(2, [{ taskId: "t", kind: "command" }]));
  stopped.apply(event(3, "run.updated", run("failed")));
  expect(stopped.outcome()).toMatchObject({ kind: "failed" });
  expect(stopped.quiet()).toBe(false);
  stopped.apply(event(4, "run.updated", run("cancelled")));
  expect(stopped.outcome()).toEqual({ kind: "stopped", status: "cancelled" });
  expect(stopped.quiet()).toBe(false);
  stopped.apply(listed(5, []));
  expect(stopped.outcome()).toEqual({ kind: "stopped", status: "cancelled" });
  expect(stopped.quiet()).toBe(true);
  // Work first listed after the run stopped is another run's, and does not hold it.
  stopped.apply(listed(6, [{ taskId: "late", kind: "command" }]));
  expect(stopped.quiet()).toBe(true);
  stopped.apply(event(7, "run.created", run("running", "other", 2, "run-9")));
  expect(stopped.busy()).toBe(true);
});

it("refuses stream sequences that are not integers before they move the replay cursor", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  expect(() => watch.apply(snapshot({ runs: [run("running")] }, 1.5))).toThrow(
    "T3 sent a thread snapshot this runtime does not understand",
  );
  expect(watch.sequence()).toBeUndefined();
  watch.apply(snapshot({ runs: [run("running")] }, 4));
  expect(() => watch.apply(event(4.5, "run.updated", run("completed")))).toThrow(
    "T3 sent a thread event this runtime does not understand",
  );
  expect(watch.sequence()).toBe(4);
  // The next integer event is still folded.
  watch.apply(event(5, "run.updated", run("completed")));
  expect(watch.sequence()).toBe(5);
  expect(watch.outcome()?.kind).toBe("completed");
});

it("refuses a snapshot whose history flag is not a boolean before folding any of it", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("running")] }, 4));
  expect(() =>
    watch.apply({ ...snapshot({ runs: [run("completed")] }, 6), hasMoreHistory: "yes" }),
  ).toThrow("T3 sent a thread snapshot this runtime does not understand");
  // The cursor and the run are the earlier snapshot's.
  expect(watch.sequence()).toBe(4);
  expect(watch.outcome()).toBeUndefined();
});

it("refuses known kinds it cannot read, ignores unknown kinds and malformed stream items it does not need", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
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

it("refuses a known item whose run it cannot identify, and still ignores runless and other runs' items", () => {
  const request = (runId: unknown): Record<string, unknown> => {
    const named = Object.entries(
      item(
        "user_input_request",
        "run-1",
        { requestId: "mine", questions: [{ header: "Scope", question: "Which file?" }] },
        "waiting",
      ),
    ).filter(([key]) => key !== "runId");
    return Object.fromEntries(runId === undefined ? named : [...named, ["runId", runId]]);
  };
  const waiting = () => {
    const watch = watchTurn("thread", "message-1", "/workspace");
    watch.apply(
      snapshot({
        runs: [run("waiting")],
        runtimeRequests: [{ id: "mine", kind: "user_input", status: "pending" }],
      }),
    );
    return watch;
  };
  // The run waits on this request; an item that names no readable run
  // must not be passed over as another run's.
  for (const runId of [undefined, 7, ""]) {
    const watch = waiting();
    expect(() => watch.apply(event(2, "turn-item.updated", request(runId)))).toThrow(
      "T3 sent a user_input_request item this runtime does not understand",
    );
  }
  const watch = waiting();
  expect(() => watch.apply(event(2, "turn-item.updated", request(null)))).not.toThrow();
  expect(() =>
    watch.apply(
      event(3, "turn-item.updated", { ...request("another-run"), questions: "unreadable" }),
    ),
  ).not.toThrow();
  expect(watch.outcome()).toBeUndefined();
});

it("refuses a run whose ordinal T3 numbers from one", () => {
  for (const ordinal of [0, -1]) {
    const shown = watchTurn("thread", "message-1", "/workspace");
    expect(() =>
      shown.apply(snapshot({ runs: [run("running", "message-1", ordinal, "run-1")] })),
    ).toThrow("T3 sent a thread snapshot this runtime does not understand");
    const live = watchTurn("thread", "message-1", "/workspace");
    live.apply(snapshot({ runs: [] }));
    expect(() =>
      live.apply(event(2, "run.created", run("running", "message-1", ordinal, "run-1"))),
    ).toThrow("T3 sent a run this runtime does not understand");
  }
});

/** A run of another thread, for the turn's message. */
const foreignRun = (status: string, id: string) => ({
  ...run(status, "message-1", 1, id),
  threadId: "another-thread",
});

it("refuses another thread's run for the turn's message in the first snapshot", () => {
  // A snapshot of the turn's thread listing another thread's run folds none
  // of it: that run's message is no answer of this turn.
  const initial = watchTurn("thread", "message-1", "/workspace");
  expect(() =>
    initial.apply(
      snapshot({
        runs: [foreignRun("completed", "run-9")],
        messages: [
          {
            id: "a",
            runId: "run-9",
            role: "assistant",
            text: "Not this turn's.",
            createdAt: "2026-10-06T08:00:00Z",
          },
        ],
      }),
    ),
  ).toThrow("T3 sent run run-9 of another thread");
  expect(initial.run()).toBeUndefined();
  expect(initial.outcome()).toBeUndefined();
  expect(initial.sequence()).toBeUndefined();
  expect(initial.synchronized()).toBe(false);
});

it("refuses another thread's run for the turn's message live", () => {
  // It neither replaces the turn's run nor becomes what a cancel stops, and
  // the cursor stays before it.
  const live = watchTurn("thread", "message-1", "/workspace");
  live.apply(snapshot({ runs: [run("running")] }));
  expect(() => live.apply(event(2, "run.created", foreignRun("running", "run-9")))).toThrow(
    "T3 sent run run-9 of another thread",
  );
  expect(live.run()?.id).toBe("run-1");
  expect(live.activeRun()?.id).toBe("run-1");
  expect(live.sequence()).toBe(1);
});

it("orders the turn's answer by when T3 made each message, refusing one with no readable time", () => {
  const said = (id: string, text: string, createdAt: string) => ({
    id,
    runId: "run-1",
    role: "assistant",
    text,
    createdAt,
  });
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("running")],
      messages: [said("a", "First.", "2026-10-03T10:00:00.000Z")],
    }),
  );
  // 10:30 UTC, though its text sorts before both others.
  watch.apply(event(2, "message.updated", said("b", "Third.", "2026-10-03T09:30:00.000-01:00")));
  watch.apply(event(3, "message.updated", said("c", "Second.", "2026-10-03T10:15:00.000Z")));
  expect(watch.activity().headline).toBe("Third.");
  watch.apply(event(4, "run.updated", run("completed")));
  expect(watch.outcome()).toEqual({
    kind: "completed",
    output: "First.\n\nSecond.\n\nThird.",
  });

  // A message of the turn without a time is refused before it counts.
  const refused = watchTurn("thread", "message-1", "/workspace");
  refused.apply(
    snapshot({
      runs: [run("running")],
      messages: [said("a", "First.", "2026-10-03T10:00:00.000Z")],
    }),
  );
  expect(() =>
    refused.apply(event(2, "message.updated", said("z", "Unplaced.", "not a time"))),
  ).toThrow("T3 sent an assistant message this runtime does not understand");
  expect(refused.activity().headline).toBe("First.");
  // So is one a snapshot lists, which then moves nothing on.
  const shown = watchTurn("thread", "message-1", "/workspace");
  shown.apply(
    snapshot({
      runs: [run("running")],
      messages: [said("a", "First.", "2026-10-03T10:00:00.000Z")],
    }),
  );
  expect(() =>
    shown.apply(
      snapshot(
        {
          runs: [run("running")],
          messages: [
            said("a", "First.", "2026-10-03T10:00:00.000Z"),
            said("z", "Unplaced.", "not a time"),
          ],
        },
        2,
      ),
    ),
  ).toThrow("T3 sent an assistant message this runtime does not understand");
  expect(shown.sequence()).toBe(1);
  // Messages of other runs are not this turn's to judge.
  expect(() =>
    refused.apply(
      event(3, "message.updated", { ...said("y", "Other.", "not a time"), runId: "run-9" }),
    ),
  ).not.toThrow();
});

it("moves the replay cursor past an event only once it is folded or passed over", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("running")] }));
  // A kind this runtime does not read is passed over.
  watch.apply(event(2, "future.event", { anything: true }));
  expect(watch.sequence()).toBe(2);
  // Another run's message is not this turn's to judge, even without a readable time.
  watch.apply(
    event(3, "message.updated", {
      id: "o",
      runId: "run-9",
      role: "assistant",
      text: "Other.",
      createdAt: "not a time",
    }),
  );
  expect(watch.sequence()).toBe(3);
  // The turn's own message without one is refused, and the cursor stays.
  expect(() =>
    watch.apply(
      event(4, "message.updated", {
        id: "z",
        runId: "run-1",
        role: "assistant",
        text: "Unplaced.",
        createdAt: "not a time",
      }),
    ),
  ).toThrow("T3 sent an assistant message this runtime does not understand");
  expect(watch.sequence()).toBe(3);
});

it("still owes the wake after the roster clears, so a cancel in that gap is not quiet", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
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
  const watch = watchTurn("thread", "message-current", "/workspace");
  const history = Array.from({ length: 500 }, (_, index) => index);
  watch.apply(
    snapshot({
      runs: history.map((index) => ({
        ...run("completed", `message-${String(index)}`, index + 1, `run-${String(index)}`),
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
      ...run("running", "message-current", 501, "run-500"),
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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
  watch.apply(event(52, "turn-item.updated", wakeOpening("run-2")));
  watch.apply(
    event(53, "message.updated", {
      id: "w",
      runId: "run-2",
      role: "assistant",
      text: "Done.",
      createdAt: "2026-10-04T08:00:05Z",
    }),
  );
  expect(watch.outcome()).toEqual({ kind: "completed", output: "Started it.\n\nDone." });

  // Work that visibly ended inside the running turn owes nothing.
  const inTurn = watchTurn("thread", "message-1", "/workspace");
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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
    links: 0,
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
  const watch = watchTurn("thread", "message-current", "/workspace");
  const history = Array.from({ length: 500 }, (_, index) => index);
  watch.apply(
    snapshot({
      runs: history.map((index) => ({
        ...run("completed", `message-${String(index)}`, index + 1, `run-${String(index)}`),
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
      ...run("starting", "message-current", 501, "run-500"),
      providerThreadId: "thread-9",
    }),
  );
  // Turn start: the session record arrives before the thread record that names it, then the run.
  watch.apply(event(4, "provider-session.updated", { id: "session-9", status: "running" }));
  watch.apply(
    event(5, "provider-thread.updated", { id: "thread-9", providerSessionId: "session-9" }),
  );
  watch.apply(
    event(6, "run.updated", {
      ...run("running", "message-current", 501, "run-500"),
      providerThreadId: "thread-9",
    }),
  );
  // The run starts background work.
  watch.apply(
    event(7, "provider-thread.updated", {
      id: "thread-9",
      providerSessionId: "session-9",
      pendingBackgroundTasks: [{ taskId: "t", kind: "command" }],
    }),
  );
  expect(watch.retained()).toMatchObject({ runs: 1, providerThreads: 1, sessions: 1 });
  expect(watch.providerSession()).toBe("session-9");
  // The root completes with the roster still listed: the live session owes the report; nothing is detached.
  watch.apply(
    event(8, "run.updated", {
      ...run("completed", "message-current", 501, "run-500"),
      providerThreadId: "thread-9",
    }),
  );
  expect(watch.outcome()).toBeUndefined();
  expect(watch.quiet()).toBe(false);
  // Its wake completes the turn.
  watch.apply(
    event(9, "provider-thread.updated", {
      id: "thread-9",
      providerSessionId: "session-9",
      pendingBackgroundTasks: [],
    }),
  );
  watch.apply(
    event(10, "provider-thread.updated", {
      id: "thread-9",
      providerSessionId: "session-9",
      pendingBackgroundTasks: [],
    }),
  );
  watch.apply(
    event(11, "run.created", {
      ...run("running", "wake", 502, "run-501"),
      providerThreadId: "thread-9",
    }),
  );
  watch.apply(event(12, "turn-item.updated", wakeOpening("run-501")));
  watch.apply(
    event(13, "message.updated", {
      id: "w",
      runId: "run-501",
      role: "assistant",
      text: "Done.",
      createdAt: "2026-10-04T10:00:00Z",
    }),
  );
  watch.apply(
    event(14, "run.updated", {
      ...run("completed", "wake", 502, "run-501"),
      providerThreadId: "thread-9",
    }),
  );
  expect(watch.outcome()).toEqual({ kind: "completed", output: "Done." });
  expect(watch.retained()).toMatchObject({ providerThreads: 1, sessions: 1 });
});

it("treats a session record it never saw as unknown, not as detached", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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
  const owing = watchTurn("thread", "message-1", "/workspace");
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
  owing.apply(event(3, "turn-item.updated", wakeOpening("run-2")));
  expect(owing.activeRun()?.id).toBe("run-2");
});

it("leaves a pending request no item of the turn names unassigned, even while the run waits", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("completed", "older", 1, "run-0"), run("waiting", "message-1", 2, "run-1")],
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
  const watch = watchTurn("thread", "current-message", "/workspace");
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
  watch.apply(event(10, "turn-item.updated", wakeOpening("own-wake")));
  watch.apply(
    event(11, "message.updated", {
      id: "own-output",
      runId: "own-wake",
      role: "assistant",
      text: "OWN RESULT.",
      createdAt: "2026-10-04T15:00:03Z",
    }),
  );
  watch.apply(event(12, "run.updated", { ...own, status: "completed" }));
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
    turnItems: [wakeOpening("owed-wake")],
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
  const fresh = watchTurn("thread", "current-message", "/workspace");
  fresh.apply(snapshot(projection, 42));
  expect(fresh.outcome()).toEqual({
    kind: "unresolved",
    explanation:
      "A run T3's records cannot attribute followed this turn's, and T3's state cannot show whether it reports this turn's background work",
  });
  expect(fresh.quiet()).toBe(true);
  // A fold that had seen the run's listed work adopts the follow-up and answers with its result.
  const informed = watchTurn("thread", "current-message", "/workspace");
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
    const watch = watchTurn("thread", "current-message", "/workspace", options);
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
    watch.apply(event(5, "turn-item.updated", wakeOpening("wake")));
    watch.apply(
      event(6, "message.updated", {
        id: "w",
        runId: "wake",
        role: "assistant",
        text: "Result.",
        createdAt: "2026-10-04T15:00:03Z",
      }),
    );
    watch.apply(event(7, "run.updated", { ...wake, status: "completed" }));
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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
  const watch = watchTurn("thread", "message-9", "/workspace");
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
  const current = watchTurn("thread", "message-1", "/workspace");
  expect(() =>
    current.apply(
      snapshot({ runs: [run("completed")], providerThreads: [], providerSessions: [] }),
    ),
  ).toThrow("T3 named provider thread provider-thread for run run-1 without sending its record");
  // A run created without its record having arrived first is the same broken reference.
  const created = watchTurn("thread", "message-1", "/workspace");
  created.apply(snapshot({ runs: [], providerThreads: [], providerSessions: [] }));
  expect(() => created.apply(event(2, "run.created", run("running")))).toThrow(
    "T3 named provider thread provider-thread for run run-1 without sending its record",
  );
  // A run T3 has not attached to a provider thread yet has nothing listed.
  const unattached = watchTurn("thread", "message-1", "/workspace");
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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

it("keeps a native Agent child and its provider wake without a pending-background roster", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("running")] }));
  watch.apply(
    event(2, "subagent.updated", task("native", "running", "run-1", { origin: "provider_native" })),
  );
  watch.apply(
    event(3, "message.updated", reply("a", "run-1", "PARENT ACK", "2026-10-05T11:14:53Z")),
  );
  watch.apply(event(4, "run.updated", run("completed")));
  // The real producer can expose the Agent node but no roster. Its work
  // must not be abandoned with the completed parent run.
  expect(watch.outcome()).toBeUndefined();
  expect(watch.quiet()).toBe(false);
  watch.apply(
    event(
      5,
      "subagent.updated",
      task("native", "completed", "run-1", { origin: "provider_native" }),
    ),
  );
  expect(watch.outcome()).toBeUndefined();
  // A person's later message, even on the same provider thread, is not the wake.
  watch.apply(event(6, "run.created", run("running", "other", 2)));
  watch.apply(event(7, "turn-item.updated", item("user_message", "run-2", { text: "other" })));
  watch.apply(
    event(8, "message.updated", reply("other", "run-2", "FOREIGN", "2026-10-05T11:14:54Z")),
  );
  watch.apply(event(9, "run.updated", run("completed", "other", 2)));
  expect(watch.outcome()).toBeUndefined();
  watch.apply(event(10, "run.created", run("running", "wake", 3)));
  watch.apply(event(11, "turn-item.updated", wakeOpening("run-3")));
  watch.apply(
    event(12, "message.updated", reply("b", "run-3", "CHILD RESULT", "2026-10-05T11:14:59Z")),
  );
  watch.apply(event(13, "run.updated", run("completed", "wake", 3)));
  expect(watch.outcome()).toEqual({ kind: "completed", output: "PARENT ACK\n\nCHILD RESULT" });
  expect(watch.activity().work?.items).toEqual([
    expect.objectContaining({ id: "native", state: "succeeded" }),
  ]);
  expect(watch.quiet()).toBe(true);
});

it("does not call a failed parent quiet while its native child is still running", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("failed")],
      subagents: [task("native", "running", "run-1", { origin: "provider_native" })],
    }),
  );
  expect(watch.outcome()?.kind).toBe("failed");
  expect(watch.quiet()).toBe(false);
  watch.apply(
    event(
      2,
      "subagent.updated",
      task("native", "cancelled", "run-1", { origin: "provider_native" }),
    ),
  );
  expect(watch.quiet()).toBe(true);
});

it("fails closed when a fresh snapshot cannot tell whether a settled native child still owes a wake", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("completed")],
      subagents: [task("native", "completed", "run-1", { origin: "provider_native" })],
      messages: [reply("parent", "run-1", "PARENT ACK", "2026-10-05T11:14:53Z")],
    }),
  );
  expect(watch.outcome()).toMatchObject({ kind: "unresolved" });
  expect(watch.quiet()).toBe(false);
  // If the provider publishes its wake before the unresolved turn exits,
  // its own opening resolves that ambiguity without taking another user's run.
  watch.apply(event(2, "run.created", run("running", "wake", 2)));
  watch.apply(event(3, "turn-item.updated", wakeOpening("run-2")));
  watch.apply(
    event(4, "message.updated", reply("result", "run-2", "CHILD RESULT", "2026-10-05T11:14:59Z")),
  );
  watch.apply(event(5, "run.updated", run("completed", "wake", 2)));
  expect(watch.outcome()).toEqual({ kind: "completed", output: "PARENT ACK\n\nCHILD RESULT" });
});

it("retains native result uncertainty when only its parent was known before reconnecting", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("running")] }));
  watch.apply(
    snapshot(
      {
        runs: [run("completed")],
        subagents: [task("native", "completed", "run-1", { origin: "provider_native" })],
        messages: [reply("parent", "run-1", "PARENT ACK", "2026-10-05T11:14:53Z")],
      },
      2,
    ),
  );
  expect(watch.outcome()).toMatchObject({ kind: "unresolved" });
  expect(watch.quiet()).toBe(false);
});

it.each([false, true])(
  "keeps recovered native results unresolved when one wake cannot account for two children (resumed: %s)",
  (resumed) => {
    const watch = watchTurn("thread", "message-1", "/workspace");
    if (resumed) watch.apply(snapshot({ runs: [run("running")] }));
    watch.apply(
      snapshot(
        {
          runs: [run("completed"), run("completed", "wake", 2)],
          subagents: [
            task("a", "completed", "run-1", { origin: "provider_native" }),
            task("b", "completed", "run-1", { origin: "provider_native" }),
          ],
          turnItems: [wakeOpening("run-2")],
          messages: [
            reply("parent", "run-1", "PARENT", "2026-10-05T11:14:53Z"),
            reply("first", "run-2", "FIRST RESULT", "2026-10-05T11:14:59Z"),
          ],
        },
        resumed ? 2 : 1,
      ),
    );
    expect(watch.outcome()).toMatchObject({ kind: "unresolved" });
    expect(watch.quiet()).toBe(false);
    expect(watch.activity().work?.partial).toBe(true);
    // A generic second wake still has no child identity; counting runs is
    // not proof that either child was reported exactly once.
    watch.apply(event(3, "run.created", run("running", "wake-again", 3)));
    watch.apply(event(4, "turn-item.updated", wakeOpening("run-3")));
    watch.apply(event(5, "run.updated", run("completed", "wake-again", 3)));
    expect(watch.outcome()).toMatchObject({ kind: "unresolved" });
  },
);

it("attributes a native provider wake already present beside its child in the first snapshot", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("completed"), run("completed", "wake", 2)],
      subagents: [task("native", "completed", "run-1", { origin: "provider_native" })],
      turnItems: [wakeOpening("run-2")],
      messages: [
        reply("parent", "run-1", "PARENT ACK", "2026-10-05T11:14:53Z"),
        reply("result", "run-2", "CHILD RESULT", "2026-10-05T11:14:59Z"),
      ],
    }),
  );
  expect(watch.outcome()).toEqual({ kind: "completed", output: "PARENT ACK\n\nCHILD RESULT" });
  expect(watch.quiet()).toBe(true);
});

it.each([false, true])(
  "refuses surplus generic wakes for one recovered native result (resumed: %s)",
  (resumed) => {
    const watch = watchTurn("thread", "message-1", "/workspace");
    if (resumed) watch.apply(snapshot({ runs: [run("running")] }));
    watch.apply(
      snapshot(
        {
          runs: [run("completed"), run("completed", "wake", 2), run("completed", "surplus", 3)],
          subagents: [task("native", "completed", "run-1", { origin: "provider_native" })],
          turnItems: [wakeOpening("run-2"), wakeOpening("run-3")],
          messages: [
            reply("parent", "run-1", "PARENT", "2026-10-05T11:14:53Z"),
            reply("own", "run-2", "OWN RESULT", "2026-10-05T11:14:59Z"),
            reply("other", "run-3", "FOREIGN RESULT", "2026-10-05T11:15:01Z"),
          ],
        },
        resumed ? 2 : 1,
      ),
    );
    expect(watch.outcome()).toMatchObject({ kind: "unresolved" });
    expect(watch.latestRun()?.id).toBe("run-2");
  },
);

it.each([false, true])(
  "retains only native result uncertainty still owed before a gap (inside: %s)",
  (inside) => {
    const watch = watchTurn("thread", "message-1", "/workspace");
    watch.apply(
      snapshot({
        runs: [run("running")],
        subagents: [
          task("a", inside ? "completed" : "running", "run-1", { origin: "provider_native" }),
          ...(inside ? [task("b", "running", "run-1", { origin: "provider_native" })] : []),
        ],
      }),
    );
    watch.apply(event(2, "run.updated", run("completed")));
    if (!inside)
      watch.apply(
        event(
          3,
          "subagent.updated",
          task("a", "completed", "run-1", { origin: "provider_native" }),
        ),
      );
    watch.apply(
      snapshot(
        {
          runs: [run("completed")],
          subagents: [
            task("a", "completed", "run-1", { origin: "provider_native" }),
            task("b", "completed", "run-1", { origin: "provider_native" }),
          ],
          messages: [reply("parent", "run-1", "PARENT", "2026-10-05T11:14:53Z")],
        },
        4,
      ),
    );
    expect(watch.outcome()).toBeUndefined();
    watch.apply(event(5, "run.created", run("running", "wake", 2)));
    watch.apply(event(6, "turn-item.updated", wakeOpening("run-2")));
    watch.apply(
      event(7, "message.updated", reply("result", "run-2", "B RESULT", "2026-10-05T11:14:59Z")),
    );
    watch.apply(event(8, "run.updated", run("completed", "wake", 2)));
    if (inside) {
      expect(watch.outcome()).toEqual({ kind: "completed", output: "PARENT\n\nB RESULT" });
      expect(watch.quiet()).toBe(true);
    } else {
      // A was still owed, too: one unlinked wake cannot discharge both A and B.
      expect(watch.outcome()).toMatchObject({ kind: "unresolved" });
      expect(watch.quiet()).toBe(false);
    }
  },
);

it("ends unresolved rather than waiting forever for a native child omitted by a resumed snapshot", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("running")],
      subagents: [task("native", "running", "run-1", { origin: "provider_native" })],
    }),
  );
  watch.apply(event(2, "run.updated", run("completed")));
  expect(watch.outcome()).toBeUndefined();
  watch.apply(snapshot({ runs: [run("completed")] }, 3));
  expect(watch.outcome()).toMatchObject({ kind: "unresolved" });
  expect(watch.quiet()).toBe(false);
  expect(watch.activity().work).toMatchObject({
    partial: true,
    items: [expect.objectContaining({ id: "native", state: "unknown" })],
  });
});

it("does not await native Agent work that finished inside the parent or belongs to another run", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("running")],
      subagents: [task("foreign", "running", "run-other", { origin: "provider_native" })],
    }),
  );
  watch.apply(
    event(2, "subagent.updated", task("native", "running", "run-1", { origin: "provider_native" })),
  );
  watch.apply(
    event(
      3,
      "subagent.updated",
      task("native", "completed", "run-1", { origin: "provider_native" }),
    ),
  );
  watch.apply(event(4, "message.updated", reply("a", "run-1", "RESULT", "2026-10-05T11:14:53Z")));
  watch.apply(event(5, "run.updated", run("completed")));
  expect(watch.outcome()).toEqual({ kind: "completed", output: "RESULT" });
  expect(watch.quiet()).toBe(true);
});

it("keeps other native children after the first child's wake completes", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("running")],
      subagents: [
        task("a", "running", "run-1", { origin: "provider_native" }),
        task("b", "running", "run-1", { origin: "provider_native" }),
      ],
    }),
  );
  watch.apply(
    event(2, "message.updated", reply("parent", "run-1", "PARENT", "2026-10-05T11:14:52Z")),
  );
  watch.apply(event(3, "run.updated", run("completed")));
  watch.apply(
    event(4, "subagent.updated", task("a", "completed", "run-1", { origin: "provider_native" })),
  );
  watch.apply(event(5, "run.created", run("running", "wake-a", 2)));
  watch.apply(event(6, "turn-item.updated", wakeOpening("run-2")));
  watch.apply(
    event(7, "message.updated", reply("first", "run-2", "FIRST", "2026-10-05T11:14:53Z")),
  );
  watch.apply(event(8, "run.updated", run("completed", "wake-a", 2)));
  expect(watch.outcome()).toBeUndefined();
  expect(watch.quiet()).toBe(false);
  watch.apply(
    event(9, "subagent.updated", task("b", "completed", "run-1", { origin: "provider_native" })),
  );
  watch.apply(event(10, "run.created", run("running", "wake-b", 3)));
  watch.apply(event(11, "turn-item.updated", wakeOpening("run-3")));
  watch.apply(
    event(12, "message.updated", reply("second", "run-3", "SECOND", "2026-10-05T11:14:54Z")),
  );
  watch.apply(event(13, "run.updated", run("completed", "wake-b", 3)));
  expect(watch.outcome()).toEqual({ kind: "completed", output: "PARENT\n\nFIRST\n\nSECOND" });
  expect(watch.quiet()).toBe(true);
});

it("replaces the report-back row with its confirmed completion in a follow-up report", () => {
  const watch = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  watch.apply(snapshot({ runs: [run("completed")], subagents: [task("child", "running")] }));
  watch.apply(
    event(2, "run.updated", {
      ...run("completed"),
      delegatedCompletion: { disposition: "open", delivery: { messageId: "continuation" } },
    }),
  );
  watch.apply(
    event(
      3,
      "subagent.updated",
      task("child", "completed", "run-1", { completionDelivery: { state: "delivered" } }),
    ),
  );
  watch.apply(event(4, "run.created", run("running", "continuation", 2)));
  expect(watch.activity().work?.items).toContainEqual({
    id: "report:message-1",
    kind: "task",
    label: "report back to this conversation",
    state: "running",
  });
  watch.apply(
    event(5, "message.updated", reply("b", "run-2", "CHILD RESULT", "2026-10-05T11:14:59Z")),
  );
  watch.apply(event(6, "run.updated", run("completed", "continuation", 2)));
  expect(watch.outcome()).toEqual({ kind: "completed", output: "CHILD RESULT" });
  expect(watch.activity().work?.items).toContainEqual({
    id: "report:message-1",
    kind: "task",
    label: "report back to this conversation",
    state: "succeeded",
  });
});

it.each([true, false])(
  "settles a report row even without a continuation run (elsewhere: %s)",
  (elsewhere) => {
    const watch = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
    watch.apply(
      snapshot({
        runs: [run("completed"), run("completed", "other", 2)],
        subagents: [
          task("child", elsewhere ? "completed" : "cancelled", "run-1", {
            completionDelivery: elsewhere
              ? { state: "acknowledged", observedByRunId: "run-2" }
              : { state: "disposed" },
          }),
        ],
      }),
    );
    expect(watch.outcome()?.kind).toBe(elsewhere ? "completed" : "failed");
    expect(watch.activity().work?.items).toContainEqual(
      expect.objectContaining({
        id: "report:message-1",
        state: elsewhere ? "succeeded" : "failed",
      }),
    );
  },
);

it("warns about native work started by a follow-up even without a provider roster", () => {
  const watch = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  watch.apply(
    snapshot({
      runs: [
        {
          ...run("completed"),
          delegatedCompletion: { disposition: "open", delivery: { messageId: "continuation" } },
        },
        run("completed", "continuation", 2),
      ],
      subagents: [
        task("child", "completed", "run-1", { completionDelivery: { state: "delivered" } }),
        task("native", "running", "run-2", { origin: "provider_native" }),
      ],
      messages: [reply("report", "run-2", "REPORT", "2026-10-05T11:14:59Z")],
    }),
  );
  expect(watch.outcome()).toEqual({
    kind: "completed",
    output:
      "REPORT\n\n[The report started background work in T3; its result will show in T3, not here.]",
  });
  expect(watch.activity().work?.items).toContainEqual(
    expect.objectContaining({ id: "native", state: "running" }),
  );
});

it("marks a report missing from a bounded snapshot as failed, not successfully delivered", () => {
  const watch = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  watch.apply({
    ...snapshot({
      runs: [
        {
          ...run("completed"),
          delegatedCompletion: { disposition: "open", delivery: { messageId: "continuation" } },
        },
        run("completed", "continuation", 2),
      ],
      subagents: [
        task("child", "completed", "run-1", { completionDelivery: { state: "delivered" } }),
      ],
    }),
    hasMoreHistory: true,
  });
  expect(watch.outcome()?.kind).toBe("failed");
  expect(watch.activity().work?.items).toContainEqual({
    id: "report:message-1",
    kind: "task",
    label: "report back to this conversation",
    state: "failed",
  });
});

it.each(["", " \n "])(
  "fails an empty completed report (%j), including its report-back row",
  (text) => {
    const watch = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
    watch.apply(
      snapshot({
        runs: [
          {
            ...run("completed"),
            delegatedCompletion: { disposition: "open", delivery: { messageId: "continuation" } },
          },
          run("completed", "continuation", 2),
        ],
        subagents: [
          task("child", "completed", "run-1", { completionDelivery: { state: "delivered" } }),
        ],
        messages: text.length === 0 ? [] : [reply("empty", "run-2", text, "2026-10-05T11:14:59Z")],
      }),
    );
    expect(watch.outcome()).toEqual({
      kind: "failed",
      explanation: "T3 finished the run without an answer.",
    });
    expect(watch.activity().work?.items).toContainEqual({
      id: "report:message-1",
      kind: "task",
      label: "report back to this conversation",
      state: "failed",
    });
  },
);

it("reports a failed continuation as failed rather than retaining a queued report-back row", () => {
  const watch = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  watch.apply(
    snapshot({
      runs: [run("completed")],
      subagents: [
        task("child", "completed", "run-1", { completionDelivery: { state: "claimed" } }),
      ],
    }),
  );
  watch.apply(
    event(2, "run.updated", {
      ...run("completed"),
      delegatedCompletion: { disposition: "open", delivery: { messageId: "continuation" } },
    }),
  );
  watch.apply(event(3, "run.created", run("running", "continuation", 2)));
  watch.apply(event(4, "run.updated", run("failed", "continuation", 2)));
  expect(watch.outcome()?.kind).toBe("failed");
  expect(watch.activity().work?.items).toContainEqual({
    id: "report:message-1",
    kind: "task",
    label: "report back to this conversation",
    state: "failed",
  });
});

it("answers when its run completes and hands delegated work still owed to a follow-up, in the native order", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
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
  const watch = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  watch.apply(snapshot(delegated));
  expect(watch.outcome()).toBeUndefined();
  expect(watch.activity()).toMatchObject({
    phase: "waiting",
    work: {
      items: [
        { id: "task-1", state: "running" },
        { id: "report:message-1", state: "queued" },
      ],
    },
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
  expect(watch.activity().work?.items).toContainEqual(
    expect.objectContaining({ id: "report:message-1", state: "queued" }),
  );
});

it("follows a resumed snapshot's continuation by its message link, and fails when the work ends without a report", () => {
  const watch = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
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
  const ended = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
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
  const lost = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  lost.apply(snapshot({ runs: [] }));
  expect(lost.outcome()).toMatchObject({ kind: "failed" });
});

it("refuses a resumed snapshot that omits a follow-up's parent run, keeping what it knew", () => {
  const delegated = (runs: unknown[]) => ({ runs, subagents: [task("task-1", "running")] });
  // The follow-up owns no run until T3 starts a continuation: only the parent
  // run it observed identifies the work it reports.
  const follow = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  follow.apply(snapshot(delegated([run("completed")])));
  expect(follow.run()?.id).toBe("run-1");
  expect(follow.outcome()).toBeUndefined();
  // A snapshot lists runs whole: one that omits the observed parent is
  // refused before it replaces anything.
  follow.resubscribing();
  expect(() =>
    follow.apply(snapshot(delegated([run("completed", "someone-else", 2)]), 900)),
  ).toThrow("T3 sent a thread snapshot that omits run run-1 this turn observed");
  expect(follow.sequence()).toBe(1);
  expect(follow.synchronized()).toBe(false);
  // One that lists it resumes the fold, and the continuation T3 starts for
  // the work is still the follow-up's.
  follow.apply(snapshot(delegated([run("completed"), run("completed", "someone-else", 2)]), 900));
  expect(follow.sequence()).toBe(900);
  follow.apply(
    event(
      901,
      "subagent.updated",
      task("task-1", "completed", "run-1", { completionDelivery: { state: "claimed" } }),
    ),
  );
  follow.apply(
    event(902, "run.updated", {
      ...run("completed"),
      delegatedCompletion: { disposition: "open", delivery: { messageId: "continuation-1" } },
    }),
  );
  follow.apply(event(903, "run.created", run("running", "continuation-1", 3)));
  expect(follow.activeRun()?.id).toBe("run-3");
  follow.apply(
    event(904, "message.updated", reply("c", "run-3", "Reported.", "2026-10-06T08:00:00Z")),
  );
  follow.apply(event(905, "run.updated", run("completed", "continuation-1", 3)));
  expect(follow.outcome()).toMatchObject({ kind: "completed", output: "Reported." });
});

it("links a continuation queued behind its active parent, or announced only after it was seen", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
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
  const watch = watchTurn("thread", "message-1", "/workspace");
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

it("keeps confirmed outcomes when a resumed snapshot omits finished children, and only live ones become unknown", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  const delivered = { completionDelivery: { state: "delivered" } };
  watch.apply(
    snapshot({
      runs: [run("running")],
      subagents: [
        task("done", "completed", "run-1", delivered),
        task("broke", "failed", "run-1", delivered),
        task("stopped", "cancelled", "run-1", delivered),
        task("live", "running"),
      ],
    }),
  );
  expect(watch.activity().work?.partial).toBe(false);
  watch.resubscribing();
  watch.apply(snapshot({ runs: [run("running")], subagents: [] }, 5));
  const states = Object.fromEntries(
    (watch.activity().work?.items ?? [])
      .filter((work) => work.kind === "delegation")
      .map((work) => [work.id, work.state]),
  );
  // What T3 confirmed stands; the missing present-day coverage is reported as partial.
  expect(states).toEqual({
    done: "succeeded",
    broke: "failed",
    stopped: "cancelled",
    live: "unknown",
  });
  expect(watch.activity().work?.partial).toBe(true);
  // The live child's report is still owed: its omission proves nothing.
  watch.apply(event(6, "run.updated", run("completed")));
  watch.apply(reply6());
  expect(watch.outcome()).toMatchObject({ kind: "completed", followUp: { after: 1 } });

  function reply6() {
    return event(7, "message.updated", {
      id: "a",
      runId: "run-1",
      role: "assistant",
      text: "started",
      createdAt: "2026-10-05T12:00:00Z",
    });
  }
});

it("bounds the work it keeps when every child is live, reporting partial coverage and an owed report", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("running")] }));
  for (let index = 0; index < 1000; index += 1)
    watch.apply(event(index + 2, "subagent.updated", task(`child-${String(index)}`, "running")));
  expect(watch.retained().delegations).toBe(32);
  const summary = watch.activity().work;
  expect(summary?.items).toHaveLength(16);
  expect(summary?.partial).toBe(true);
  // Children the bound dropped may still owe a report: the answer carries a cursor.
  watch.apply(event(2000, "run.updated", run("completed")));
  expect(watch.outcome()).toMatchObject({ kind: "completed", followUp: { after: 1 } });
  // Settled children make room for later ones; overflow alone never proves delivery.
  const settled = { completionDelivery: { state: "delivered" } };
  for (let index = 0; index < 32; index += 1)
    watch.apply(
      event(
        2001 + index,
        "subagent.updated",
        task(`child-${String(index)}`, "completed", "run-1", settled),
      ),
    );
  watch.apply(event(3000, "subagent.updated", task("late", "running")));
  expect(watch.retained().delegations).toBe(32);
  expect(watch.activity().work?.items.some((work) => work.id === "late")).toBe(true);
  expect(watch.outcome()).toMatchObject({ followUp: { after: 1 } });

  // A snapshot lists children whole, so coverage and the owed report are decided afresh.
  watch.resubscribing();
  watch.apply(
    snapshot(
      {
        runs: [run("completed")],
        subagents: [
          ...Array.from({ length: 1000 }, (_, index) =>
            task(`child-${String(index)}`, "completed", "run-1", settled),
          ),
          task("late", "completed", "run-1", settled),
        ],
      },
      4000,
    ),
  );
  expect(watch.retained().delegations).toBeLessThanOrEqual(32);
  expect(watch.activity().work?.partial).toBe(true);
  expect(watch.outcome()).toEqual({ kind: "completed", output: "" });
});

it("keeps only the turn's continuation links and a bounded window of others, across a large foreign history", () => {
  const watch = watchTurn("thread", "message-current", "/workspace");
  const history = Array.from({ length: 500 }, (_, index) => index);
  watch.apply(
    snapshot({
      runs: history.map((index) => ({
        ...run("completed", `message-${String(index)}`, index + 1, `run-${String(index)}`),
        delegatedCompletion: {
          disposition: "open",
          delivery: { messageId: `continuation-${String(index)}` },
        },
      })),
      messages: history.map((index) => ({
        id: `continuation-${String(index)}`,
        runId: null,
        role: "user",
        text: "",
        createdAt: "2026-10-05T10:00:00Z",
        delegatedCompletion: { parentRunId: `run-${String(index)}` },
      })),
    }),
  );
  expect(watch.retained().links).toBeLessThanOrEqual(16);
  // The turn runs and starts background work, so it owes a wake.
  const thread = (taskIds: string[]) => ({
    id: "provider-thread",
    providerSessionId: "session",
    pendingBackgroundTasks: taskIds.map((taskId) => ({ taskId, kind: "command" })),
  });
  watch.apply(event(2, "run.created", run("starting", "message-current", 501, "run-500")));
  watch.apply(event(3, "run.updated", run("running", "message-current", 501, "run-500")));
  watch.apply(event(4, "provider-thread.updated", thread(["own"])));
  watch.apply(event(5, "run.updated", run("completed", "message-current", 501, "run-500")));
  watch.apply(event(6, "provider-thread.updated", thread([])));
  // Another turn's work is reported next: T3 announces its continuation before its run.
  watch.apply(
    event(7, "run.updated", {
      ...run("completed", "message-499", 500, "run-499"),
      delegatedCompletion: { disposition: "open", delivery: { messageId: "late-report" } },
    }),
  );
  watch.apply(event(8, "run.created", run("running", "late-report", 501)));
  // That continuation is not this turn's wake, nor a stray.
  expect(watch.outcome()).toBeUndefined();
  expect(watch.activeRun()).toBeUndefined();
  expect(watch.retained().links).toBeLessThanOrEqual(16);

  // A link may name its parent before the fold knows the parent is the turn's.
  watch.apply(
    event(9, "message.updated", {
      id: "own-report",
      runId: null,
      role: "user",
      text: "",
      createdAt: "2026-10-05T12:00:00Z",
      delegatedCompletion: { parentRunId: "run-502" },
    }),
  );
  watch.apply(event(10, "run.updated", run("completed", "late-report", 501)));
  // The turn's own wake, which delegated in turn.
  watch.apply(event(11, "run.created", run("running", "wake", 502)));
  watch.apply(event(12, "turn-item.updated", wakeOpening("run-502")));
  watch.apply(
    event(13, "message.updated", {
      id: "w",
      runId: "run-502",
      role: "assistant",
      text: "Result.",
      createdAt: "2026-10-05T12:00:01Z",
    }),
  );
  watch.apply(event(14, "run.updated", run("completed", "wake", 502)));
  // Its continuation, announced before its parent was known, is linked to the turn.
  expect(watch.outcome()).toMatchObject({ kind: "completed", output: "Result.", followUp: {} });
});

it("resumes a follow-up on a fresh snapshot while T3 still owes the delivery, and links the continuation by its notification", () => {
  const follow = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  // After a restart: the parent completed, its child finished, and T3 has
  // claimed the result but not yet started the continuation.
  follow.apply(
    snapshot({
      runs: [run("completed")],
      subagents: [
        task("child", "completed", "run-1", { completionDelivery: { state: "claimed" } }),
      ],
    }),
  );
  expect(follow.outcome()).toBeUndefined();
  expect(follow.activity()).toMatchObject({
    phase: "waiting",
    work: {
      items: [
        { id: "report:message-1", state: "queued" },
        { id: "child", state: "succeeded" },
      ],
    },
  });
  // The continuation starts: its message is not projected and the parent's
  // announcement was cleared; its first item names the task.
  follow.apply(event(2, "run.created", run("running", "message:continuation", 2)));
  follow.apply(
    event(3, "turn-item.updated", {
      id: "notification-2",
      runId: "run-2",
      type: "notification",
      status: "completed",
      ordinal: 1,
      title: null,
      startedAt: null,
      completedAt: null,
      updatedAt: "2026-10-05T12:00:00Z",
      source: { kind: "delegated_task", taskIds: ["child"] },
      outcome: "completed",
      summary: "Delegated task finished",
    }),
  );
  follow.apply(
    event(
      4,
      "subagent.updated",
      task("child", "completed", "run-1", { completionDelivery: { state: "delivered" } }),
    ),
  );
  follow.apply(
    event(5, "message.updated", {
      id: "r",
      runId: "run-2",
      role: "assistant",
      text: "The child said PONG.",
      createdAt: "2026-10-05T12:00:01Z",
    }),
  );
  follow.apply(event(6, "run.updated", run("completed", "message:continuation", 2)));
  expect(follow.outcome()).toEqual({ kind: "completed", output: "The child said PONG." });
});

// Acknowledgement is consumption, not ownership: T3 sets `observedByRunId` to
// whichever run was active when an agent read the result.
const acknowledged = (observedByRunId: string) => ({
  completionDelivery: { state: "acknowledged", observedByRunId },
});
const READ_ELSEWHERE =
  "The agent read the result of the delegated work in another run of this conversation, not in a report of its own; T3 shows it.";
const userMessageItem = (runId: string) => ({
  id: `input-${runId}`,
  runId,
  type: "user_message",
  status: "completed",
  ordinal: 0,
  title: null,
  startedAt: null,
  completedAt: null,
  updatedAt: "2026-10-05T12:00:00Z",
});
const notificationItem = (runId: string, taskIds: readonly string[]) => ({
  id: `notification-${runId}`,
  runId,
  type: "notification",
  status: "completed",
  ordinal: 0,
  title: null,
  startedAt: null,
  completedAt: null,
  updatedAt: "2026-10-05T12:00:00Z",
  source: { kind: "delegated_task", taskIds },
  outcome: "completed",
  summary: "Delegated task finished",
});

it("refuses an opening item whose run it cannot identify before moving past it", () => {
  // The turn's run completed owing a wake: the run after it waits for its opening.
  const owing = () => {
    const watch = watchTurn("thread", "message-1", "/workspace");
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
      }),
    );
    watch.apply(
      event(2, "provider-thread.updated", {
        id: "provider-thread",
        providerSessionId: "session",
        pendingBackgroundTasks: [],
      }),
    );
    watch.apply(event(3, "run.created", run("running", "wake", 2)));
    return watch;
  };
  const naming = (opening: Record<string, unknown>, runId: unknown): Record<string, unknown> => {
    const named = Object.entries(opening).filter(([key]) => key !== "runId");
    return Object.fromEntries(runId === undefined ? named : [...named, ["runId", runId]]);
  };
  for (const opening of [wakeOpening("run-2"), userMessageItem("run-2")])
    for (const runId of [undefined, 7, ""]) {
      const watch = owing();
      expect(() => watch.apply(event(4, "turn-item.updated", naming(opening, runId)))).toThrow(
        `T3 sent a ${opening.type} item this runtime does not understand`,
      );
      expect(watch.sequence()).toBe(3);
    }
  // T3 may leave such an item runless, and items of kinds this runtime does
  // not read are passed over; the wake's own opening still opens it.
  const watch = owing();
  watch.apply(event(4, "turn-item.updated", naming(wakeOpening("run-2"), null)));
  watch.apply(event(5, "turn-item.updated", naming(userMessageItem("run-2"), null)));
  watch.apply(event(6, "turn-item.updated", { ...item("future_kind", "run-2", {}), runId: 7 }));
  expect(watch.sequence()).toBe(6);
  expect(watch.activeRun()).toBeUndefined();
  watch.apply(event(7, "turn-item.updated", wakeOpening("run-2")));
  expect(watch.activeRun()?.id).toBe("run-2");
  // A notification from a source this runtime does not know opens the run
  // without attributing it.
  const unknown = owing();
  unknown.apply(
    event(4, "turn-item.updated", { ...wakeOpening("run-2"), source: { kind: "future_source" } }),
  );
  expect(unknown.sequence()).toBe(4);
  expect(unknown.activeRun()).toBeUndefined();
});

it("shares the provider session while another run's background work is listed, not only while it runs", () => {
  const roster = (sequence: number, tasks: readonly string[]) =>
    event(sequence, "provider-thread.updated", {
      id: "provider-thread",
      providerSessionId: "session",
      pendingBackgroundTasks: tasks.map((taskId) => ({ taskId, kind: "command" })),
    });
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("running")] }));
  watch.apply(roster(2, ["own-task"]));
  watch.apply(event(3, "run.updated", run("completed")));
  // The roster empties while the turn's wake is owed.
  watch.apply(roster(4, []));
  expect(watch.sessionShared()).toBe(false);
  // A person's run starts work of its own and completes, leaving it listed.
  watch.apply(event(5, "run.created", run("running", "ui-message", 2)));
  watch.apply(event(6, "turn-item.updated", userMessageItem("run-2")));
  expect(watch.sessionShared()).toBe(true);
  watch.apply(roster(7, ["ui-task"]));
  watch.apply(event(8, "run.updated", run("completed", "ui-message", 2)));
  // Nothing occupies the thread for a new message, yet the session is shared.
  expect(watch.busy()).toBe(false);
  expect(watch.sessionShared()).toBe(true);
  // Once T3 no longer lists that work, the session is the turn's alone again.
  watch.apply(roster(9, []));
  expect(watch.sessionShared()).toBe(false);
});

it("never adopts another message's run because an agent read the result in it (coordinator reproduction)", () => {
  const follow = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  follow.apply(
    snapshot({
      runs: [run("completed", "message-1", 1), run("completed", "ui-message", 2)],
      messages: [
        {
          id: "ui-answer",
          runId: "run-2",
          role: "assistant",
          text: "UNRELATED UI ANSWER (already delivered by its own turn)",
          createdAt: "2026-10-05T12:00:00Z",
        },
      ],
      turnItems: [userMessageItem("run-2")],
      subagents: [task("child", "completed", "run-1", acknowledged("run-2"))],
    }),
  );
  expect(follow.outcome()).toEqual({ kind: "completed", output: READ_ELSEWHERE });
  expect(follow.retained().runs).toBe(0);
});

it("never adopts another message's run from a live acknowledgement, even one named before the run appears", () => {
  const follow = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  follow.apply(snapshot({ runs: [run("completed")], subagents: [task("child", "running")] }));
  // The acknowledgement names a run the fold has not seen yet.
  follow.apply(
    event(2, "subagent.updated", task("child", "completed", "run-1", acknowledged("run-2"))),
  );
  follow.apply(event(3, "run.created", run("running", "ui-message", 2)));
  follow.apply(event(4, "turn-item.updated", userMessageItem("run-2")));
  follow.apply(
    event(5, "message.updated", {
      id: "ui-answer",
      runId: "run-2",
      role: "assistant",
      text: "UNRELATED UI ANSWER",
      createdAt: "2026-10-05T12:00:00Z",
    }),
  );
  follow.apply(event(6, "run.updated", run("completed", "ui-message", 2)));
  expect(follow.outcome()).toEqual({ kind: "completed", output: READ_ELSEWHERE });
  expect(follow.retained().runs).toBe(0);
});

it("reports a real continuation that read its own result, live and after a restart, without a note", () => {
  const live = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  live.apply(snapshot({ runs: [run("completed")], subagents: [task("child", "running")] }));
  live.apply(event(2, "run.created", run("running", "continuation", 2)));
  live.apply(event(3, "turn-item.updated", notificationItem("run-2", ["child"])));
  live.apply(
    event(4, "subagent.updated", task("child", "completed", "run-1", acknowledged("run-2"))),
  );
  live.apply(
    event(5, "message.updated", {
      id: "report",
      runId: "run-2",
      role: "assistant",
      text: "The child said PONG.",
      createdAt: "2026-10-05T12:00:00Z",
    }),
  );
  live.apply(event(6, "run.updated", run("completed", "continuation", 2)));
  expect(live.outcome()).toEqual({ kind: "completed", output: "The child said PONG." });

  const resumed = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  resumed.apply(
    snapshot({
      runs: [run("completed", "message-1", 1), run("completed", "continuation", 2)],
      messages: [
        {
          id: "report",
          runId: "run-2",
          role: "assistant",
          text: "The child said PONG.",
          createdAt: "2026-10-05T12:00:00Z",
        },
      ],
      turnItems: [notificationItem("run-2", ["child"])],
      subagents: [task("child", "completed", "run-1", acknowledged("run-2"))],
    }),
  );
  expect(resumed.outcome()).toEqual({ kind: "completed", output: "The child said PONG." });
});

it("keeps a pending announcement owed when a snapshot meets a full set of links (coordinator reproduction), and reports it when it runs", () => {
  const base = { runs: [run("completed", "message-1", 1)] };
  const overflow = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 33 } });
  overflow.apply(snapshot(base));
  const links = Array.from({ length: 32 }, (_, index) => ({
    id: `c${String(index + 1)}`,
    runId: `run-${String(index + 2)}`,
    role: "user",
    text: "",
    createdAt: "2026-10-05T12:00:00Z",
    delegatedCompletion: { parentRunId: "run-1" },
  }));
  links.forEach((link, index) => overflow.apply(event(index + 2, "message.updated", link)));
  overflow.apply(
    snapshot(
      {
        runs: [
          {
            ...run("completed", "message-1", 1),
            delegatedCompletion: { disposition: "open", delivery: { messageId: "pending-33" } },
          },
          ...links.map((link, index) => run("completed", link.id, index + 2)),
        ],
        messages: links,
      },
      40,
    ),
  );
  // The announced continuation is still owed: the report waits for it.
  expect(overflow.outcome()).toBeUndefined();
  expect(overflow.retained().links).toBeLessThanOrEqual(32 + 16);
  // Its continuation runs, and the report is delivered.
  overflow.apply(event(41, "run.created", run("running", "pending-33", 34)));
  overflow.apply(
    event(42, "message.updated", {
      id: "r34",
      runId: "run-34",
      role: "assistant",
      text: "The 33rd result.",
      createdAt: "2026-10-05T12:01:00Z",
    }),
  );
  overflow.apply(event(43, "run.updated", run("completed", "pending-33", 34)));
  expect(overflow.outcome()).toEqual({ kind: "completed", output: "The 33rd result." });
});

it("counts announced continuations beyond the bound as owed and partial when none has run", () => {
  const watch = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  watch.apply(snapshot({ runs: [run("completed")] }));
  for (let index = 0; index < 40; index += 1)
    watch.apply(
      event(index + 2, "message.updated", {
        id: `owed-${String(index)}`,
        runId: null,
        role: "user",
        text: "",
        createdAt: "2026-10-05T12:00:00Z",
        delegatedCompletion: { parentRunId: "run-1" },
      }),
    );
  expect(watch.retained().links).toBeLessThanOrEqual(32 + 16);
  expect(watch.outcome()).toBeUndefined();
  expect(watch.activity().work?.partial).toBe(true);
});

it("keeps linking live continuations beyond the bound, making room from those that ran, so the report is not left owed", () => {
  for (const linkedBy of ["notification", "message"] as const) {
    const follow = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
    follow.apply(snapshot({ runs: [run("completed")] }));
    let sequence = 2;
    const delivered = { completionDelivery: { state: "delivered" } };
    for (let index = 0; index < 40; index += 1) {
      const ordinal = index + 2;
      const runId = `run-${String(ordinal)}`;
      const child = `child-${String(index)}`;
      const message = `continuation-${String(index)}`;
      follow.apply(event(sequence++, "subagent.updated", task(child, "running")));
      follow.apply(
        event(sequence++, "subagent.updated", task(child, "completed", "run-1", delivered)),
      );
      // T3 links a continuation by its own message, then starts its run.
      if (linkedBy === "message")
        follow.apply(
          event(sequence++, "message.updated", {
            id: message,
            runId,
            role: "user",
            text: "",
            createdAt: "2026-10-05T12:00:00Z",
            delegatedCompletion: { parentRunId: "run-1" },
          }),
        );
      follow.apply(event(sequence++, "run.created", run("running", message, ordinal)));
      if (linkedBy === "notification")
        follow.apply(event(sequence++, "turn-item.updated", notificationItem(runId, [child])));
      follow.apply(
        event(
          sequence++,
          "message.updated",
          reply(
            `report-${String(index)}`,
            runId,
            `REPORT ${String(index)}`,
            `2026-10-05T12:${String(index).padStart(2, "0")}:00Z`,
          ),
        ),
      );
      follow.apply(event(sequence++, "run.updated", run("completed", message, ordinal)));
    }
    const outcome = follow.outcome();
    expect(outcome, linkedBy).toMatchObject({ kind: "completed" });
    expect(outcome, linkedBy).not.toHaveProperty("followUp");
    expect(outcome?.kind === "completed" && outcome.output.endsWith("REPORT 39"), linkedBy).toBe(
      true,
    );
    expect(follow.retained().links, linkedBy).toBeLessThanOrEqual(32);
  }
});

// Direct background work: T3 links no wake to the work it reports, so only a
// run T3 opened for the provider can be the turn's wake.
const backgroundRoot = {
  runs: [run("completed")],
  providerThreads: [
    {
      id: "provider-thread",
      providerSessionId: "session",
      pendingBackgroundTasks: [{ taskId: "t", kind: "command" }],
    },
  ],
  messages: [
    {
      id: "root-answer",
      runId: "run-1",
      role: "assistant",
      text: "Started own background work.",
      createdAt: "2026-10-05T12:00:00Z",
    },
  ],
};
const answerOf = (id: string, runId: string, text: string) => ({
  id,
  runId,
  role: "assistant",
  text,
  createdAt: "2026-10-05T12:00:05Z",
});

it("never owns, answers with or waits on a run a person's message opened while its wake is owed (coordinator reproduction)", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(snapshot(backgroundRoot));
  watch.apply(
    event(2, "provider-thread.updated", {
      id: "provider-thread",
      providerSessionId: "session",
      pendingBackgroundTasks: [],
    }),
  );
  // The coordinator's order: the run is created running, then its opening item.
  watch.apply(event(3, "run.created", run("running", "ui-message", 2)));
  watch.apply(event(4, "turn-item.updated", userMessageItem("run-2")));
  watch.apply(event(5, "message.updated", answerOf("ui-answer", "run-2", "UNRELATED UI ANSWER")));
  watch.apply(event(6, "run.updated", run("completed", "ui-message", 2)));
  expect(watch.outcome()).toBeUndefined();
  expect(watch.activeRun()).toBeUndefined();
  expect(watch.latestRun()?.id).toBe("run-1");
  expect(watch.retained().runs).toBe(1);
  // A person's run queued while the turn waits is not judged before it starts, nor after.
  watch.apply(event(7, "run.created", run("queued", "ui-later", 3)));
  expect(watch.busy()).toBe(true);
  watch.apply(event(8, "turn-item.updated", userMessageItem("run-3")));
  watch.apply(event(9, "run.updated", run("running", "ui-later", 3)));
  expect(watch.activeRun()).toBeUndefined();
  watch.apply(event(10, "run.updated", run("completed", "ui-later", 3)));
  // The wake T3 opens with the provider's notification is the turn's.
  watch.apply(event(11, "run.created", run("running", "wake", 4)));
  watch.apply(event(12, "turn-item.updated", wakeOpening("run-4")));
  expect(watch.activeRun()?.id).toBe("run-4");
  watch.apply(event(13, "message.updated", answerOf("own-result", "run-4", "OWN RESULT.")));
  watch.apply(event(14, "run.updated", run("completed", "wake", 4)));
  expect(watch.outcome()).toEqual({
    kind: "completed",
    output: "Started own background work.\n\nOWN RESULT.",
  });
  expect(watch.retained().runs).toBe(2);
});

it("tells a person's run from the turn's wake in a resumed snapshot by how T3 opened each", () => {
  const resumed = (projection: Record<string, unknown>) => {
    const watch = watchTurn("thread", "message-1", "/workspace");
    watch.apply(snapshot(backgroundRoot));
    watch.resubscribing();
    watch.apply(
      snapshot(
        {
          ...backgroundRoot,
          providerThreads: [{ id: "provider-thread", providerSessionId: "session" }],
          ...projection,
        },
        40,
      ),
    );
    return watch;
  };
  const ui = run("completed", "ui-message", 2);
  const uiAnswer = answerOf("ui-answer", "run-2", "UNRELATED UI ANSWER");
  // Only the person's run followed: it is not the wake, which is still owed.
  const waiting = resumed({
    runs: [run("completed"), ui],
    messages: [...backgroundRoot.messages, uiAnswer],
    turnItems: [userMessageItem("run-2")],
  });
  expect(waiting.outcome()).toBeUndefined();
  expect(waiting.retained().runs).toBe(1);
  // The wake followed it: the answer is the turn's own.
  const answered = resumed({
    runs: [run("completed"), ui, run("completed", "wake", 3)],
    messages: [
      ...backgroundRoot.messages,
      uiAnswer,
      answerOf("own-result", "run-3", "OWN RESULT."),
    ],
    turnItems: [userMessageItem("run-2"), wakeOpening("run-3")],
  });
  expect(answered.outcome()).toEqual({
    kind: "completed",
    output: "Started own background work.\n\nOWN RESULT.",
  });
  // A started run whose opening the snapshot does not show cannot be told: never adopted.
  const unknown = resumed({
    runs: [run("completed"), ui],
    messages: [...backgroundRoot.messages, uiAnswer],
  });
  expect(unknown.outcome()).toMatchObject({ kind: "unresolved" });
  expect(unknown.retained().runs).toBe(1);
});

it("says only that a result was acknowledged when T3 names no run it was read in (coordinator reproduction)", () => {
  const follow = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  follow.apply(
    snapshot({
      runs: [run("completed")],
      subagents: [
        task("child", "completed", "run-1", {
          completionDelivery: { state: "acknowledged", observedByRunId: null },
        }),
      ],
    }),
  );
  expect(follow.outcome()).toEqual({
    kind: "completed",
    output:
      "T3 records the result of the delegated work as acknowledged, so no report of its own follows here; T3 shows it.",
  });
  // A named run that is not the report's is still said to be another run.
  const named = watchTurn("thread", "message-1", "/workspace", { followUp: { after: 1 } });
  named.apply(
    snapshot({
      runs: [run("completed"), run("completed", "ui-message", 2)],
      turnItems: [userMessageItem("run-2")],
      subagents: [task("child", "completed", "run-1", acknowledged("run-2"))],
    }),
  );
  expect(named.outcome()).toEqual({ kind: "completed", output: READ_ELSEWHERE });
});

// Another provider's process on the thread: its wake names no work, and runs
// on its own provider thread, not the one holding this turn's work.
const ownRoot = {
  ...run("completed", "own-message", 2, "own-run"),
  providerThreadId: "own-provider",
};
const olderRun = {
  ...run("completed", "old-message", 1, "old-run"),
  providerThreadId: "old-provider",
};
const ownProvider = (tasks: string[]) => ({
  id: "own-provider",
  providerSessionId: "own-session",
  pendingBackgroundTasks: tasks.map((taskId) => ({ taskId, kind: "command" })),
});
const olderProvider = (tasks: string[]) => ({
  id: "old-provider",
  providerSessionId: "old-session",
  pendingBackgroundTasks: tasks.map((taskId) => ({ taskId, kind: "command" })),
});
const twoProviders = {
  runs: [olderRun, ownRoot],
  providerThreads: [ownProvider(["own-task"]), olderProvider(["old-task"])],
  providerSessions: [
    { id: "own-session", status: "ready" },
    { id: "old-session", status: "ready" },
  ],
  messages: [answerOf("own-answer", "own-run", "Started own work.")],
};
const olderWake = (status: string, ordinal = 3) => ({
  ...run(status, "older-task-notification", ordinal, "older-task-wake"),
  providerThreadId: "old-provider",
});
const ownWake = (status: string, ordinal = 4) => ({
  ...run(status, "own-task-notification", ordinal, "own-task-wake"),
  providerThreadId: "own-provider",
});

it("never adopts another provider's wake while its own work is listed (coordinator reproduction)", () => {
  const watch = watchTurn("thread", "own-message", "/workspace");
  watch.apply(snapshot(twoProviders));
  watch.apply(event(2, "provider-thread.updated", olderProvider([])));
  watch.apply(event(3, "run.created", olderWake("running")));
  watch.apply(event(4, "turn-item.updated", wakeOpening("older-task-wake")));
  // Neither its active run nor its latest: cancellation targets only its own run.
  expect(watch.activeRun()).toBeUndefined();
  expect(watch.latestRun()?.id).toBe("own-run");
  expect(watch.busy()).toBe(true);
  watch.apply(
    event(5, "message.updated", answerOf("old-result", "older-task-wake", "FOREIGN OLDER RESULT")),
  );
  watch.apply(event(6, "run.updated", olderWake("completed")));
  // Its own task is still listed: not answered, not quiet.
  expect(watch.outcome()).toBeUndefined();
  expect(watch.quiet()).toBe(false);
  expect(watch.retained().runs).toBe(1);
  // Its own work finishes and its own provider's process wakes.
  watch.apply(event(7, "provider-thread.updated", ownProvider([])));
  watch.apply(event(8, "run.created", ownWake("running")));
  watch.apply(event(9, "turn-item.updated", wakeOpening("own-task-wake")));
  expect(watch.activeRun()?.id).toBe("own-task-wake");
  watch.apply(event(10, "message.updated", answerOf("own-result", "own-task-wake", "OWN RESULT.")));
  watch.apply(event(11, "run.updated", ownWake("completed")));
  expect(watch.outcome()).toEqual({
    kind: "completed",
    output: "Started own work.\n\nOWN RESULT.",
  });
  expect(watch.quiet()).toBe(true);
});

it("never adopts another provider's wake from a resumed snapshot", () => {
  const resumed = (projection: Record<string, unknown>) => {
    const watch = watchTurn("thread", "own-message", "/workspace");
    watch.apply(snapshot(twoProviders));
    watch.resubscribing();
    watch.apply(snapshot({ ...twoProviders, ...projection }, 30));
    return watch;
  };
  const foreignAnswer = answerOf("old-result", "older-task-wake", "FOREIGN OLDER RESULT");
  const waiting = resumed({
    runs: [olderRun, ownRoot, olderWake("completed")],
    providerThreads: [ownProvider(["own-task"]), olderProvider([])],
    messages: [...twoProviders.messages, foreignAnswer],
    turnItems: [wakeOpening("older-task-wake")],
  });
  expect(waiting.outcome()).toBeUndefined();
  expect(waiting.quiet()).toBe(false);
  expect(waiting.latestRun()?.id).toBe("own-run");
  const answered = resumed({
    runs: [olderRun, ownRoot, olderWake("completed"), ownWake("completed")],
    providerThreads: [ownProvider([]), olderProvider([])],
    messages: [
      ...twoProviders.messages,
      foreignAnswer,
      answerOf("own-result", "own-task-wake", "OWN RESULT."),
    ],
    turnItems: [wakeOpening("older-task-wake"), wakeOpening("own-task-wake")],
  });
  expect(answered.outcome()).toEqual({
    kind: "completed",
    output: "Started own work.\n\nOWN RESULT.",
  });
});

it("takes only a notification source T3 defines as a wake, and attributes nothing to a malformed or unknown one", () => {
  const opened = (source: unknown) => {
    const watch = watchTurn("thread", "message-1", "/workspace");
    watch.apply(snapshot(backgroundRoot));
    watch.apply(
      event(2, "provider-thread.updated", {
        id: "provider-thread",
        providerSessionId: "session",
        pendingBackgroundTasks: [],
      }),
    );
    watch.apply(event(3, "run.created", run("running", "woken", 2)));
    watch.apply(
      event(4, "turn-item.updated", {
        ...wakeOpening("run-2"),
        ...(source === undefined ? { source: undefined } : { source }),
      }),
    );
    const adopted = watch.activeRun()?.id;
    watch.apply(event(5, "message.updated", answerOf("answer", "run-2", "WAKE ANSWER")));
    watch.apply(event(6, "run.updated", run("completed", "woken", 2)));
    return { adopted, outcome: watch.outcome() };
  };
  // The coordinator's malformed and unknown sources.
  for (const source of [undefined, false, { kind: "unrecognised_future_kind" }]) {
    const { adopted, outcome } = opened(source);
    expect(adopted, JSON.stringify(source)).toBeUndefined();
    expect(outcome, JSON.stringify(source)).toMatchObject({ kind: "unresolved" });
  }
  // The kinds T3 sends for a wake, on the wire and decoded.
  for (const source of [
    { kind: "background_task" },
    { kind: "background_task", work: "subagent", childThreadId: "child" },
    { kind: "background_command" },
    { kind: "monitor" },
    { kind: "command" },
    { kind: "subagent" },
  ]) {
    const { adopted, outcome } = opened(source);
    expect(adopted, source.kind).toBe("run-2");
    expect(outcome, source.kind).toEqual({
      kind: "completed",
      output: "Started own background work.\n\nWAKE ANSWER",
    });
  }
});

it("shows each child once, whichever origin T3 gives it, beside its item and roster entry", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(
    snapshot({
      runs: [run("running")],
      providerThreads: [
        {
          id: "provider-thread",
          providerSessionId: "session",
          pendingBackgroundTasks: [
            { taskId: "bg-native", kind: "subagent", childThreadId: "thread:native" },
          ],
        },
      ],
      subagents: [
        // The provider's own child, with its tool item and its roster entry.
        task("native", "running", "run-1", { origin: "provider_native", title: "Explore" }),
        // T3's own delegated task, with its tool item.
        task("owned", "running", "run-1", { title: "Research" }),
      ],
      turnItems: [
        item("subagent", "run-1", { id: "item-native", subagentId: "native" }, "running"),
        item("subagent", "run-1", { id: "item-owned", subagentId: "owned" }, "running"),
      ],
    }),
  );
  const activity = watch.activity();
  expect(activity.running).toEqual([]);
  // Each child once, as its delegation, beside the report T3's own task owes.
  expect(activity.work?.items.map((work) => [work.id, work.kind, work.label])).toEqual([
    ["native", "delegation", "Explore"],
    ["owned", "delegation", "Research"],
    ["report:message-1", "task", "report back to this conversation"],
  ]);
  // Both items still count as the parent's calls.
  expect(activity.tool_calls).toBe(2);
});

it("times calls from T3's own dates, and estimates a stable start only when T3 gives none", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  const undated = (updatedAt: string) =>
    item(
      "command_execution",
      "run-1",
      { id: "undated", input: "make", startedAt: null, updatedAt },
      "running",
    );
  watch.apply(
    snapshot({
      runs: [run("running")],
      turnItems: [
        undated("2026-10-04T08:00:05Z"),
        item("command_execution", "run-1", { id: "dated", input: "ls" }, "running"),
        item("file_search", "run-1", {
          id: "done",
          pattern: "fn main",
          startedAt: "2026-10-04T08:00:00Z",
          completedAt: "2026-10-04T08:02:15.500Z",
        }),
      ],
    }),
  );
  const first = watch.activity();
  const estimate = first.running.find((call) => call.detail === "make");
  expect(estimate?.estimated).toBe(true);
  // T3's own start is used as given, and never marked as an estimate.
  expect(first.running.find((call) => call.detail === "ls")).toEqual({
    tool: "bash",
    detail: "ls",
    started_at: Date.parse("2026-10-04T08:00:00Z") / 1000,
  });
  expect(first.recent).toEqual([
    { tool: "grep", detail: '"fn main"', ok: true, duration_ms: 135_500 },
  ]);
  // Later updates and a resumed snapshot do not move the estimated start.
  watch.apply(event(2, "turn-item.updated", undated("2026-10-04T08:09:00Z")));
  watch.resubscribing();
  watch.apply(
    snapshot({ runs: [run("running")], turnItems: [undated("2026-10-04T08:10:00Z")] }, 3),
  );
  expect(watch.activity().running.find((call) => call.detail === "make")).toEqual(estimate);
  // A call that ends without both dates has no duration.
  watch.apply(
    event(4, "turn-item.updated", { ...undated("2026-10-04T08:11:00Z"), status: "completed" }),
  );
  expect(watch.activity().recent.at(-1)).toEqual({ tool: "bash", detail: "make", ok: true });
});

it("holds a pending or waiting call, counting and timing it only once T3 shows it running", () => {
  const at = (time: string) => Date.parse(`2026-10-04T${time}Z`);
  vi.useFakeTimers({ now: at("08:00:00"), toFake: ["Date"] });
  try {
    const watch = watchTurn("thread", "message-1", "/workspace");
    const call = (id: string, status: string, startedAt: string | null = null) =>
      item(
        "command_execution",
        "run-1",
        { id, input: id, startedAt, updatedAt: new Date().toISOString() },
        status,
      );
    // T3 lists both calls before running either.
    watch.apply(
      snapshot({
        runs: [run("running")],
        turnItems: [call("make", "pending"), call("ls", "pending")],
      }),
    );
    const listed = watch.activity();
    expect(listed).toMatchObject({ phase: "waiting", running: [], recent: [], tool_calls: 0 });
    // A resumed snapshot still showing them pending changes nothing.
    vi.setSystemTime(at("08:00:30"));
    watch.resubscribing();
    watch.apply(
      snapshot(
        { runs: [run("running")], turnItems: [call("make", "pending"), call("ls", "pending")] },
        2,
      ),
    );
    expect(watch.activity()).toEqual(listed);
    // An undated call's estimated start is when T3 first shows it running;
    // a dated one's is T3's own.
    vi.setSystemTime(at("08:01:00"));
    watch.apply(event(3, "turn-item.updated", call("make", "running")));
    watch.apply(event(4, "turn-item.updated", call("ls", "running", "2026-10-04T08:00:55Z")));
    const estimated = {
      tool: "bash",
      detail: "make",
      started_at: at("08:01:00") / 1000,
      estimated: true,
    };
    const dated = { tool: "bash", detail: "ls", started_at: at("08:00:55") / 1000 };
    expect(watch.activity()).toMatchObject({
      phase: "tools",
      running: [estimated, dated],
      tool_calls: 2,
    });
    // Waiting again, a call is held, still counted; running again, its start has not moved.
    vi.setSystemTime(at("08:02:00"));
    watch.apply(event(5, "turn-item.updated", call("make", "waiting")));
    watch.apply(event(6, "turn-item.updated", { ...call("ls", "completed"), startedAt: null }));
    expect(watch.activity()).toMatchObject({ phase: "waiting", running: [], tool_calls: 2 });
    // A snapshot resumed while it waits keeps it counted; a call it lists
    // that never ran is held uncounted.
    const ls = { ...call("ls", "completed"), startedAt: null };
    vi.setSystemTime(at("08:02:30"));
    watch.resubscribing();
    watch.apply(
      snapshot(
        {
          runs: [run("running")],
          turnItems: [call("make", "waiting"), ls, call("lint", "pending")],
        },
        7,
      ),
    );
    expect(watch.activity()).toMatchObject({ phase: "waiting", running: [], tool_calls: 2 });
    vi.setSystemTime(at("08:03:00"));
    watch.apply(event(8, "turn-item.updated", call("make", "running")));
    expect(watch.activity()).toMatchObject({ running: [estimated], tool_calls: 2 });
    watch.resubscribing();
    watch.apply(
      snapshot(
        {
          runs: [run("running")],
          turnItems: [call("make", "running"), ls, call("lint", "pending")],
        },
        9,
      ),
    );
    expect(watch.activity()).toMatchObject({ running: [estimated], tool_calls: 2 });
    watch.apply(event(10, "turn-item.updated", call("make", "completed")));
    expect(watch.activity()).toMatchObject({
      phase: "waiting",
      running: [],
      recent: [
        { tool: "bash", detail: "ls", ok: true },
        { tool: "bash", detail: "make", ok: true },
      ],
      tool_calls: 2,
    });
  } finally {
    vi.useRealTimers();
  }
});

it("keeps the reasoning T3 publishes for the turn's runs as notes, across snapshots, and none of other runs", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  const reasoning = (id: string, runId: string, text: string, status = "completed") =>
    item("reasoning", runId, { id, text }, status);
  watch.apply(
    snapshot({
      runs: [run("running", "message-1", 2, "run-1"), run("running", "message-0", 1, "run-0")],
      turnItems: [
        reasoning("thought-1", "run-1", "First, read the storage code.", "running"),
        reasoning("other", "run-0", "Another message's reasoning."),
      ],
    }),
  );
  expect(watch.activity().notes).toEqual(["First, read the storage code."]);
  // Streamed text replaces its own note in place; a new item adds one.
  watch.apply(
    event(
      2,
      "turn-item.updated",
      reasoning("thought-1", "run-1", "First, read the storage code\nand its tests."),
    ),
  );
  watch.apply(
    event(3, "turn-item.updated", reasoning("thought-2", "run-1", "Then compare the views.")),
  );
  const notes = ["First, read the storage code and its tests.", "Then compare the views."];
  expect(watch.activity().notes).toEqual(notes);
  // A bounded resumed snapshot that leaves them out does not forget them.
  watch.resubscribing();
  watch.apply(snapshot({ runs: [run("running", "message-1", 2, "run-1")] }, 9));
  expect(watch.activity().notes).toEqual(notes);
  expect(watch.outcome()).toBeUndefined();
});

it("says which model, effort and context the turn's run uses, as T3 records them", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  const selected = (model: string, options: { id: string; value: string | boolean }[]) => ({
    ...run("running"),
    modelSelection: { instanceId: "claudeAgent", model, options },
  });
  watch.apply(
    snapshot({
      runs: [
        selected("claude-sonnet-5", [
          { id: "effort", value: "high" },
          { id: "fastMode", value: false },
        ]),
      ],
      providerThreads: [
        {
          id: "provider-thread",
          providerSessionId: "session",
          contextUsage: { usedTokens: 45_000, maxTokens: 200_000 },
        },
      ],
    }),
  );
  expect(watch.activity().agent).toEqual({
    model: "claude-sonnet-5",
    effort: "high",
    context: { used: 45_000, window: 200_000 },
  });
  // Codex names its effort differently; a run without usage reports none.
  watch.apply(
    event(2, "provider-thread.updated", { id: "provider-thread", providerSessionId: "session" }),
  );
  watch.apply(
    event(3, "run.updated", selected("gpt-6", [{ id: "reasoningEffort", value: "xhigh" }])),
  );
  expect(watch.activity().agent).toEqual({ model: "gpt-6", effort: "xhigh" });
  expect(Check(TurnActivitySchema, watch.activity())).toBe(true);
});

it("dates a call and background work when the fold first sees them, so reading activity changes nothing", () => {
  vi.useFakeTimers();
  try {
    vi.setSystemTime(new Date("2026-10-04T08:00:00Z"));
    const watch = watchTurn("thread", "message-1", "/workspace");
    const listing = (tasks: { taskId: string; kind: string; description?: string }[]) => ({
      id: "provider-thread",
      providerSessionId: "session",
      pendingBackgroundTasks: tasks,
    });
    const call = item("command_execution", "run-1", { input: "make", startedAt: null }, "running");
    watch.apply(snapshot({ runs: [run("running")], turnItems: [call] }));
    watch.apply(
      event(
        2,
        "provider-thread.updated",
        listing([{ taskId: "bg", kind: "command", description: "sleep 300" }]),
      ),
    );
    const seenAt = Math.floor(Date.parse("2026-10-04T08:00:00Z") / 1000);
    // Read a minute later, and again: both show when the fold saw the work.
    vi.setSystemTime(new Date("2026-10-04T08:01:00Z"));
    const first = watch.activity();
    vi.setSystemTime(new Date("2026-10-04T08:02:00Z"));
    expect(watch.activity()).toEqual(first);
    expect(first.running).toEqual([
      { tool: "bash", detail: "make", started_at: seenAt, estimated: true },
      { tool: "command", detail: "sleep 300", started_at: seenAt, estimated: true },
    ]);
    // Work no longer listed is forgotten as it goes: listed again, it is new.
    watch.apply(event(3, "provider-thread.updated", listing([])));
    watch.apply(
      event(
        4,
        "provider-thread.updated",
        listing([{ taskId: "bg", kind: "command", description: "sleep 300" }]),
      ),
    );
    expect(watch.activity().running.at(-1)?.started_at).toBe(seenAt + 120);
  } finally {
    vi.useRealTimers();
  }
});
