import {
  fauxAssistantMessage,
  fauxToolCall,
  type FauxResponseStep,
  type JsonObject,
} from "@earendil-works/pi-ai";
import { defineExtension, Harness, type ToolRegistration } from "@earendil-works/pi-durable";
import { createCoreClient } from "@carillon/runtime/core";
import type { TurnRequest } from "@carillon/runtime/operations";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import { afterEach, expect, it, vi } from "vitest";
import { DELIVERY } from "./delivery.ts";
import { fakeClock, startFixture, textsOf, type Fixture } from "./fixture.ts";

const fixtures: Fixture[] = [];
afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.close()));
});

async function fixture(...args: Parameters<typeof startFixture>) {
  const started = await startFixture(...args);
  fixtures.push(started);
  return started;
}

/** The tool calls messages ask for, by message. */
const calls = new Map<string, { name: string; args: JsonObject }>();

/** A message asking the model to call the tool. */
function tool(name: string, args: JsonObject): string {
  const message = `tool ${name} ${JSON.stringify(args)}`;
  calls.set(message, { name, args });
  return message;
}

/**
 * Calls the tool a message asks for, says what a tool answered, and answers
 * anything else, such as a scheduled run's prompt, with its last text.
 */
const model: FauxResponseStep = (context) => {
  const last = context.messages.findLast((message) => message.role !== "system");
  const texts = textsOf(last);
  if (last?.role === "toolResult") return fauxAssistantMessage(`Tool: ${texts.join("")}`);
  const call = calls.get(texts.at(-1) ?? "");
  if (call !== undefined)
    return fauxAssistantMessage(fauxToolCall(call.name, call.args), { stopReason: "toolUse" });
  return fauxAssistantMessage(`Ran: ${texts.at(-1) ?? ""}`);
};
const answers = () => Array.from({ length: 40 }, () => model);

/** Lets anything that is due run. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 100));

/** Schedules the prompt, returning the schedule's ID. */
async function schedule(
  agent: Fixture,
  turnId: string,
  args: JsonObject,
  overrides?: Partial<TurnRequest>,
): Promise<number> {
  const answer = await agent.ask(turnId, tool("schedule", args), overrides);
  const id = /^Tool: Schedule (\d+) first runs at /u.exec(answer)?.[1];
  if (id === undefined) throw new Error(`not scheduled: ${answer}`);
  return Number(id);
}

/** The key a schedule's run opens its turn under: its agent's, and its store's. */
function runKey(id: number, run: number, agent = "default"): unknown {
  return expect.stringMatching(
    new RegExp(`^${agent}:[0-9a-f-]{36}:schedule:${String(id)}:${String(run)}$`, "u"),
  );
}

/** Waits until the runtime has asked core to open `count` turns. */
async function opened(agent: Fixture, count: number): Promise<void> {
  for (let attempt = 0; agent.openings().length < count && attempt < 100; attempt += 1)
    await settle();
  expect(agent.openings()).toHaveLength(count);
}

it("runs a schedule when it is due, in a turn it opens and settles with the answer", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  const agent = await fixture({ clock, defaults: ["schedules"], answers: answers() });
  const answer = await agent.ask(
    "turn-1",
    tool("schedule", { prompt: "Water the plants.", at: "2026-10-24T09:00:00Z", name: "plants" }),
  );
  const id = /^Tool: Schedule (\d+) first runs at 2026-10-24T09:00:00\.000Z\.$/u.exec(answer)?.[1];
  expect(id, answer).toBeDefined();
  clock.advance(59 * 60_000);
  await settle();
  expect(agent.openings()).toEqual([]);
  clock.advance(60_000);
  expect(await agent.outcome("opened-1")).toMatchObject({
    outcome: "answered",
    output: "Ran: Water the plants.",
  });
  expect(agent.openings()).toEqual([
    {
      interface_id: "discord-private",
      agent_id: "default",
      audience_id: "conversation",
      key: runKey(Number(id), 1),
      label: "Scheduled: plants",
    },
  ]);
});

