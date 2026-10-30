import {
  fauxAssistantMessage,
  fauxToolCall,
  Type,
  type FauxResponseStep,
  type Message,
  type SystemMessage,
} from "@earendil-works/pi-ai";
import { defineExtension, defineTask, defineTool } from "@earendil-works/pi-durable";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { afterEach, expect, it } from "vitest";
import { heldAnswer, startFixture, type Fixture } from "./fixture.ts";

const fixtures: Fixture[] = [];
afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.close()));
});

async function fixture(...args: Parameters<typeof startFixture>) {
  const started = await startFixture(...args);
  fixtures.push(started);
  return started;
}

/** Answers each request, keeping what it sent. */
function recording(count: number) {
  const requests: (readonly Message[])[] = [];
  const answers: FauxResponseStep[] = Array.from({ length: count }, () => (context) => {
    requests.push(context.messages);
    return fauxAssistantMessage(`Answer ${String(requests.length)}.`);
  });
  return { requests, answers };
}

const systems = (messages: readonly Message[]) =>
  messages.filter((message): message is SystemMessage => message.role === "system");

/** The prompt sections in effect after a request's system messages. */
function sections(messages: readonly Message[]): Record<string, string> {
  const current: Record<string, string> = {};
  for (const message of systems(messages))
    for (const [key, value] of Object.entries(message.sections ?? {}))
      if (value === null) delete current[key];
      else current[key] = value;
  return current;
}

/** A snapshot selecting one user extension, whose default export is `body`. */
const userExtension = (body: string) => ({
  "extensions/user.js": `export default ${body};`,
  "settings.json": JSON.stringify({ extensions: ["extensions/user.js"] }),
});

const echo = defineExtension({
  name: "echo",
  tools: [
    defineTool({
      name: "echo",
      description: "Echoes.",
      parameters: Type.Object({}),
      execute: () => Promise.resolve({ output: [{ type: "text", text: "echo" }] }),
    }),
  ],
});

it("renders a snapshot's prompt once, and only what an edit changed", async () => {
  const { requests, answers } = recording(3);
  // Skills are offered where a tool can read them.
  const agent = await fixture({ builtIns: [{ ...echo, name: "coding" }], answers });
  const first = await agent.snapshot({
    "SYSTEM.md": "You are the installation's assistant.",
    "AGENTS.md": "Be brief.",
    "skills/status/SKILL.md":
      "---\nname: status\ndescription: Report status.\n---\nSay how things are.",
    "settings.json": JSON.stringify({ extensions: ["coding"] }),
  });
  const edited = await agent.snapshot({
    "SYSTEM.md": "You are the installation's careful assistant.",
    "AGENTS.md": "Be brief.",
    "skills/status/SKILL.md":
      "---\nname: status\ndescription: Report status.\n---\nSay how things are.",
    "settings.json": JSON.stringify({ extensions: ["coding"] }),
  });
  for (const [id, root] of [
    ["turn-1", first],
    ["turn-2", first],
    ["turn-3", edited],
  ] as const) {
    await agent.runtime.turns.submit(agent.turn(id, "hello", { resource_root: root }));
    await agent.outcome(id);
  }
  const [one = [], two = [], three = []] = requests;
  const prompt = sections(one);
  expect(prompt["system"]).toBe("You are the installation's assistant.");
  expect(prompt["project_context"]).toContain("Be brief.");
  expect(prompt["skills"]).toContain("<name>status</name>");
  expect(prompt["cwd"]).toBe(`<cwd>\n${agent.workspace}\n</cwd>`);
  expect(systems(two)).toEqual(systems(one));
  // One delta, which also moves the snapshot's paths to the new one.
  const added = systems(three).slice(systems(one).length);
  expect(added).toHaveLength(1);
  const delta = added[0]?.sections ?? {};
  expect(delta["system"]).toBe("You are the installation's careful assistant.");
  expect(delta["skills"]).toContain(edited);
  expect(delta).not.toHaveProperty("cwd");
});

