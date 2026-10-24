import {
  fauxAssistantMessage,
  fauxText,
  fauxThinking,
  fauxToolCall,
  Type,
} from "@earendil-works/pi-ai";
import { defineExtension, defineTool } from "@earendil-works/pi-durable";
import { afterEach, expect, it } from "vitest";
import { trackActivity } from "./activity.ts";
import { heldAnswer, pauseTool, startFixture, type Fixture } from "./fixture.ts";

const fixtures: Fixture[] = [];
afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.close()));
});

async function fixture(...args: Parameters<typeof startFixture>) {
  const started = await startFixture(...args);
  fixtures.push(started);
  return started;
}

const failing = defineExtension({
  name: "failing",
  tools: [
    defineTool({
      name: "fail",
      description: "Always fails.",
      parameters: Type.Object({}),
      execute: () => Promise.reject(new Error("failed on purpose")),
    }),
  ],
});

it("shows a run's thinking, commentary and tools while it runs, and reports them with its answer", async () => {
  const pause = pauseTool();
  const agent = await fixture({
    builtIns: [pause.extension, failing],
    defaults: ["pause", "failing"],
    answers: [
      fauxAssistantMessage(
        [
          fauxThinking("Planning the steps."),
          fauxText("Trying both tools."),
          fauxToolCall("fail", {}),
        ],
        { stopReason: "toolUse" },
      ),
      fauxAssistantMessage([fauxText("Now pausing."), fauxToolCall("pause", {})], {
        stopReason: "toolUse",
      }),
      fauxAssistantMessage("All done."),
    ],
  });
  await agent.runtime.turns.submit(agent.turn("turn-1", "work"));
  await pause.started;
  let running = agent.runtime.activity("turn-1", "discord-private");
  for (let attempt = 0; running?.running.length !== 1 && attempt < 100; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 10));
    running = agent.runtime.activity("turn-1", "discord-private");
  }
  expect(running).toMatchObject({
    phase: "tools",
    headline: "Now pausing.",
    running: [{ tool: "pause" }],
    recent: [{ tool: "fail", ok: false }],
    tool_calls: 2,
    notes: ["Planning the steps."],
    agent: { model: "scripted" },
  });
  pause.release();
  const outcome = await agent.outcome("turn-1");
  expect(outcome).toMatchObject({
    outcome: "answered",
    output: "All done.",
    activity: {
      headline: "All done.",
      running: [],
      recent: [
        { tool: "fail", ok: false },
        { tool: "pause", ok: true },
      ],
      tool_calls: 2,
    },
  });
  // Settled with core, the turn is no longer described.
  for (let attempt = 0; attempt < 100; attempt += 1) {
    if (agent.runtime.activity("turn-1", "discord-private") === undefined) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  expect.fail("the settled turn is still described");
});

/** Resolves once the turn's activity is in `phase`. */
async function reaches(agent: Fixture, turnId: string, phase: string): Promise<void> {
  for (let attempt = 0; attempt < 300; attempt += 1) {
    if (agent.runtime.activity(turnId, "discord-private")?.phase === phase) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  expect.fail(`${turnId} never reached phase ${phase}`);
}

it("shows a run waiting to ask its model again", async () => {
  const agent = await fixture({
    answers: [
      fauxAssistantMessage([], { stopReason: "error", errorMessage: "The model is overloaded." }),
      fauxAssistantMessage("Answered on retry."),
    ],
  });
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello"));
  await reaches(agent, "turn-1", "retrying");
  expect(await agent.outcome("turn-1")).toMatchObject({ output: "Answered on retry." });
});

it("shows a run compacting its context before it can answer", async () => {
  const summarized = Promise.withResolvers<void>();
  const agent = await fixture({
    answers: [
      fauxAssistantMessage("Long ".repeat(25_000)),
      fauxAssistantMessage([], { stopReason: "error", errorMessage: "prompt is too long" }),
      heldAnswer("The story so far.", summarized.promise),
      fauxAssistantMessage("Answered after compacting."),
    ],
  });
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello"));
  await agent.outcome("turn-1");
  await agent.runtime.turns.submit(agent.turn("turn-2", "again"));
  await reaches(agent, "turn-2", "compacting");
  summarized.resolve();
  expect(await agent.outcome("turn-2")).toMatchObject({ output: "Answered after compacting." });
});

it("lists the latest running and finished calls and counts them all", () => {
  const tracker = trackActivity("/work");
  tracker.observe({ type: "run_start", inputs: [] });
  for (let call = 0; call < 10; call += 1)
    tracker.observe({
      type: "tool_execution_start",
      toolCallId: `call-${String(call)}`,
      toolName: "bash",
      args: { command: `echo ${String(call)}` },
    });
  for (let call = 0; call < 7; call += 1)
    tracker.observe({
      type: "tool_execution_end",
      toolCallId: `call-${String(call)}`,
      toolName: "bash",
    });
  const snapshot = tracker.snapshot();
  expect(snapshot.tool_calls).toBe(10);
  expect(snapshot.running.map((call) => call.detail)).toEqual(["echo 7", "echo 8", "echo 9"]);
  expect(snapshot.recent.map((call) => call.detail)).toEqual([
    "echo 2",
    "echo 3",
    "echo 4",
    "echo 5",
    "echo 6",
  ]);
  expect(snapshot.recent.every((call) => call.ok && typeof call.duration_ms === "number")).toBe(
    true,
  );
  expect(snapshot.phase).toBe("tools");
});
