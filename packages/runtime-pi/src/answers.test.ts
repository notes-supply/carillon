import { BACKGROUND_CONTEXT as context } from "@earendil-works/chord/context";
import {
  createModels,
  fauxAssistantMessage,
  fauxProvider,
  fauxToolCall,
  type FauxResponseFactory,
} from "@earendil-works/pi-ai";
import {
  createRegistry,
  defineExtension,
  Harness,
  MemoryStorage,
  type SubmissionId,
} from "@earendil-works/pi-durable";
import type { TurnOutcome } from "@carillon/protocol";
import { chmod, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, it } from "vitest";
import { ATTACH } from "./answers.ts";
import { deliveryTask, type DeliveryInput } from "./delivery.ts";
import { heldAnswer, pauseTool, startFixture, textsOf, type Fixture } from "./fixture.ts";

const fixtures: Fixture[] = [];
afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.close()));
});

async function fixture(...args: Parameters<typeof startFixture>) {
  const started = await startFixture(...args);
  fixtures.push(started);
  return started;
}

/** A model call that attaches each path in one tool round. */
function attaching(...paths: string[]) {
  return fauxAssistantMessage(
    paths.map((path) => fauxToolCall("attach", { path })),
    { stopReason: "toolUse" },
  );
}

const reporting: FauxResponseFactory = (context) => {
  const results = context.messages.filter((message) => message.role === "toolResult");
  return fauxAssistantMessage(`Tools: ${results.flatMap(textsOf).join(" | ")}`);
};

it("sends the files the agent attached with its answer, in order and once each", async () => {
  const agent = await fixture({
    answers: [
      attaching("out/chart.png", "data set.csv", "out/chart.png"),
      fauxAssistantMessage("Here is the chart."),
    ],
  });
  await mkdir(join(agent.workspace, "out"));
  await writeFile(join(agent.workspace, "out/chart.png"), "chart");
  await writeFile(join(agent.workspace, "data set.csv"), "a,b\n");
  await agent.runtime.turns.submit(agent.turn("turn-1", "Chart it."));
  expect(await agent.outcome("turn-1")).toMatchObject({
    outcome: "answered",
    output: "Here is the chart.",
    files: [join(agent.workspace, "out/chart.png"), join(agent.workspace, "data set.csv")],
  });
  agent.model.setResponses([fauxAssistantMessage("Nothing else.")]);
  await agent.runtime.turns.submit(agent.turn("turn-2", "Anything else?"));
  expect(await agent.outcome("turn-2")).not.toHaveProperty("files");
});

it("tells the model why a path cannot be attached, and the answer carries none of them", async () => {
  const agent = await fixture({
    answers: [attaching("missing.png", "folder", "private.txt"), reporting],
  });
  await mkdir(join(agent.workspace, "folder"));
  const unreadable = join(agent.workspace, "private.txt");
  await writeFile(unreadable, "secret");
  await chmod(unreadable, 0o000);
  await agent.runtime.turns.submit(agent.turn("turn-1", "Send them."));
  const outcome = await agent.outcome("turn-1");
  expect(outcome).not.toHaveProperty("files");
  const said = outcome.outcome === "answered" ? outcome.output : "";
  const refusals = [
    `There is no file at ${join(agent.workspace, "missing.png")}.`,
    `${join(agent.workspace, "folder")} is not a regular file.`,
    // Root reads any file; an ordinary owner cannot read one with no permissions.
    ...(process.getuid?.() === 0 ? [] : [`${unreadable} cannot be read: permission denied.`]),
  ];
  for (const refusal of refusals) expect(said).toContain(refusal);
});

it("keeps the files attached before a restart for the answer that follows it", async () => {
  const never = new Promise<void>(() => undefined);
  let agent = await fixture({
    durable: true,
    answers: [
      attaching("report.pdf"),
      heldAnswer("lost", never),
      fauxAssistantMessage("Here is the report."),
    ],
  });
  await writeFile(join(agent.workspace, "report.pdf"), "%PDF");
  await agent.runtime.turns.submit(agent.turn("turn-1", "Report."));
  for (let attempt = 0; agent.model.state.callCount < 2 && attempt < 200; attempt += 1)
    await new Promise((resolve) => setTimeout(resolve, 10));
  agent = await agent.restart();
  fixtures.splice(0, 1, agent);
  expect(await agent.outcome("turn-1")).toMatchObject({
    outcome: "answered",
    output: "Here is the report.",
    files: [join(agent.workspace, "report.pdf")],
  });
});

const never = new Promise<void>(() => undefined);

/** Waits until the scripted model has answered `count` requests. */
async function called(agent: Fixture, count: number): Promise<void> {
  for (let attempt = 0; agent.model.state.callCount < count && attempt < 200; attempt += 1)
    await new Promise((resolve) => setTimeout(resolve, 10));
  expect(agent.model.state.callCount).toBe(count);
}

it("drops the files of a stopped run, so a later answer does not carry them", async () => {
  const agent = await fixture({
    answers: [attaching("old-report.txt"), heldAnswer("never", never), fauxAssistantMessage("42.")],
  });
  await writeFile(join(agent.workspace, "old-report.txt"), "report");
  await agent.runtime.turns.submit(agent.turn("turn-1", "Prepare the report."));
  await called(agent, 2);
  await agent.runtime.turns.control("turn-1", "discord-private", "stop");
  expect(await agent.outcome("turn-1")).toEqual({ outcome: "stopped" });
  await agent.runtime.turns.submit(agent.turn("turn-2", "Something unrelated?"));
  expect(await agent.outcome("turn-2")).toMatchObject({ outcome: "answered", output: "42." });
  expect(await agent.outcome("turn-2")).not.toHaveProperty("files");
});