it("runs a recurring schedule each time it is due, in its time zone", async () => {
  // 08:00 in Stockholm, the day before it leaves summer time.
  const clock = fakeClock("2026-10-24T06:00:00Z");
  const agent = await fixture({ clock, defaults: ["schedules"], answers: answers() });
  const id = await schedule(agent, "turn-1", {
    prompt: "Good morning.",
    cron: "0 9 * * *",
    timezone: "Europe/Stockholm",
  });
  clock.advance(60 * 60_000);
  expect(await agent.outcome("opened-1")).toMatchObject({ output: "Ran: Good morning." });
  expect(await agent.ask("turn-2", tool("schedule_list", {}))).toBe(
    `Tool: ${String(id)}: next at 2026-10-25T08:00:00.000Z, then on 0 9 * * * in Europe/Stockholm: Good morning.`,
  );
  // Summer time ends overnight, so the next 09:00 is 25 hours later.
  clock.advance(24 * 60 * 60_000);
  await settle();
  expect(agent.openings()).toHaveLength(1);
  clock.advance(60 * 60_000);
  expect(await agent.outcome("opened-2")).toMatchObject({ output: "Ran: Good morning." });
  expect(agent.openings()).toMatchObject([{ key: runKey(id, 1) }, { key: runKey(id, 2) }]);
});

it("cancels a schedule, which then never runs", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  const agent = await fixture({ clock, defaults: ["schedules"], answers: answers() });
  const id = await schedule(agent, "turn-1", {
    prompt: "Water the plants.",
    at: "2026-10-24T11:00:00+02:00",
  });
  expect(await agent.ask("turn-2", tool("schedule_list", {}))).toBe(
    `Tool: ${String(id)}: next at 2026-10-24T09:00:00.000Z, once: Water the plants.`,
  );
  expect(await agent.ask("turn-3", tool("schedule_cancel", { id }))).toBe(
    `Tool: Cancelled schedule ${String(id)}.`,
  );
  expect(await agent.ask("turn-4", tool("schedule_list", {}))).toBe(
    "Tool: This conversation has no schedules.",
  );
  expect(await agent.ask("turn-5", tool("schedule_cancel", { id }))).toContain(
    `This conversation has no schedule ${String(id)}.`,
  );
  clock.advance(2 * 60 * 60_000);
  await settle();
  expect(agent.openings()).toEqual([]);
});

it("ends a schedule that is due once the agent no longer selects schedules", async () => {
  const clock = fakeClock("2026-10-24T08:30:00Z");
  const agent = await fixture({ clock, defaults: ["schedules"], answers: answers() });
  await schedule(agent, "turn-1", { prompt: "Hourly.", cron: "0 * * * *", timezone: "UTC" });
  const without = await agent.snapshot({ "settings.json": JSON.stringify({ extensions: [] }) });
  await agent.runtime.turns.submit(agent.turn("turn-2", "hello", { resource_root: without }));
  await agent.outcome("turn-2");
  clock.advance(60 * 60_000);
  await settle();
  // Selected again, the schedule has ended.
  expect(await agent.ask("turn-3", tool("schedule_list", {}))).toBe(
    "Tool: This conversation has no schedules.",
  );
  clock.advance(60 * 60_000);
  await settle();
  expect(agent.openings()).toEqual([]);
});

it.each([
  ["both a time and a cron expression", { at: "2026-10-25T09:00:00Z", cron: "0 9 * * *" }],
  ["neither a time nor a cron expression", {}],
  ["a time that has passed", { at: "2026-10-24T07:00:00Z" }],
  ["a cron expression that is none", { cron: "every morning" }],
  ["a time that is not ISO 8601", { at: "2026-10-25 09:00" }],
  ["a cron expression that runs every second", { cron: "* * * * * *" }],
  ["an unknown time zone", { cron: "0 9 * * *", timezone: "Mars/Olympus_Mons" }],
])("refuses a schedule with %s", async (_name, when) => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  const agent = await fixture({ clock, defaults: ["schedules"], answers: answers() });
  const answer = await agent.ask("turn-1", tool("schedule", { prompt: "Hello.", ...when }));
  expect(answer).not.toMatch(/first runs/u);
  expect(await agent.ask("turn-2", tool("schedule_list", {}))).toBe(
    "Tool: This conversation has no schedules.",
  );
});

