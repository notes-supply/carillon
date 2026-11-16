import { expect, test } from "vitest";
import type { Activity } from "../api/stream.ts";
import { ledger } from "./ledger.ts";

const ACCEPTED = 1000;
const NOW = 1134;
const IDLE: Activity = { phase: "model", running: [], recent: [], tool_calls: 0 };

test("a turn core has not handed over waits its turn", () => {
  const shown = ledger({ state: "queued" }, ACCEPTED, NOW);
  expect(shown.phase).toBe("Waiting its turn");
  expect(shown.waiting).toBe(true);
  expect(shown.tally).toBe("2 min 14 s");
});

test("a turn its runtime holds behind another waits its turn too", () => {
  const activity: Activity = { ...IDLE, phase: "queued" };
  const shown = ledger({ state: "running", activity, controls: [] }, ACCEPTED, NOW);
  expect(shown.phase).toBe("Waiting its turn");
  expect(shown.waiting).toBe(true);
});

test("a running turn whose runtime said nothing is at work", () => {
  const shown = ledger({ state: "running", activity: null, controls: [] }, ACCEPTED, NOW);
  expect({
    phase: shown.phase,
    waiting: shown.waiting,
    headline: shown.headline,
    marks: shown.marks,
  }).toStrictEqual({ phase: "At work", waiting: false, headline: "", marks: [] });
  expect(shown.counted).toBe("0 tool calls");
});

test("a ledger has a mark and a line for each call, and counts them in words", () => {
  const activity: Activity = {
    phase: "tools",
    tool_calls: 6,
    recent: [
      { tool: "grep", detail: '"beta" docs/', ok: true, duration_ms: 8 },
      { tool: "bash", ok: false, duration_ms: 41 },
      { tool: "read", ok: true },
    ],
    running: [{ tool: "bash", detail: "gh pr list", started_at: 1122, estimated: true }],
  };
  const shown = ledger({ state: "running", activity, controls: [] }, ACCEPTED, NOW);
  expect(shown.phase).toBe("Using tools");
  expect(shown.marks).toStrictEqual(["past", "past", "done", "failed", "done", "running"]);
  expect(shown.counted).toBe("6 tool calls, 1 under way, 1 failed, 2 finished");
  expect(shown.calls).toStrictEqual([
    { mark: "done", name: "grep", detail: '"beta" docs/', time: "8 ms" },
    { mark: "failed", name: "bash", detail: "", time: "failed, 41 ms" },
    { mark: "done", name: "read", detail: "", time: "" },
    { mark: "running", name: "bash", detail: "gh pr list", time: "about 12 s" },
  ]);
  expect(shown.tally).toBe("6 tool calls, 2 min 14 s");
});

test("a turn of very many calls keeps its row of marks bounded", () => {
  const activity: Activity = { ...IDLE, tool_calls: 400 };
  expect(ledger({ state: "running", activity, controls: [] }, ACCEPTED, NOW).marks.length).toBe(48);
});

test("the headline is the model's commentary, or else its latest note", () => {
  const noted: Activity = { ...IDLE, notes: ["First.", "Latest."] };
  const said: Activity = { ...noted, headline: "Reading." };
  expect(ledger({ state: "running", activity: noted, controls: [] }, ACCEPTED, NOW).headline).toBe(
    "Latest.",
  );
  expect(ledger({ state: "running", activity: said, controls: [] }, ACCEPTED, NOW).headline).toBe(
    "Reading.",
  );
});

test("delegated work and what the runtime left out are said in words", () => {
  const activity: Activity = {
    ...IDLE,
    work: {
      items: [
        {
          id: "w1",
          kind: "delegation",
          label: "Check the changelog",
          state: "running",
          actor: "scout",
          started_at: 1130,
        },
        { id: "w2", kind: "task", label: "Draft the summary", state: "queued" },
        {
          id: "w3",
          kind: "handoff",
          label: "To the reviewer",
          state: "succeeded",
          duration_ms: 2500,
        },
      ],
      partial: true,
      omitted: 2,
    },
    agent: { model: "gpt-6.1", effort: "high", context: { used: 18_250, window: 400_000 } },
    partial: true,
  };
  const shown = ledger({ state: "running", activity, controls: [] }, ACCEPTED, NOW);
  expect(shown.work).toStrictEqual([
    {
      mark: "running",
      name: "Delegated",
      detail: "Check the changelog (scout)",
      time: "running, 4 s",
    },
    { mark: "waiting", name: "Task", detail: "Draft the summary", time: "queued" },
    { mark: "done", name: "Handed off", detail: "To the reviewer", time: "done, 2 s" },
  ]);
  expect(shown.notes).toStrictEqual([
    "2 more not shown.",
    "The runtime could not see all of the delegated work.",
    "Part of this report was left out for its size.",
    "gpt-6.1 high, context 5% full",
  ]);
});
