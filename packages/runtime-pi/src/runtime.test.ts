import {
  fauxAssistantMessage,
  fauxText,
  fauxToolCall,
  type FauxResponseFactory,
  type FauxResponseStep,
} from "@earendil-works/pi-ai";
import type { Subject } from "@carillon/protocol";
import { afterEach, expect, it } from "vitest";
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

const outcomes = (agent: Fixture, turnId: string) =>
  agent.core.received.filter((call) => call.path === `/v1/runtime/turns/${turnId}/outcome`);

/** The outcomes posted for the turn, once there are `count` of them. */
async function posted(agent: Fixture, turnId: string, count: number): Promise<unknown[]> {
  for (let attempt = 0; outcomes(agent, turnId).length < count && attempt < 300; attempt += 1)
    await new Promise((resolve) => setTimeout(resolve, 10));
  return outcomes(agent, turnId).map((call) => call.body);
}

it("keeps each turn once, however often core hands it over", async () => {
  const agent = await fixture({ answers: [fauxAssistantMessage("Hello there.")] });
  const turn = agent.turn("turn-1", "hello");
  await Promise.all([agent.runtime.turns.submit(turn), agent.runtime.turns.submit(turn)]);
  expect(await agent.outcome("turn-1")).toMatchObject({
    outcome: "answered",
    output: "Hello there.",
  });
  // A turn it finished, whose outcome core may not have recorded, is accepted again.
  await agent.runtime.turns.submit(turn);
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(outcomes(agent, "turn-1")).toHaveLength(1);
  expect(agent.model.state.callCount).toBe(1);
  expect(agent.core.received[0]).toMatchObject({
    provider: "runtime-pi",
    authorization: "Bearer test-only",
  });
});

it.each<[string, FauxResponseStep, object]>([
  [
    "a cut-off answer with a note",
    fauxAssistantMessage("Partial", { stopReason: "length" }),
    {
      outcome: "answered",
      output: "Partial\n\n[The answer was cut off at the model's output limit.]",
    },
  ],
  [
    "a provider's explanation",
    fauxAssistantMessage([], { stopReason: "error", errorMessage: "The account is suspended." }),
    { outcome: "failed", explanation: "The account is suspended." },
  ],
  [
    "a failure whose text is not fit to show",
    fauxAssistantMessage([], { stopReason: "error", errorMessage: "<html>denied</html>" }),
    { outcome: "failed" },
  ],
  [
    "an empty answer",
    fauxAssistantMessage(""),
    { outcome: "failed", explanation: "The model gave no answer." },
  ],
])("settles %s", async (_name, answer, expected) => {
  const agent = await fixture({ answers: [answer] });
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello"));
  expect(await agent.outcome("turn-1")).toMatchObject(expected);
});

it("fails a turn whose model is gone, saying how to choose another", async () => {
  const agent = await fixture();
  const provider = agent.model.provider.id;
  agent.models.deleteProvider(provider);
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello"));
  expect(await agent.outcome("turn-1")).toMatchObject({
    outcome: "failed",
    explanation: `${provider}/scripted is not available. Choose another model with /settings.`,
  });
});

const ranOn: FauxResponseFactory = (_context, options, _state, model) =>
  fauxAssistantMessage(`${model.id} thinking ${options?.reasoning ?? "off"}`);

const channel = (channel_id: string): Subject => ({
  kind: "channel",
  interface_id: "discord-private",
  channel_id,
  agent_id: "default",
});

it("runs every conversation of a channel on the channel's model once it changes, and no other channel", async () => {
  const agent = await fixture({ answers: Array.from({ length: 6 }, () => ranOn) });
  const ask = (turn: string, audience_id: string, channel_id: string) =>
    agent.ask(turn, "hello", { audience_id, channel_id });
  expect(await ask("turn-1", "first", "300")).toBe("scripted thinking off");
  expect(await ask("turn-2", "other", "301")).toBe("scripted thinking off");
  const reasoner = `${agent.model.provider.id}/reasoner`;
  await agent.set(channel("300"), "model", reasoner);
  const lines = await agent.set(channel("300"), "model.thinking", "high");
  expect(lines.map(({ id, value, inherited }) => ({ id, value, inherited }))).toEqual([
    { id: "model", value: { kind: "one", id: reasoner }, inherited: false },
    { id: "model.thinking", value: { kind: "one", id: "high" }, inherited: false },
  ]);
  expect(await ask("turn-3", "first", "300")).toBe("reasoner thinking high");
  expect(await ask("turn-4", "second", "300")).toBe("reasoner thinking high");
  expect(await ask("turn-5", "other", "301")).toBe("scripted thinking off");
  const { settings } = await agent.runtime.settings({ subject: channel("301") });
  expect(settings).toMatchObject([{ id: "model", inherited: true }]);
});