it("tries a due run again while core cannot open its turn", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  const agent = await fixture({ clock, defaults: ["schedules"], answers: answers() });
  await schedule(agent, "turn-1", { prompt: "Water the plants.", at: "2026-10-24T09:00:00Z" });
  agent.core.answer(503);
  clock.advance(60 * 60_000);
  await opened(agent, 1);
  clock.advance(1_000);
  expect(await agent.outcome("opened-1")).toMatchObject({ output: "Ran: Water the plants." });
  expect(agent.openings()).toHaveLength(2);
});

it("runs a schedule that fell due while the runtime was down once, then as it falls due", async () => {
  const clock = fakeClock("2026-10-24T08:30:00Z");
  let agent = await fixture({
    clock,
    durable: true,
    defaults: ["schedules"],
    answers: answers(),
  });
  const id = await schedule(agent, "turn-1", {
    prompt: "Hourly.",
    cron: "0 * * * *",
    timezone: "UTC",
  });
  agent = await agent.restart({ meanwhile: () => clock.advance(5 * 60 * 60_000) });
  fixtures.splice(0, 1, agent);
  expect(await agent.outcome("opened-1")).toMatchObject({ output: "Ran: Hourly." });
  await settle();
  expect(agent.openings()).toMatchObject([{ key: runKey(id, 1) }]);
  clock.advance(30 * 60_000);
  expect(await agent.outcome("opened-2")).toMatchObject({ output: "Ran: Hourly." });
  expect(agent.openings()).toMatchObject([{ key: runKey(id, 1) }, { key: runKey(id, 2) }]);
});

it("places a run once when a restart cuts it off between opening its turn and placing it", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  let agent = await fixture({
    clock,
    durable: true,
    defaults: ["schedules"],
    answers: answers(),
  });
  await schedule(agent, "turn-1", { prompt: "Water the plants.", at: "2026-10-24T09:00:00Z" });
  const calls = agent.model.state.callCount;
  // Core opens the turn, but the runtime stops before it hears so.
  const stall = agent.core.stall("/v1/runtime/turns");
  clock.advance(60 * 60_000);
  await stall.reached;
  agent = await agent.restart();
  fixtures.splice(0, 1, agent);
  stall.release();
  expect(await agent.outcome("opened-1")).toMatchObject({
    outcome: "answered",
    output: "Ran: Water the plants.",
  });
  await settle();
  const [first, second] = agent.openings();
  expect(agent.openings()).toHaveLength(2);
  expect(second).toEqual(first);
  expect(
    agent.core.received.filter((call) => call.path === "/v1/runtime/turns/opened-1/outcome"),
  ).toHaveLength(1);
  expect(agent.model.state.callCount).toBe(calls + 1);
});

it("skips a run core refuses for good, and runs the next", async () => {
  const clock = fakeClock("2026-10-24T08:30:00Z");
  const agent = await fixture({ clock, defaults: ["schedules"], answers: answers() });
  const id = await schedule(agent, "turn-1", { prompt: "Hourly.", cron: "0 * * * *" });
  agent.core.answer(409);
  clock.advance(30 * 60_000);
  await opened(agent, 1);
  clock.advance(60 * 60_000);
  expect(await agent.outcome("opened-1")).toMatchObject({ output: "Ran: Hourly." });
  expect(agent.openings()).toMatchObject([{ key: runKey(id, 1) }, { key: runKey(id, 2) }]);
});