it("applies a toggled extension from the next request of a conversation", async () => {
  const { requests, answers } = recording(2);
  const agent = await fixture({ builtIns: [echo], answers });
  const without = await agent.snapshot({ "settings.json": JSON.stringify({ extensions: [] }) });
  const withEcho = await agent.snapshot({
    "settings.json": JSON.stringify({ extensions: ["echo"] }),
  });
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello", { resource_root: without }));
  await agent.outcome("turn-1");
  await agent.runtime.turns.submit(agent.turn("turn-2", "again", { resource_root: withEcho }));
  await agent.outcome("turn-2");
  const tools = (messages: readonly Message[] = []) =>
    systems(messages).flatMap((message) => (message.toolsAdded ?? []).map((tool) => tool.name));
  expect(tools(requests[0])).toEqual([]);
  expect(tools(requests[1])).toEqual(["echo"]);
});

it("loads a user extension its snapshot lists, also for an answer resumed after a restart", async () => {
  const { requests, answers } = recording(1);
  let agent = await fixture({
    durable: true,
    answers: [heldAnswer("lost", new Promise<void>(() => undefined)), ...answers],
  });
  const root = await agent.snapshot({
    "extensions/greeting.ts":
      'export default { sections: [{ key: "greeting", render: (): string => "Greetings from the user." }] };',
    "settings.json": JSON.stringify({ extensions: { add: ["extensions/greeting.ts"] } }),
  });
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello", { resource_root: root }));
  for (let attempt = 0; agent.model.state.callCount === 0 && attempt < 100; attempt += 1)
    await new Promise((resolve) => setTimeout(resolve, 10));
  agent = await agent.restart();
  fixtures.splice(0, 1, agent);
  await agent.outcome("turn-1");
  expect(sections(requests[0] ?? [])).toMatchObject({
    greeting: "<greeting>\nGreetings from the user.\n</greeting>",
  });
});

it.each([
  ["settings that are not JSON", { "settings.json": "{" }, "is not valid JSON"],
  [
    "a setting it does not know",
    { "settings.json": JSON.stringify({ defaultTools: ["bash"] }) },
    "settings this runtime does not know: defaultTools",
  ],
  [
    "an extension that does not exist",
    { "settings.json": JSON.stringify({ extensions: ["memory"] }) },
    "selects memory, which is neither a built-in extension",
  ],
  [
    "a user extension that throws",
    {
      "extensions/broken.js": 'throw new Error("broken on purpose");',
      "settings.json": JSON.stringify({ extensions: ["extensions/broken.js"] }),
    },
    "could not be loaded: broken on purpose",
  ],
  [
    "a user extension that is no extension",
    {
      "extensions/plain.mjs": "export default 42;",
      "settings.json": JSON.stringify({ extensions: ["extensions/plain.mjs"] }),
    },
    "must export a pi-durable extension object",
  ],
  [
    "a user extension with a bad section key",
    userExtension('{ sections: [{ key: "Bad Key", render: () => "" }] }'),
    'cannot be installed: Section key "Bad Key" must match',
  ],
  [
    "a user extension with a section that renders nothing",
    userExtension('{ sections: [{ key: "greeting" }] }'),
    "/sections/0 must have required properties render",
  ],
  [
    "a user extension with a hook that handles nothing",
    userExtension('{ hooks: [{ task: "pi.generation" }] }'),
    "/hooks/0 must have required properties handlers",
  ],
  [
    "a user extension with a tool that cannot run",
    userExtension('{ tools: [{ name: "x", description: "X.", parameters: {} }] }'),
    "/tools/0 must have required properties execute",
  ],
  [
    "a user extension with a wrap that targets nothing",
    userExtension("{ wraps: [{ wrap: (tool) => tool }] }"),
    "/wraps/0",
  ],
  [
    "a user extension with two tasks of one name",
    userExtension(
      '{ tasks: [{ definition: { name: "my.job" } }, { definition: { name: "my.job" } }] }',
    ),
    "cannot be installed: Task my.job of extension extensions/user.js is already installed",
  ],
  [
    "a user extension with a task name the runtime uses",
    userExtension('{ tasks: [{ definition: { name: "carillon.delivery" } }] }'),
    "cannot be installed: Task carillon.delivery of extension extensions/user.js is already installed",
  ],
])("refuses a turn whose snapshot has %s", async (_name, files, message) => {
  const agent = await fixture({ answers: [fauxAssistantMessage("unused")] });
  const root = await agent.snapshot(files);
  const turn = agent.turn("turn-1", "hello", { resource_root: root });
  await expect(agent.runtime.turns.submit(turn)).rejects.toThrow(message);
  await expect(agent.runtime.turns.submit(turn)).rejects.toMatchObject({ status: 422 });
  expect(agent.model.state.callCount).toBe(0);
});

