import {
  fauxAssistantMessage,
  fauxText,
  fauxToolCall,
  type FauxResponseStep,
} from "@earendil-works/pi-ai";
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
  agent.models.deleteProvider(agent.model.provider.id);
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello"));
  expect(await agent.outcome("turn-1")).toMatchObject({
    outcome: "failed",
    explanation: "No model is available. Choose one with /model.",
  });
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