it("keeps two agents' runs apart, though their stores number schedules alike", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  const agent = await fixture({
    clock,
    agents: ["default", "other"],
    defaults: ["schedules"],
    answers: answers(),
  });
  const ids = [];
  for (const agent_id of ["default", "other"])
    ids.push(
      await schedule(
        agent,
        `turn-${agent_id}`,
        { prompt: `Remind ${agent_id}.`, at: "2026-10-24T09:00:00Z" },
        { agent_id },
      ),
    );
  const [first, second] = ids;
  expect(first).toBe(second);
  clock.advance(60 * 60_000);
  const outputs = await Promise.all(
    ["opened-1", "opened-2"].map(async (turn) => {
      const outcome = await agent.outcome(turn);
      return outcome.outcome === "answered" ? outcome.output : outcome.outcome;
    }),
  );
  expect(outputs.toSorted()).toEqual(["Ran: Remind default.", "Ran: Remind other."]);
  expect(agent.openings()).toEqual(
    expect.arrayContaining([
      expect.objectContaining({ agent_id: "default", key: runKey(first ?? 0, 1, "default") }),
      expect.objectContaining({ agent_id: "other", key: runKey(second ?? 0, 1, "other") }),
    ]),
  );
});

it("opens a recreated store's runs anew, though it numbers them as the old one did", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  let agent = await fixture({ clock, defaults: ["schedules"], answers: answers() });
  const first = await schedule(agent, "turn-1", { prompt: "Water.", at: "2026-10-24T09:00:00Z" });
  clock.advance(60 * 60_000);
  expect(await agent.outcome("opened-1")).toMatchObject({ output: "Ran: Water." });
  agent = await agent.restart({ fresh: true });
  fixtures.splice(0, 1, agent);
  const second = await schedule(agent, "turn-2", { prompt: "Feed.", at: "2026-10-24T10:00:00Z" });
  expect(second).toBe(first);
  clock.advance(60 * 60_000);
  expect(await agent.outcome("opened-2")).toMatchObject({ output: "Ran: Feed." });
});

it("places no run whose turn core settled before the runtime heard it opened", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  let agent = await fixture({
    clock,
    durable: true,
    defaults: ["schedules"],
    answers: answers(),
  });
  await schedule(agent, "turn-1", { prompt: "Water the plants.", at: "2026-10-24T09:00:00Z" });
  const calls = agent.model.state.callCount;
  const stall = agent.core.stall("/v1/runtime/turns");
  clock.advance(60 * 60_000);
  await stall.reached;
  const { core } = agent;
  agent = await agent.restart({
    meanwhile: () =>
      createCoreClient({ url: core.url, providerId: "runtime-pi", token: "test-only" }).settle(
        "opened-1",
        { outcome: "stopped" },
      ),
  });
  fixtures.splice(0, 1, agent);
  stall.release();
  await opened(agent, 2);
  await settle();
  expect(agent.model.state.callCount).toBe(calls);
  expect(
    agent.core.received.filter((call) => call.path === "/v1/runtime/turns/opened-1/outcome"),
  ).toHaveLength(1);
});

it("stops a run's opened turn once schedules are deselected before it is placed", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  let agent = await fixture({
    clock,
    durable: true,
    defaults: ["schedules"],
    answers: answers(),
  });
  await schedule(agent, "turn-1", { prompt: "Water the plants.", at: "2026-10-24T09:00:00Z" });
  const stall = agent.core.stall("/v1/runtime/turns");
  clock.advance(60 * 60_000);
  await stall.reached;
  const without = await agent.snapshot({ "settings.json": JSON.stringify({ extensions: [] }) });
  await agent.runtime.turns.submit(agent.turn("turn-2", "hello", { resource_root: without }));
  await agent.outcome("turn-2");
  const calls = agent.model.state.callCount;
  agent = await agent.restart();
  fixtures.splice(0, 1, agent);
  stall.release();
  expect(await agent.outcome("opened-1")).toEqual({ outcome: "stopped" });
  expect(agent.model.state.callCount).toBe(calls);
});