it("serves with its built-in extensions when the recorded snapshot no longer loads", async () => {
  let agent = await fixture({
    durable: true,
    builtIns: [echo],
    defaults: ["echo"],
    answers: [fauxAssistantMessage("First."), fauxAssistantMessage("Second.")],
  });
  const recorded = await agent.snapshot({});
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello", { resource_root: recorded }));
  await agent.outcome("turn-1");
  await mkdir(join(recorded, "extensions"));
  for (const [path, content] of Object.entries(
    userExtension('{ sections: [{ key: "Bad Key", render: () => "" }] }'),
  ))
    await writeFile(join(recorded, path), content);
  agent = await agent.restart();
  fixtures.splice(0, 1, agent);
  const next = await agent.snapshot({});
  await agent.runtime.turns.submit(agent.turn("turn-2", "again", { resource_root: next }));
  expect(await agent.outcome("turn-2")).toMatchObject({ output: "Second." });
});

it("uses the new sections of user extensions that swap their task names", async () => {
  const { requests, answers } = recording(2);
  const agent = await fixture({ answers });
  const module = (key: string, text: string, task: string) =>
    `export default { sections: [{ key: "${key}", render: () => "${text}" }], tasks: [{ definition: { name: "${task}" } }] };`;
  const settings = JSON.stringify({ extensions: ["extensions/a.js", "extensions/b.js"] });
  const before = await agent.snapshot({
    "extensions/a.js": module("a", "A before.", "my.x"),
    "extensions/b.js": module("b", "B before.", "my.y"),
    "settings.json": settings,
  });
  const after = await agent.snapshot({
    "extensions/a.js": module("a", "A after.", "my.y"),
    "extensions/b.js": module("b", "B after.", "my.x"),
    "settings.json": settings,
  });
  for (const [id, root] of [
    ["turn-1", before],
    ["turn-2", after],
  ] as const) {
    await agent.runtime.turns.submit(agent.turn(id, "hello", { resource_root: root }));
    await agent.outcome(id);
  }
  expect(sections(requests[0] ?? [])).toMatchObject({
    a: "<a>\nA before.\n</a>",
    b: "<b>\nB before.\n</b>",
  });
  expect(sections(requests[1] ?? [])).toMatchObject({
    a: "<a>\nA after.\n</a>",
    b: "<b>\nB after.\n</b>",
  });
});

