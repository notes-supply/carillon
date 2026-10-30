import { BACKGROUND_CONTEXT as context } from "@earendil-works/chord/context";
import {
  createModels,
  fauxAssistantMessage,
  fauxProvider,
  fauxText,
  fauxToolCall,
  type FauxResponseStep,
  type Message,
} from "@earendil-works/pi-ai";
import {
  createRegistry,
  defineExtension,
  GenerationTask,
  Harness,
  hook,
  InboxDoc,
  MemoryStorage,
  type Extension,
} from "@earendil-works/pi-durable";
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

async function eventually<T>(read: () => T | undefined): Promise<T> {
  for (let attempt = 0; attempt < 300; attempt += 1) {
    const value = read();
    if (value !== undefined) return value;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error("the expected state never came");
}

const never = new Promise<void>(() => undefined);
const texts = (messages: readonly Message[]) =>
  messages.flatMap((message) =>
    message.role !== "user"
      ? []
      : typeof message.content === "string"
        ? [message.content]
        : message.content.flatMap((part) => (part.type === "text" ? [part.text] : [])),
  );

/** An agent whose first turn waits in a `pause` tool call, with the busy mode settings give. */
async function busy(
  mode: string,
  answers: FauxResponseStep[] = [],
  { extensions = [], stubborn = false }: { extensions?: Extension[]; stubborn?: boolean } = {},
) {
  const pause = pauseTool({ stubborn });
  const agent = await fixture({
    builtIns: [pause.extension, ...extensions],
    defaults: ["pause", ...extensions.map((extension) => extension.name)],
    answers: [
      fauxAssistantMessage(fauxToolCall("pause", {}), { stopReason: "toolUse" }),
      ...answers,
    ],
  });
  const resources = await agent.snapshot({ "settings.json": JSON.stringify({ busy: mode }) });
  const turn = (id: string, input: string) => agent.turn(id, input, { resource_root: resources });
  await agent.runtime.turns.submit(turn("turn-1", "first"));
  await pause.started;
  return { agent, pause, turn };
}

/**
 * A generation hook that runs the action it is given next, once: after a
 * tool round, before its boundary places waiting messages, or as an answer
 * is taken, before the commit that ends its run.
 */
function atBoundary(point: "afterTools" | "onYield") {
  let action: (() => void) | undefined;
  const run = (): undefined => {
    const next = action;
    action = undefined;
    next?.();
    return undefined;
  };
  return {
    extension: defineExtension({
      name: "boundary",
      hooks: [
        hook(GenerationTask, point === "afterTools" ? { afterTools: run } : { onYield: run }),
      ],
    }),
    next(next: () => void) {
      action = next;
    },
  };
}

it("keeps a waiting message in pi-durable's inbox as an item a commit can make a steer", async () => {
  const model = fauxProvider();
  const models = createModels();
  models.setProvider(model.provider);
  const harness = await Harness.open(
    new MemoryStorage(),
    { models, registry: createRegistry(), settings: { followUpMode: "one-at-a-time" } },
    context,
  );
  try {
    const gate = Promise.withResolvers<void>();
    model.setResponses([heldAnswer("first", gate.promise), fauxAssistantMessage("second")]);
    const root = await harness.root(context, {
      agent: { model: { provider: model.provider.id, modelId: model.getModel().id } },
    });
    await root.submit({ type: "input", content: "one" }, context);
    const waiting = await root.submit(
      {
        type: "input",
        content: [{ type: "text", text: "two" }],
        requestId: "turn-2",
        whenBusy: "followUp",
        entry: { kind: "carillon.message", data: { turn: "turn-2" } },
      },
      context,
    );
    expect(await harness.snapshot(InboxDoc, root.id, context)).toEqual({
      items: [
        {
          id: waiting.id,
          mode: "followUp",
          content: [{ type: "text", text: "two" }],
          entry: { kind: "carillon.message", data: { turn: "turn-2" } },
        },
      ],
    });
    await root.commit(async (tx) => {
      const [item] = (await tx.doc(InboxDoc, root.id)).items;
      if (item !== undefined) item.mode = "steer";
    }, context);
    expect((await harness.snapshot(InboxDoc, root.id, context))?.items[0]?.mode).toBe("steer");
    gate.resolve();
    expect(await (await harness.submission(waiting.id, context))?.wait(context)).toMatchObject({
      status: "done",
    });
  } finally {
    await harness.close(context);
  }
});

it("queues a message behind a running answer, offering its controls", async () => {
  const { agent, pause, turn } = await busy("queue", [
    fauxAssistantMessage("First answer."),
    fauxAssistantMessage("Second answer."),
  ]);
  await agent.runtime.turns.submit(turn("turn-2", "second"));
  expect(await eventually(() => agent.runtime.activity("turn-2", "discord-private"))).toMatchObject(
    { phase: "queued", controls: ["steer", "interrupt", "stop"] },
  );
  expect(agent.runtime.activity("turn-2", "elsewhere")).toBeUndefined();
  expect(agent.runtime.activity("turn-1", "discord-private")).toMatchObject({
    phase: "tools",
    running: [{ tool: "pause" }],
  });
  expect(agent.runtime.restartReadiness()).toEqual({
    state: "unready",
    reason: "a tool call is running",
  });
  pause.release();
  expect(await agent.outcome("turn-1")).toMatchObject({ output: "First answer." });
  expect(await agent.outcome("turn-2")).toMatchObject({ output: "Second answer." });
  expect(agent.runtime.restartReadiness()).toEqual({ state: "ready" });
});

it("steers a waiting message into the running answer", async () => {
  const { agent, pause, turn } = await busy("queue", [fauxAssistantMessage("Joined answer.")]);
  await agent.runtime.turns.submit(turn("turn-2", "second"));
  await eventually(() => agent.runtime.activity("turn-2", "discord-private"));
  await agent.runtime.turns.control?.("turn-2", "discord-private", "steer");
  pause.release();
  expect(await agent.outcome("turn-2")).toMatchObject({ output: "Joined answer." });
  expect(await agent.outcome("turn-1")).toEqual({ outcome: "steered" });
  // Only a waiting message can be steered.
  await expect(
    agent.runtime.turns.control?.("turn-1", "discord-private", "steer"),
  ).rejects.toMatchObject({ status: 409 });
});

it("interrupts the running answer for a waiting message", async () => {
  let seen: readonly Message[] = [];
  const { agent, turn } = await busy("queue", [
    (request) => {
      seen = request.messages;
      return fauxAssistantMessage(fauxText("After the interruption."));
    },
  ]);
  await agent.runtime.turns.submit(turn("turn-2", "second"));
  await eventually(() => agent.runtime.activity("turn-2", "discord-private"));
  await agent.runtime.turns.control?.("turn-2", "discord-private", "interrupt");
  expect(await agent.outcome("turn-1")).toEqual({ outcome: "stopped" });
  expect(await agent.outcome("turn-2")).toMatchObject({ output: "After the interruption." });
  expect(texts(seen)).toEqual([
    "first",
    "[Carillon] The person interrupted your previous answer.",
    "second",
  ]);
});

it("stops a waiting message, and then the running one with what waits behind it", async () => {
  const { agent, turn } = await busy("queue", [fauxAssistantMessage("Third answer.")]);
  await agent.runtime.turns.submit(turn("turn-2", "second"));
  await agent.runtime.turns.submit(turn("turn-3", "third"));
  await eventually(() => agent.runtime.activity("turn-3", "discord-private"));
  // A message is controlled only from the interface it came from.
  await expect(agent.runtime.turns.control?.("turn-3", "elsewhere", "stop")).rejects.toMatchObject({
    status: 404,
  });
  await agent.runtime.turns.control?.("turn-2", "discord-private", "stop");
  expect(await agent.outcome("turn-2")).toEqual({ outcome: "stopped" });
  await agent.runtime.turns.control?.("turn-1", "discord-private", "stop");
  expect(await agent.outcome("turn-1")).toEqual({ outcome: "stopped" });
  expect(await agent.outcome("turn-3")).toMatchObject({ output: "Third answer." });
  await expect(
    agent.runtime.turns.control?.("turn-1", "discord-private", "stop"),
  ).rejects.toMatchObject({ status: 409 });

  await expect(
    agent.runtime.turns.control?.("unknown", "discord-private", "stop"),
  ).rejects.toMatchObject({ status: 404 });
});

it("in interrupt mode, ends the running answer for a new message, saying so", async () => {
  let seen: readonly Message[] = [];
  const pause = pauseTool();
  const agent = await fixture({
    builtIns: [pause.extension],
    defaults: ["pause"],
    answers: [
      heldAnswer("never", never),
      (request) => {
        seen = request.messages;
        return fauxAssistantMessage(fauxText("Answering the new message."));
      },
    ],
  });
  const resources = await agent.snapshot({
    "settings.json": JSON.stringify({ busy: "interrupt" }),
  });
  await agent.runtime.turns.submit(agent.turn("turn-1", "first", { resource_root: resources }));
  await eventually(() => (agent.model.state.callCount > 0 ? true : undefined));
  await agent.runtime.turns.submit(agent.turn("turn-2", "second", { resource_root: resources }));
  expect(await agent.outcome("turn-1")).toEqual({ outcome: "stopped" });
  expect(await agent.outcome("turn-2")).toMatchObject({ output: "Answering the new message." });
  expect(texts(seen)).toEqual([
    "first",
    "[Carillon] This message interrupted your previous answer.",
    "second",
  ]);
});

it("offers a waiting steer no steer control", async () => {
  const { agent, pause, turn } = await busy("steer", [fauxAssistantMessage("Joined answer.")]);
  await agent.runtime.turns.submit(turn("turn-2", "second"));
  expect(await eventually(() => agent.runtime.activity("turn-2", "discord-private"))).toMatchObject(
    { phase: "queued", controls: ["interrupt", "stop"] },
  );
  pause.release();
  expect(await agent.outcome("turn-2")).toMatchObject({ output: "Joined answer." });
});

it("stops no later message when the stopped one is answered before its run ends", async () => {
  const boundary = atBoundary("onYield");
  const { agent, pause, turn } = await busy(
    "queue",
    [fauxAssistantMessage("First answer."), fauxAssistantMessage("Second answer.")],
    { extensions: [boundary.extension] },
  );
  let stopping: Promise<unknown> | undefined;
  boundary.next(() => {
    stopping = agent.runtime.turns
      .control?.("turn-1", "discord-private", "stop")
      .catch((failure: unknown) => failure);
  });
  await agent.runtime.turns.submit(turn("turn-2", "second"));
  pause.release();
  expect(await agent.outcome("turn-2")).toMatchObject({ output: "Second answer." });
  await stopping;
});

it.each([
  // No boundary places a follow-up, so the interrupt ends the run.
  ["queue", "stopped"],
  // The boundary places the steer into the run meanwhile, so the run answers it.
  ["steer", "steered"],
] as const)(
  "in %s mode, an interrupt at a tool boundary leaves the running message %s and answers the waiting one",
  async (mode, running) => {
    const boundary = atBoundary("afterTools");
    const { agent, pause, turn } = await busy(mode, [fauxAssistantMessage("Answering both.")], {
      extensions: [boundary.extension],
    });
    let interrupting: Promise<unknown> | undefined;
    boundary.next(() => {
      interrupting = agent.runtime.turns
        .control?.("turn-2", "discord-private", "interrupt")
        .catch((failure: unknown) => failure);
    });
    await agent.runtime.turns.submit(turn("turn-2", "second"));
    await eventually(() => agent.runtime.activity("turn-2", "discord-private"));
    pause.release();
    expect(await agent.outcome("turn-2")).toMatchObject({ output: "Answering both." });
    await expect(interrupting).resolves.toBeUndefined();
    expect(await agent.outcome("turn-1")).toEqual({ outcome: running });
  },
);

it("sends one note when the person interrupts for a message already interrupting", async () => {
  const seen: (readonly Message[])[] = [];
  const answer =
    (text: string): FauxResponseStep =>
    (request) => {
      seen.push(request.messages);
      return fauxAssistantMessage(text);
    };
  const { agent, pause, turn } = await busy(
    "interrupt",
    [answer("Answering the new message."), answer("Answering the third.")],
    { stubborn: true },
  );
  await agent.runtime.turns.submit(turn("turn-2", "second"));
  // The stubborn tool holds the abort the handoff started while the person interrupts.
  const interrupting = agent.runtime.turns
    .control?.("turn-2", "discord-private", "interrupt")
    .catch((failure: unknown) => failure);
  pause.release();
  expect(await interrupting).toMatchObject({ status: 409 });
  expect(await agent.outcome("turn-1")).toEqual({ outcome: "stopped" });
  expect(await agent.outcome("turn-2")).toMatchObject({ output: "Answering the new message." });
  await agent.runtime.turns.submit(turn("turn-3", "third"));
  expect(await agent.outcome("turn-3")).toMatchObject({ output: "Answering the third." });
  expect(texts(seen.at(-1) ?? [])).toEqual([
    "first",
    "[Carillon] This message interrupted your previous answer.",
    "second",
    "third",
  ]);
});

it("hands a message over at once in interrupt mode, ending the answer it interrupts later", async () => {
  const { agent, pause, turn } = await busy(
    "interrupt",
    [fauxAssistantMessage("Answering the new message.")],
    { stubborn: true },
  );
  // The stubborn tool holds the abort, so each handoff returns while it waits.
  await Promise.all([
    agent.runtime.turns.submit(turn("turn-2", "second")),
    agent.runtime.turns.submit(turn("turn-2", "second")),
  ]);
  await agent.runtime.turns.submit(turn("turn-2", "second"));
  pause.release();
  expect(await agent.outcome("turn-1")).toEqual({ outcome: "stopped" });
  expect(await agent.outcome("turn-2")).toMatchObject({ output: "Answering the new message." });
});