it("keeps a due schedule while the agent's resources no longer load, and runs it once they do", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  let agent = await fixture({ clock, durable: true, answers: answers() });
  const settings = JSON.stringify({ extensions: ["schedules"] });
  const root = await agent.snapshot({ "settings.json": settings });
  await schedule(
    agent,
    "turn-1",
    { prompt: "Water the plants.", at: "2026-10-24T09:00:00Z" },
    { resource_root: root },
  );
  agent = await agent.restart({ meanwhile: () => writeFile(join(root, "settings.json"), "{") });
  fixtures.splice(0, 1, agent);
  clock.advance(60 * 60_000);
  await settle();
  clock.advance(10 * 60_000);
  await settle();
  expect(agent.openings()).toEqual([]);
  const loads = await agent.snapshot({ "settings.json": settings });
  await agent.ask("turn-2", "hello", { resource_root: loads });
  clock.advance(60_000);
  expect(await agent.outcome("opened-1")).toMatchObject({ output: "Ran: Water the plants." });
});

it.each([
  ["once", { at: "2026-10-24T09:00:00Z" }],
  ["hourly", { cron: "0 * * * *" }],
])("stops a run whose turn is opening when its schedule, %s, is cancelled", async (_name, when) => {
  const clock = fakeClock("2026-10-24T08:30:00Z");
  const agent = await fixture({ clock, defaults: ["schedules"], answers: answers() });
  const id = await schedule(agent, "turn-1", { prompt: "Hourly.", ...when });
  const stall = agent.core.stall("/v1/runtime/turns");
  clock.advance(30 * 60_000);
  await stall.reached;
  expect(await agent.ask("turn-2", tool("schedule_cancel", { id }))).toBe(
    `Tool: Cancelled schedule ${String(id)}.`,
  );
  const calls = agent.model.state.callCount;
  stall.release();
  expect(await agent.outcome("opened-1")).toEqual({ outcome: "stopped" });
  expect(agent.model.state.callCount).toBe(calls);
  clock.advance(60 * 60_000);
  await settle();
  expect(agent.openings()).toHaveLength(1);
});

it("runs a schedule once after core comes back from a day away", async () => {
  const clock = fakeClock("2026-10-24T08:30:00Z");
  const agent = await fixture({ clock, defaults: ["schedules"], answers: answers() });
  const id = await schedule(agent, "turn-1", { prompt: "Hourly.", cron: "0 * * * *" });
  const calls = agent.model.state.callCount;
  // Core takes the first run's opening, but its answer is lost for a day.
  const stall = agent.core.stall("/v1/runtime/turns");
  clock.advance(30 * 60_000);
  await stall.reached;
  for (let hour = 0; hour < 24; hour += 1) {
    clock.advance(60 * 60_000);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  await settle();
  expect(agent.openings()).toHaveLength(1);
  stall.release();
  expect(await agent.outcome("opened-1")).toMatchObject({ output: "Ran: Hourly." });
  await settle();
  expect(agent.openings()).toMatchObject([{ key: runKey(id, 1) }]);
  expect(agent.model.state.callCount).toBe(calls + 1);
  clock.advance(60 * 60_000);
  expect(await agent.outcome("opened-2")).toMatchObject({ output: "Ran: Hourly." });
  expect(agent.openings()).toMatchObject([{ key: runKey(id, 1) }, { key: runKey(id, 2) }]);
});

it("stops a run whose schedule is cancelled while its input is placed", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  const agent = await fixture({ clock, defaults: ["schedules"], answers: answers() });
  const id = await schedule(agent, "turn-1", {
    prompt: "Water the plants.",
    at: "2026-10-24T09:00:00Z",
  });
  // The run's model lookup waits, after the run was found due and before it is placed.
  const looking = Promise.withResolvers<void>();
  const lookup = Promise.withResolvers<void>();
  const { models } = agent;
  const getAvailable = models.getAvailable.bind(models);
  models.getAvailable = async (...args) => {
    models.getAvailable = getAvailable;
    looking.resolve();
    await lookup.promise;
    return getAvailable(...args);
  };
  clock.advance(60 * 60_000);
  await looking.promise;
  expect(await agent.ask("turn-2", tool("schedule_cancel", { id }))).toBe(
    `Tool: Cancelled schedule ${String(id)}.`,
  );
  lookup.resolve();
  expect(await agent.outcome("opened-1")).toEqual({ outcome: "stopped" });
});