it("installs a user extension's replacement once the task its predecessor was kept for ends", async () => {
  const { requests, answers } = recording(2);
  const job = Promise.withResolvers<void>();
  const ended = Promise.withResolvers<void>();
  const longJob = defineTask<null, { phase: "run" }, null>({
    name: "my.job",
    version: 1,
    initial: () => ({ phase: "run" }),
    phases: {
      run: async (_task, runtime, context) => {
        await job.promise;
        await runtime.commit(
          () => ({ status: "terminal", outcome: { status: "completed", result: null } }),
          context,
        );
        ended.resolve();
      },
    },
    abort: (_task, runtime, context) =>
      runtime.commit(() => ({ status: "terminal", outcome: { status: "aborted" } }), context),
  });
  const start = defineTool({
    name: "start",
    description: "Starts the job.",
    parameters: Type.Object({}),
    async execute(_args, api, context) {
      await api.createTask(
        longJob,
        null,
        { ownership: { kind: "conversation" }, background: true },
        context,
      );
      return { output: [{ type: "text", text: "started" }] };
    },
  });
  Reflect.set(globalThis, "carillonKept", { start, job: longJob });
  try {
    const agent = await fixture({
      answers: [
        fauxAssistantMessage(fauxToolCall("start", {}), { stopReason: "toolUse" }),
        fauxAssistantMessage("Started."),
        ...answers,
      ],
    });
    // Both define my.shared, so the replacement cannot install beside the module kept for my.job.
    const shared = '{ definition: { name: "my.shared" } }';
    const kept = await agent.snapshot({
      "extensions/old.js": `const { start, job } = globalThis.carillonKept; export default { tools: [start], tasks: [job, ${shared}] };`,
      "settings.json": JSON.stringify({ extensions: ["extensions/old.js"] }),
    });
    const replaced = await agent.snapshot({
      "extensions/new.js": `export default { sections: [{ key: "replacement", render: () => "Replaced." }], tasks: [${shared}] };`,
      "settings.json": JSON.stringify({ extensions: ["extensions/new.js"] }),
    });
    await agent.runtime.turns.submit(agent.turn("turn-1", "start", { resource_root: kept }));
    expect(await agent.outcome("turn-1")).toMatchObject({ output: "Started." });
    await agent.runtime.turns.submit(agent.turn("turn-2", "hello", { resource_root: replaced }));
    await agent.outcome("turn-2");
    job.resolve();
    await ended.promise;
    await agent.runtime.turns.submit(agent.turn("turn-3", "again", { resource_root: replaced }));
    await agent.outcome("turn-3");
    const [whileKept = [], afterwards = []] = requests;
    expect(sections(whileKept)).not.toHaveProperty("replacement");
    expect(sections(afterwards)).toMatchObject({
      replacement: "<replacement>\nReplaced.\n</replacement>",
    });
  } finally {
    job.resolve();
    Reflect.deleteProperty(globalThis, "carillonKept");
  }
});