it("fails the turns of an agent with no model, naming the command that sets one", async () => {
  const agent = await fixture({ agentModel: null, answers: [ranOn] });
  const { settings } = await agent.runtime.settings({
    subject: { kind: "agent", agent_id: "default" },
  });
  expect(settings).toMatchObject([{ id: "model", value: null }]);
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello"));
  const failed = await agent.outcome("turn-1");
  expect(failed.outcome === "failed" && failed.explanation).toContain(
    "`carillon settings --agent default model=<provider>/<model>`",
  );
  await agent.set(
    { kind: "agent", agent_id: "default" },
    "model",
    `${agent.model.provider.id}/scripted`,
  );
  expect(await agent.ask("turn-2", "hello")).toBe("scripted thinking off");
});

it("refuses the settings of an agent it does not serve", async () => {
  const agent = await fixture();
  await expect(
    agent.runtime.settings({ subject: { kind: "agent", agent_id: "elsewhere" } }),
  ).rejects.toMatchObject({ status: 422 });
});

it("settles messages steered into one run with its answer, carried by the newest", async () => {
  const pause = pauseTool();
  const agent = await fixture({
    builtIns: [pause.extension],
    defaults: ["pause"],
    answers: [
      fauxAssistantMessage(fauxToolCall("pause", {}), { stopReason: "toolUse" }),
      (context) => {
        const users = context.messages.filter((message) => message.role === "user");
        return fauxAssistantMessage(fauxText(`Both: ${String(users.length)}`));
      },
    ],
  });
  await agent.runtime.turns.submit(agent.turn("turn-1", "first"));
  await pause.started;
  await agent.runtime.turns.submit(agent.turn("turn-2", "also this"));
  pause.release();
  expect(await agent.outcome("turn-2")).toMatchObject({ outcome: "answered", output: "Both: 2" });
  expect(await agent.outcome("turn-1")).toEqual({ outcome: "steered" });
});

it("posts an outcome again until core records it", async () => {
  const agent = await fixture({ answers: [fauxAssistantMessage("Done.")] });
  agent.core.answer(503);
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello"));
  const [first, second] = await posted(agent, "turn-1", 2);
  expect(first).toMatchObject({ outcome: "answered", output: "Done." });
  expect(second).toEqual(first);
});

it("replaces an outcome core refuses for good with a plain failure, once", async () => {
  const agent = await fixture({
    answers: [fauxAssistantMessage("Too long."), fauxAssistantMessage("Also refused.")],
  });
  agent.core.answer(413);
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello"));
  const [refused, replacement] = await posted(agent, "turn-1", 2);
  expect(refused).toMatchObject({ outcome: "answered", output: "Too long." });
  expect(replacement).toEqual({
    outcome: "failed",
    explanation: "Carillon could not deliver this answer.",
  });
  // Refused again, the turn is given up and no longer held, not posted forever.
  agent.core.answer(422, 422);
  await agent.runtime.turns.submit(agent.turn("turn-2", "hello"));
  await posted(agent, "turn-2", 2);
  for (
    let attempt = 0;
    agent.runtime.activity("turn-2", "discord-private") && attempt < 300;
    attempt += 1
  )
    await new Promise((resolve) => setTimeout(resolve, 10));
  expect(agent.runtime.activity("turn-2", "discord-private")).toBeUndefined();
  expect(outcomes(agent, "turn-2")).toHaveLength(2);
});

it("resumes an answer cut off by a restart and settles it once", async () => {
  const never = new Promise<void>(() => undefined);
  let agent = await fixture({
    durable: true,
    answers: [heldAnswer("lost", never), fauxAssistantMessage("Resumed.")],
  });
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello"));
  for (let attempt = 0; agent.model.state.callCount === 0 && attempt < 100; attempt += 1)
    await new Promise((resolve) => setTimeout(resolve, 10));
  agent = await agent.restart();
  fixtures.splice(0, 1, agent);
  expect(await agent.outcome("turn-1")).toMatchObject({ outcome: "answered", output: "Resumed." });
  // Core handing the turn over again after the restart changes nothing.
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello"));
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(outcomes(agent, "turn-1")).toHaveLength(1);
  expect(agent.model.state.callCount).toBe(2);
});

it("refuses a turn of an agent it does not serve", async () => {
  const agent = await fixture();
  await expect(
    agent.runtime.turns.submit(agent.turn("turn-1", "hello", { agent_id: "elsewhere" })),
  ).rejects.toMatchObject({ status: 422 });
});