it("stops a run whose schedule is cancelled while its input is placed, though it answers before the stop", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  // The opening carries on after tracking the run's turn only once core has its outcome.
  const open = Harness.open.bind(Harness);
  const opening = vi.spyOn(Harness, "open").mockImplementation(async (...args) => {
    const harness = await open(...args);
    const commit = harness.commit.bind(harness);
    harness.commit = async (change, context) => {
      let tracks = false;
      const result = await commit((tx) => {
        const createTask = tx.createTask.bind(tx);
        tx.createTask = (task, input, options) => {
          tracks ||=
            task.definition.name === DELIVERY && JSON.stringify(input).includes("opened-1");
          return createTask(task, input, options);
        };
        return change(tx);
      }, context);
      if (tracks) await agent.outcome("opened-1");
      return result;
    };
    return harness;
  });
  const agent = await fixture({ clock, defaults: ["schedules"], answers: answers() });
  opening.mockRestore();
  const id = await schedule(agent, "turn-1", {
    prompt: "Water the plants.",
    at: "2026-10-24T09:00:00Z",
  });
  const looking = Promise.withResolvers<void>();
  const lookup = Promise.withResolvers<void>();
  const { models } = agent;
  const getAvailable = models.getAvailable.bind(models);
  models.getAvailable = async (...args) => {
    models.getAvailable = getAvailable;
    looking.resolve();
    await lookup.promise;
    return getAvailable(...args);
  };
  clock.advance(60 * 60_000);
  await looking.promise;
  await agent.ask("turn-2", tool("schedule_cancel", { id }));
  lookup.resolve();
  expect(await agent.outcome("opened-1")).toEqual({ outcome: "stopped" });
});

it("makes one schedule when a restart cuts off the call that made it", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  const made = Promise.withResolvers<void>();
  let cut = true;
  // The first call is cut off once its schedule exists, before it returns.
  const crash = defineExtension({
    name: "crash",
    wraps: [
      {
        tool: "schedule",
        wrap: (inner: ToolRegistration): ToolRegistration => ({
          ...inner,
          async execute(args, api, context) {
            const result = await inner.execute(args, api, context);
            if (!cut) return result;
            cut = false;
            made.resolve();
            return new Promise((_resolve, reject) => {
              context.abortSignal?.addEventListener("abort", () => reject(new Error("cut off")));
            });
          },
        }),
      },
    ],
  });
  let agent = await fixture({
    clock,
    durable: true,
    builtIns: [crash],
    defaults: ["schedules", "crash"],
    answers: answers(),
  });
  const call = tool("schedule", { prompt: "Water the plants.", at: "2026-10-24T09:00:00Z" });
  await agent.runtime.turns.submit(agent.turn("turn-1", call));
  await made.promise;
  // It is back once the schedule's time has passed.
  agent = await agent.restart({ meanwhile: () => clock.advance(2 * 60 * 60_000) });
  fixtures.splice(0, 1, agent);
  const answered = await agent.outcome("turn-1");
  expect(answered.outcome === "answered" && answered.output).toMatch(
    /^Tool: Schedule \d+ first runs at 2026-10-24T09:00:00\.000Z\.$/u,
  );
  expect(await agent.outcome("opened-1")).toMatchObject({ output: "Ran: Water the plants." });
  await settle();
  expect(agent.openings()).toHaveLength(1);
});
