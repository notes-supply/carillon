import type { Subject } from "@carillon/protocol";
import { noModel } from "@carillon/runtime/settings";
import { expect, it } from "vitest";
import { derivedId } from "../contracts.ts";
import { connectT3 } from "../t3.ts";
import {
  answer,
  build,
  conversation,
  core,
  execute,
  fake,
  readBack,
  runOf,
  runtime,
  started,
  thread,
  turn,
} from "./harness.ts";

const home = {
  kind: "channel",
  interface_id: "discord-private",
  channel_id: "300",
  agent_id: "default",
} as const satisfies Subject;
const sonnet = {
  instanceId: "claudeAgent",
  model: "claude-sonnet-5",
  options: [
    { id: "effort", value: "low" },
    { id: "fastMode", value: false },
  ],
};
const haiku = { instanceId: "claudeAgent", model: "claude-haiku-4-5", options: [] };
const modelSets = () =>
  fake.received.filter((command) => command.type === "thread.model-selection.set");

/** Chooses `id` on the subject's `setting`, or resets it, as `carillon settings` does through core. */
async function change(subject: Subject, setting: string, id: string | null) {
  const { settings } = await runtime.settings({ subject });
  const line = settings.find((candidate) => candidate.id === setting);
  if (line === undefined) throw new Error(`no ${setting} line`);
  return runtime.settings({
    subject,
    change: { setting, value: id === null ? null : { kind: "one", id }, expected: line.revision },
  });
}

/** Runs one turn to its answer and gives the selection T3 ran it on. */
async function ran(correlation: string, extra: Record<string, unknown> = {}) {
  const reply = execute(turn(correlation, "Which model?", extra));
  const messageId = derivedId("message", correlation);
  const native = () =>
    [...fake.threads.values()].find((candidate) =>
      candidate.runs.some((run) => run.userMessageId === messageId),
    );
  await fake.waitFor(() => native() !== undefined);
  const found = native();
  const run = found?.runs.find((candidate) => candidate.userMessageId === messageId);
  if (found === undefined || run === undefined) throw new Error(`${correlation} did not run`);
  fake.assistant(found.id, run.id, "Hello.");
  fake.complete(found.id, run.id);
  await reply;
  return run.modelSelection;
}

it("shows and changes a channel's model lines over T3's catalogue, writing nothing to T3", async () => {
  const { settings } = await runtime.settings({ subject: home });
  expect(settings).toMatchObject([
    {
      id: "model",
      name: "Model",
      choices: [
        { id: "claudeAgent/claude-sonnet-5", name: "Claude Sonnet 5", group: "claudeAgent" },
        { id: "claudeAgent/claude-haiku-4-5", name: "Claude Haiku 4.5", group: "claudeAgent" },
        { id: "codex/gpt-6-astra", name: "GPT-6 Astra", group: "codex" },
      ],
      value: { kind: "one", id: "claudeAgent/claude-sonnet-5" },
      inherited: true,
    },
    {
      id: "model.effort",
      name: "Effort",
      choices: [
        { id: "low", name: "Low" },
        { id: "high", name: "High" },
      ],
      value: { kind: "one", id: "low" },
      inherited: true,
    },
    {
      id: "model.fastMode",
      name: "Fast Mode",
      choices: [
        { id: "off", name: "Off" },
        { id: "on", name: "On" },
      ],
      value: { kind: "one", id: "off" },
      inherited: true,
    },
  ]);
  const changed = await change(home, "model", "claudeAgent/claude-haiku-4-5");
  expect(changed.settings).toMatchObject([
    { id: "model", value: { kind: "one", id: "claudeAgent/claude-haiku-4-5" }, inherited: false },
  ]);
  expect(fake.received).toEqual([]);
  await expect(
    runtime.settings({ subject: { ...home, agent_id: "elsewhere" } }),
  ).rejects.toMatchObject({ status: 422 });
});

it("replays a dropped first dispatch with the same command ID on the selection it resolved", async () => {
  let dropped = false;
  fake.dropRequest = (command) => {
    if (command.type !== "message.dispatch" || dropped) return false;
    dropped = true;
    return true;
  };
  let reconnect = () => {};
  const changed = new Promise<void>((resolve) => (reconnect = resolve));
  const held = await readBack(() =>
    build({
      connect: async (endpoint, options) => {
        if (dropped) await changed;
        return connectT3(endpoint, options);
      },
    }),
  );
  try {
    const reply = execute(turn("turn-1", "hi"), undefined, held);
    await fake.waitFor(() => dropped);
    // The channel moves to another model before the turn reaches T3 again.
    await change(home, "model", "claudeAgent/claude-haiku-4-5");
    reconnect();
    answer(await started("turn-1"), "Hello.");
    await reply;
  } finally {
    await held.close();
  }
  const sent = fake.received.filter((command) => command.type === "message.dispatch");
  expect(sent).toHaveLength(2);
  expect(sent[1]?.commandId).toBe(sent[0]?.commandId);
  expect(sent.map((command) => command["modelSelection"])).toEqual([sonnet, sonnet]);
  expect(runOf("turn-1")?.modelSelection).toEqual(sonnet);
  expect(await ran("turn-2")).toEqual(haiku);
});

it("runs the channel's model on the next turn of every thread in it, existing threads included", async () => {
  const elsewhere = { channel_id: "301", audience_id: "conversation-3" };
  expect(await ran("turn-1")).toEqual(sonnet);
  expect(await ran("turn-2", { audience_id: "conversation-2" })).toEqual(sonnet);
  expect(await ran("turn-3", elsewhere)).toEqual(sonnet);

  await change(home, "model", "claudeAgent/claude-haiku-4-5");
  // A model picked in T3's own UI lasts until Carillon's next turn on the thread.
  thread().modelSelection = { instanceId: "codex", model: "gpt-6-astra", options: [] };
  expect(await ran("turn-4")).toEqual(haiku);
  expect(thread().modelSelection).toEqual(haiku);
  expect(await ran("turn-5", { audience_id: "conversation-2" })).toEqual(haiku);
  expect(await ran("turn-6", elsewhere)).toEqual(sonnet);
  expect(modelSets()).toEqual([]);
});

it("sends a speed setting the channel chose with its message, a boolean as T3 takes it", async () => {
  await change(home, "model.fastMode", "on");
  expect(await ran("turn-1")).toEqual({
    ...sonnet,
    options: [
      { id: "effort", value: "low" },
      { id: "fastMode", value: true },
    ],
  });
  expect(modelSets()).toEqual([]);
});

it("fails a turn with no model of its channel or agent in the runtime's own words, before T3 opens anything", async () => {
  const { settings } = await change({ kind: "agent", agent_id: "default" }, "model", null);
  expect(settings).toMatchObject([{ id: "model", value: null }]);
  const request = turn("turn-1", "hi");
  core.hold(request);
  const outcome = core.outcome(request.correlation_id);
  await runtime.turns.submit(request);
  expect(await outcome).toMatchObject({ outcome: "failed", explanation: noModel("default") });
  expect(noModel("default")).toContain("carillon settings --agent default model=");
  expect(fake.received).toEqual([]);
  expect(await runtime.commands(conversation)).toEqual({ commands: [] });
  expect(fake.refreshes).toEqual([]);
});