it("drops the files of a run a new message interrupts", async () => {
  const agent = await fixture({
    answers: [attaching("old-report.txt"), heldAnswer("never", never), fauxAssistantMessage("42.")],
  });
  const resources = await agent.snapshot({
    "settings.json": JSON.stringify({ busy: "interrupt" }),
  });
  const turn = (id: string, input: string) => agent.turn(id, input, { resource_root: resources });
  await writeFile(join(agent.workspace, "old-report.txt"), "report");
  await agent.runtime.turns.submit(turn("turn-1", "Prepare the report."));
  await called(agent, 2);
  await agent.runtime.turns.submit(turn("turn-2", "Something unrelated?"));
  expect(await agent.outcome("turn-1")).toEqual({ outcome: "stopped" });
  expect(await agent.outcome("turn-2")).toMatchObject({ outcome: "answered", output: "42." });
  expect(await agent.outcome("turn-2")).not.toHaveProperty("files");
});

it("drops the files of a run that fails", async () => {
  const agent = await fixture({
    answers: [
      attaching("old-report.txt"),
      fauxAssistantMessage([], { stopReason: "error", errorMessage: "The account is suspended." }),
      fauxAssistantMessage("42."),
    ],
  });
  await writeFile(join(agent.workspace, "old-report.txt"), "report");
  await agent.runtime.turns.submit(agent.turn("turn-1", "Prepare the report."));
  expect(await agent.outcome("turn-1")).toMatchObject({ outcome: "failed" });
  await agent.runtime.turns.submit(agent.turn("turn-2", "Something unrelated?"));
  expect(await agent.outcome("turn-2")).toMatchObject({ outcome: "answered", output: "42." });
  expect(await agent.outcome("turn-2")).not.toHaveProperty("files");
});

it("sends the files attached before a steer with the answer that carries it", async () => {
  const pause = pauseTool();
  const agent = await fixture({
    builtIns: [pause.extension],
    defaults: ["pause"],
    answers: [
      fauxAssistantMessage(
        [fauxToolCall("attach", { path: "report.txt" }), fauxToolCall("pause", {})],
        { stopReason: "toolUse" },
      ),
      fauxAssistantMessage("Joined answer."),
    ],
  });
  await writeFile(join(agent.workspace, "report.txt"), "report");
  await agent.runtime.turns.submit(agent.turn("turn-1", "Prepare the report."));
  await pause.started;
  await agent.runtime.turns.submit(agent.turn("turn-2", "Add a summary."));
  pause.release();
  expect(await agent.outcome("turn-1")).toEqual({ outcome: "steered" });
  expect(await agent.outcome("turn-2")).toMatchObject({
    outcome: "answered",
    files: [join(agent.workspace, "report.txt")],
  });
});

it("gives each answer the files of its own run, whichever answer's delivery decides first", async () => {
  const directory = await mkdtemp(join(tmpdir(), "carillon-attached-"));
  const [first, second] = [join(directory, "first.txt"), join(directory, "second.txt")];
  await writeFile(first, "first");
  await writeFile(second, "second");
  const model = fauxProvider();
  model.setResponses([
    attaching(first),
    fauxAssistantMessage("First."),
    attaching(second),
    fauxAssistantMessage("Second."),
  ]);
  const models = createModels();
  models.setProvider(model.provider);
  const registry = createRegistry();
  const harness = await Harness.open(new MemoryStorage(), { models, registry }, context);
  const decided = new Map<string, TurnOutcome>();
  const earlier = Promise.withResolvers<void>();
  let held: SubmissionId | undefined;
  const delivery = deliveryTask({
    async wait(submission, waiting) {
      if (submission === held) await earlier.promise;
      const handle = await harness.submission(submission, waiting);
      if (handle === undefined) throw new Error(`no submission ${String(submission)}`);
      return handle.wait(waiting);
    },
    status: async (submission, reading) =>
      (await harness.submission(submission, reading))?.status(reading),
    report: () => undefined,
    selects: () => true,
    settle(turn, outcome) {
      decided.set(turn, outcome);
      return Promise.resolve("recorded");
    },
    released: () => undefined,
  });
  registry.install(defineExtension({ name: "answers", tools: [ATTACH], tasks: [delivery] }));
  try {
    const root = await harness.root(context, {
      agent: { model: { provider: model.provider.id, modelId: model.getModel().id } },
    });
    const inputs: DeliveryInput[] = [];
    for (const turn of ["turn-1", "turn-2"]) {
      const submission = await root.submit({ type: "input", content: turn }, context);
      await submission.wait(context);
      held ??= submission.id;
      inputs.push({
        turn,
        interface_id: "discord-private",
        conversation: root.id,
        submission: submission.id,
      });
    }
    await harness.commit(async (tx) => {
      for (const input of inputs)
        await tx.createTask(delivery, input, {
          ownership: { kind: "conversation" },
          conversationId: root.id,
          background: true,
        });
    }, context);
    for (let attempt = 0; !decided.has("turn-2") && attempt < 200; attempt += 1)
      await new Promise((resolve) => setTimeout(resolve, 10));
    expect([...decided.keys()]).toEqual(["turn-2"]);
    earlier.resolve();
    for (let attempt = 0; !decided.has("turn-1") && attempt < 200; attempt += 1)
      await new Promise((resolve) => setTimeout(resolve, 10));
    expect(decided.get("turn-2")).toMatchObject({ output: "Second.", files: [second] });
    expect(decided.get("turn-1")).toMatchObject({ output: "First.", files: [first] });
  } finally {
    await harness.close(context);
    await rm(directory, { recursive: true, force: true });
  }
});