it("keeps a parked task's extension while its replacement cannot install", async () => {
  const { requests, answers } = recording(2);
  const keep = Promise.withResolvers<void>();
  const kept = Promise.withResolvers<void>();
  const gate = Promise.withResolvers<void>();
  const woke = Promise.withResolvers<void>();
  const gateTask = defineTask<null, { phase: "run" }, null>({
    name: "test.gate",
    version: 1,
    initial: () => ({ phase: "run" }),
    phases: {
      run: async (_task, runtime, context) => {
        await gate.promise;
        await runtime.commit(
          () => ({ status: "terminal", outcome: { status: "completed", result: null } }),
          context,
        );
      },
    },
    abort: (_task, runtime, context) =>
      runtime.commit(() => ({ status: "terminal", outcome: { status: "aborted" } }), context),
  });
  const keeper = defineTask<null, { phase: "run" }, null>({
    name: "my.keeper",
    version: 1,
    initial: () => ({ phase: "run" }),
    phases: {
      run: async (_task, runtime, context) => {
        await keep.promise;
        await runtime.commit(
          () => ({ status: "terminal", outcome: { status: "completed", result: null } }),
          context,
        );
        kept.resolve();
      },
    },
    abort: (_task, runtime, context) =>
      runtime.commit(() => ({ status: "terminal", outcome: { status: "aborted" } }), context),
  });
  // Parked on the gate, the job needs a definition to run again once the gate opens.
  const job = defineTask<null, { phase: "park" | "done" }, null>({
    name: "my.job",
    version: 1,
    initial: () => ({ phase: "park" }),
    phases: {
      park: (_task, runtime, context) =>
        runtime.commit(
          async (tx) => ({
            status: "waiting",
            on: [
              await tx.createTask(gateTask, null, {
                ownership: { kind: "conversation" },
                background: true,
              }),
            ],
            policy: "allSettled",
            checkpoint: { phase: "done" },
          }),
          context,
        ),
      done: async (_task, runtime, context) => {
        await runtime.commit(
          () => ({ status: "terminal", outcome: { status: "completed", result: null } }),
          context,
        );
        woke.resolve();
      },
    },
    abort: (_task, runtime, context) =>
      runtime.commit(() => ({ status: "terminal", outcome: { status: "aborted" } }), context),
  });
  const start = defineTool({
    name: "start",
    description: "Starts the keeper and the job.",
    parameters: Type.Object({}),
    async execute(_args, api, context) {
      const options = { ownership: { kind: "conversation" }, background: true } as const;
      await api.createTask(keeper, null, options, context);
      await api.createTask(job, null, options, context);
      return { output: [{ type: "text", text: "started" }] };
    },
  });
  Reflect.set(globalThis, "carillonParked", { start, keeper, job });
  try {
    const agent = await fixture({
      builtIns: [defineExtension({ name: "gates", tasks: [gateTask] })],
      answers: [
        fauxAssistantMessage(fauxToolCall("start", {}), { stopReason: "toolUse" }),
        fauxAssistantMessage("Started."),
        ...answers,
      ],
    });
    const shared = '{ definition: { name: "my.shared" } }';
    const before = await agent.snapshot({
      "extensions/a.js": `const { start, keeper } = globalThis.carillonParked; export default { tools: [start], tasks: [keeper, ${shared}] };`,
      "extensions/b.js": "export default { tasks: [globalThis.carillonParked.job] };",
      "settings.json": JSON.stringify({ extensions: ["extensions/a.js", "extensions/b.js"] }),
    });
    // The replacement defines my.job, but cannot install beside a.js, kept for my.keeper.
    const replaced = await agent.snapshot({
      "extensions/c.js": `export default { sections: [{ key: "replacement", render: () => "Replaced." }], tasks: [${shared}, { definition: { name: "my.job" } }] };`,
      "settings.json": JSON.stringify({ extensions: ["extensions/c.js"] }),
    });
    await agent.runtime.turns.submit(agent.turn("turn-1", "start", { resource_root: before }));
    expect(await agent.outcome("turn-1")).toMatchObject({ output: "Started." });
    await agent.runtime.turns.submit(agent.turn("turn-2", "hello", { resource_root: replaced }));
    await agent.outcome("turn-2");
    gate.resolve();
    await woke.promise;
    keep.resolve();
    await kept.promise;
    await agent.runtime.turns.submit(agent.turn("turn-3", "again", { resource_root: replaced }));
    await agent.outcome("turn-3");
    const [whileKept = [], afterwards = []] = requests;
    expect(sections(whileKept)).not.toHaveProperty("replacement");
    expect(sections(afterwards)).toMatchObject({
      replacement: "<replacement>\nReplaced.\n</replacement>",
    });
  } finally {
    gate.resolve();
    keep.resolve();
    Reflect.deleteProperty(globalThis, "carillonParked");
  }
});

it("replaces the user extensions a newer snapshot no longer lists", async () => {
  const agent = await fixture({
    answers: [fauxAssistantMessage("First."), fauxAssistantMessage("Second.")],
  });
  const job = '{ tasks: [{ definition: { name: "my.job" } }] }';
  const before = await agent.snapshot({
    "extensions/old.js": `export default ${job};`,
    "settings.json": JSON.stringify({ extensions: ["extensions/old.js"] }),
  });
  const after = await agent.snapshot({
    "extensions/new.js": `export default ${job};`,
    "settings.json": JSON.stringify({ extensions: ["extensions/new.js"] }),
  });
  await agent.runtime.turns.submit(agent.turn("turn-1", "hello", { resource_root: before }));
  await agent.outcome("turn-1");
  await agent.runtime.turns.submit(agent.turn("turn-2", "again", { resource_root: after }));
  expect(await agent.outcome("turn-2")).toMatchObject({ output: "Second." });
});
