import { TurnActivitySchema } from "@carillon/protocol";
import { Check } from "typebox/value";
import { expect, it, vi } from "vitest";
import { watchTurn } from "../watch.ts";
import { run, item, snapshot, event } from "./fixtures.ts";

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
