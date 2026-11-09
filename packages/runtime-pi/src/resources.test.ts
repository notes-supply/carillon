import {
  fauxAssistantMessage,
  fauxToolCall,
  type FauxResponseStep,
  type Message,
  type SystemMessage,
} from "@earendil-works/pi-ai";
import { defineExtension, defineTask, defineTool } from "@earendil-works/pi-durable";
import { spawnSync } from "node:child_process";
import { chmod, mkdir, readFile, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { Type } from "typebox";
import { afterEach, expect, it, vi } from "vitest";
import { ATTACH } from "./answers.ts";
import { heldAnswer, pauseTool, startFixture, textsOf, type Fixture } from "./fixture.ts";
import { BUILT_IN_SKILLS } from "./resources.ts";

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

it("offers every agent the conversation history skill and its store, unless it has its own", async () => {
  const { requests, answers } = recording(2);
  const agent = await fixture({ builtIns: [{ ...echo, name: "coding" }], answers });
  const coding = { "settings.json": JSON.stringify({ extensions: ["coding"] }) };
  const own = await agent.snapshot({
    ...coding,
    "skills/conversation-history/SKILL.md":
      "---\nname: conversation-history\ndescription: The agent's own.\n---\nAsk the archive.",
  });
  await agent.ask("turn-1", "hello", { resource_root: await agent.snapshot(coding) });
  await agent.ask("turn-2", "again", { resource_root: own, audience_id: "elsewhere" });
  const [builtIn = {}, replaced = {}] = requests.map(sections);
  expect(builtIn["skills"]).toContain(
    fileURLToPath(new URL("conversation-history/SKILL.md", BUILT_IN_SKILLS)),
  );
  expect(builtIn["conversation_store"]).toBe(
    `<conversation_store>\n${join(agent.componentDirectory, "agents", "default.sqlite")}\n</conversation_store>`,
  );
  expect(replaced["skills"]).toContain(join(own, "skills/conversation-history/SKILL.md"));
  expect(replaced["skills"]).not.toContain(fileURLToPath(BUILT_IN_SKILLS));
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
  expect(tools(requests[0])).toEqual([ATTACH.name]);
  expect(tools(requests[1])).toEqual([ATTACH.name, "echo"]);
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

/** A user extension whose tool says it ran, as the last resources that loaded select it. */
const previous = {
  "extensions/previous.js": `export default { tools: [{ name: "previous", description: "Says it ran.", parameters: { type: "object", properties: {} }, execute: async () => ({ output: [{ type: "text", text: "previous ran" }] }) }] };`,
  "settings.json": JSON.stringify({ extensions: ["extensions/previous.js"] }),
};

/**
 * Scripts an answer per message that first calls `previous`, keeping the
 * person's text the model read and the tool's results.
 */
function callingPrevious(count: number) {
  const read: string[] = [];
  const results: string[] = [];
  const answers = Array.from({ length: count }, (): FauxResponseStep[] => [
    (context) => {
      read.push(textsOf(context.messages.findLast(({ role }) => role === "user")).join("\n\n"));
      return fauxAssistantMessage(fauxToolCall("previous", {}), { stopReason: "toolUse" });
    },
    (context) => {
      results.push(...textsOf(context.messages.at(-1)));
      return fauxAssistantMessage(`Answer ${String(results.length)}.`);
    },
  ]).flat();
  return { read, results, answers };
}

const NOTE = "[Carillon] Your resources did not load";

it.each([
  [
    "settings that are not JSON",
    { "settings.json": "{" },
    "The agent's settings.json is not valid JSON: ",
  ],
  [
    "a setting of the wrong type",
    { "settings.json": JSON.stringify({ busy: "sometimes" }) },
    "invalid at /busy",
  ],
  [
    "a setting it does not know",
    { "settings.json": JSON.stringify({ defaultTools: ["bash"] }) },
    "settings this runtime does not know: defaultTools",
  ],
  [
    "an extensions change it does not know",
    { "settings.json": JSON.stringify({ extensions: { add: ["extensions/a.js"], extra: 1 } }) },
    "settings this runtime does not know at /extensions: extra",
  ],
  [
    "an extensions change with a value of the wrong type",
    { "settings.json": JSON.stringify({ extensions: { remove: [1] } }) },
    "invalid at /extensions/remove/0: must be string",
  ],
  [
    "an extension that does not exist",
    { "settings.json": JSON.stringify({ extensions: ["memory"] }) },
    "selects memory, which is neither a built-in extension",
  ],
  [
    "an MCP server mcp.json does not configure",
    { "settings.json": JSON.stringify({ extensions: ["mcp:nowhere"] }) },
    "selects mcp:nowhere, but the Pi runtime's mcp.json configures no server nowhere",
  ],
  [
    "a user extension whose file is gone",
    { "settings.json": JSON.stringify({ extensions: ["extensions/gone.js"] }) },
    "extensions/gone.js could not be loaded",
  ],
  [
    "a user extension whose file name holds a comma",
    {
      "extensions/a,b.js": "export default {};",
      "settings.json": JSON.stringify({ extensions: ["extensions/a,b.js"] }),
    },
    "selects extensions/a,b.js, which is neither a built-in extension",
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
    "a user extension with a tool wrap that wraps nothing",
    userExtension('{ wraps: [{ tool: "echo", section: 42 }] }'),
    "/wraps/0 must have required properties wrap",
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
  [
    "a user extension that does not parse",
    {
      "extensions/broken.ts": "export default {\n  hooks: [\n};\n",
      "settings.json": JSON.stringify({ extensions: ["extensions/broken.ts"] }),
    },
    "extensions/broken.ts:3:1",
  ],
  [
    "a user extension that throws as it is imported",
    {
      "extensions/throws.ts":
        'const ready = false;\nif (!ready) throw new Error("not ready");\nexport default {};\n',
      "settings.json": JSON.stringify({ extensions: ["extensions/throws.ts"] }),
    },
    "could not be loaded: not ready\nat extensions/throws.ts:2:",
  ],
  [
    "a user extension whose import never settles",
    {
      "extensions/waits.ts": "await new Promise(() => undefined);\nexport default {};\n",
      "settings.json": JSON.stringify({ extensions: ["extensions/waits.ts"] }),
    },
    "extensions/waits.ts could not be loaded: its module did not finish importing within 0.2 seconds",
  ],
  ["a SYSTEM.md it cannot read", { "SYSTEM.md": "Be careful." }, "EACCES"],
])(
  "answers a message whose resources have %s on the last that loaded, telling the agent why",
  async (_name, files, problem) => {
    const { read, results, answers } = callingPrevious(2);
    const agent = await fixture({ answers, importDeadlineMs: 200 });
    await agent.ask("turn-1", "hello", { resource_root: await agent.snapshot(previous) });
    const root = await agent.snapshot(files);
    if (Object.hasOwn(files, "SYSTEM.md")) await chmod(join(root, "SYSTEM.md"), 0);
    expect(await agent.ask("turn-2", "fix it", { resource_root: root })).toBe("Answer 2.");
    expect(results).toEqual(["previous ran", "previous ran"]);
    expect(read[0]).toBe("hello");
    expect(read[1]).toContain(problem);
    expect(read[1]).toMatch(
      /^\[Carillon\] Your resources did not load, so you run on your previous resources until you fix them\.\n\n/u,
    );
    expect(read[1]).toMatch(/\n\nfix it$/u);
    expect(read[1]).not.toContain(root);
  },
);

it("names each build error at its place under Bun, which runs the packaged runtime", async () => {
  const agent = await fixture();
  const root = await agent.snapshot({
    "extensions/broken.ts": 'export default {\n  hooks: [\n    { task: "x" \n  ]\n;\n',
    "settings.json": JSON.stringify({ extensions: ["extensions/broken.ts"] }),
  });
  const script = `
    import { defineExtension } from "@earendil-works/pi-durable";
    import { loadSnapshot } from ${JSON.stringify(join(import.meta.dirname, "resources.ts"))};
    const loaded = await loadSnapshot(process.argv[1], process.argv[1], { carillon: defineExtension({ name: "carillon" }), builtIn: [], defaults: [], importDeadlineMs: 5000 }, process.argv[1]);
    console.log(loaded.problem);
  `;
  const bun = spawnSync("bun", ["-e", script, root], {
    cwd: import.meta.dirname,
    encoding: "utf8",
  });
  expect(bun.stderr).toBe("");
  expect(bun.stdout).toBe(
    [
      'The agent\'s extension extensions/broken.ts could not be loaded: 3 errors building "extensions/broken.ts"',
      'extensions/broken.ts:4:3: Expected "}" but found "]"',
      'extensions/broken.ts:5:1: Expected "]" but found ";"',
      'extensions/broken.ts:5:2: Expected "}" but found end of file',
      "",
    ].join("\n"),
  );
});

it("keeps answering on the last resources that loaded across a restart, until a message brings ones that load", async () => {
  const { read, results, answers } = callingPrevious(3);
  let agent = await fixture({
    durable: true,
    answers: [...answers, fauxAssistantMessage("Fixed.")],
  });
  await agent.ask("turn-1", "hello", { resource_root: await agent.snapshot(previous) });
  const broken = await agent.snapshot({ ...previous, "settings.json": "{" });
  await agent.ask("turn-2", "fix it", { resource_root: broken });
  agent = await agent.restart();
  fixtures.splice(0, 1, agent);
  await agent.ask("turn-3", "still?", { resource_root: broken });
  expect(results).toEqual(["previous ran", "previous ran", "previous ran"]);
  expect(read.map((text) => text.startsWith(NOTE))).toEqual([false, true, true]);
  const fixed = await agent.snapshot({
    "extensions/fixed.js":
      'export default { sections: [{ key: "fixed", render: () => "Fixed for good." }] };',
    "settings.json": JSON.stringify({ extensions: ["extensions/fixed.js"] }),
  });
  const { requests, answers: last } = recording(1);
  agent.model.setResponses(last);
  await agent.ask("turn-4", "better?", { resource_root: fixed });
  const [request = []] = requests;
  expect(textsOf(request.findLast(({ role }) => role === "user"))).toEqual(["better?"]);
  expect(sections(request)).toMatchObject({ fixed: "<fixed>\nFixed for good.\n</fixed>" });
});

it("answers a message whose resources do not load on the default extensions when none loaded before", async () => {
  const { requests, answers } = recording(1);
  const agent = await fixture({ builtIns: [echo], defaults: ["echo"], answers });
  const root = await agent.snapshot({ "settings.json": JSON.stringify({ busy: 1 }) });
  expect(await agent.ask("turn-1", "hello", { resource_root: root })).toBe("Answer 1.");
  const [request = []] = requests;
  expect(textsOf(request.findLast(({ role }) => role === "user"))).toEqual([
    "[Carillon] Your resources did not load, so you run on Carillon's defaults until you fix them.\n\nThe agent's settings.json is invalid at /busy: must be equal to one of the allowed values.\n\nhello",
  ]);
  expect(
    systems(request).flatMap(({ toolsAdded = [] }) => toolsAdded.map(({ name }) => name)),
  ).toEqual([ATTACH.name, "echo"]);
});

it("answers other agents while one waits for an import that never settles, and logs its problem once", async () => {
  const errors = vi.spyOn(console, "error");
  const agent = await fixture({
    agents: ["default", "other"],
    importDeadlineMs: 1_000,
    answers: [
      fauxAssistantMessage("Other."),
      fauxAssistantMessage("Waited."),
      fauxAssistantMessage("Again."),
    ],
  });
  const root = await agent.snapshot({
    "extensions/waits.ts": "await new Promise(() => undefined);\nexport default {};\n",
    "settings.json": JSON.stringify({ extensions: ["extensions/waits.ts"] }),
  });
  const waiting = agent.runtime.turns.submit(
    agent.turn("turn-1", "hello", { resource_root: root }),
  );
  expect(await agent.ask("turn-2", "hi", { agent_id: "other", audience_id: "other" })).toBe(
    "Other.",
  );
  await waiting;
  expect(await agent.outcome("turn-1")).toMatchObject({ output: "Waited." });
  expect(await agent.ask("turn-3", "again", { resource_root: root })).toBe("Again.");
  expect(
    errors.mock.calls.filter(([line]) =>
      String(line).includes("extensions/waits.ts could not be loaded"),
    ),
  ).toHaveLength(1);
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

const agentPage = { kind: "agent", agent_id: "default" } as const;
const settingsFile = (agent: Fixture) => join(agent.resources ?? "", "settings.json");

it("writes a busy mode into the agent's resources, keeping every other field", async () => {
  const agent = await fixture({
    builtIns: [echo],
    resources: {
      "settings.json": JSON.stringify({ extensions: { add: ["echo"] }, busy: "queue" }),
    },
  });
  const lines = await agent.set(agentPage, "busy", "interrupt");
  expect(JSON.parse(await readFile(settingsFile(agent), "utf8"))).toEqual({
    extensions: { add: ["echo"] },
    busy: "interrupt",
  });
  expect(lines.find(({ id }) => id === "busy")).toMatchObject({
    value: { kind: "one", id: "interrupt" },
    inherited: false,
  });
  await chmod(settingsFile(agent), 0o640);
  await agent.set(agentPage, "busy", null);
  expect(JSON.parse(await readFile(settingsFile(agent), "utf8"))).toEqual({
    extensions: { add: ["echo"] },
  });
  expect((await stat(settingsFile(agent))).mode & 0o777).toBe(0o640);
});

it("keeps settings.json's mode and says a change was made when its directory does not sync", async () => {
  const agent = await fixture({ resources: { "settings.json": "{}\n" } });
  const resources = agent.resources ?? "";
  await chmod(settingsFile(agent), 0o640);
  // A directory its owner may write but not read cannot be opened to sync.
  await chmod(resources, 0o300);
  try {
    await expect(agent.set(agentPage, "busy", "queue")).rejects.toMatchObject({
      status: 500,
      message: expect.stringContaining("changed, but not confirmed durable") as unknown,
    });
  } finally {
    await chmod(resources, 0o700);
  }
  expect(JSON.parse(await readFile(settingsFile(agent), "utf8"))).toEqual({ busy: "queue" });
  expect((await stat(settingsFile(agent))).mode & 0o777).toBe(0o640);
});

it("refuses an extension the agent does not have, writing nothing", async () => {
  const agent = await fixture({
    builtIns: [echo],
    resources: { "settings.json": '{"extensions":["echo"]}\n' },
  });
  await expect(agent.set(agentPage, "extensions", ["echo", "nope"])).rejects.toMatchObject({
    status: 422,
    message: expect.stringContaining("nope is not an extension") as unknown,
  });
  expect(await readFile(settingsFile(agent), "utf8")).toBe('{"extensions":["echo"]}\n');
});

it("carries a busy change in the next capture, while an admitted turn keeps its own", async () => {
  const pause = pauseTool();
  const agent = await fixture({
    builtIns: [pause.extension],
    defaults: ["pause"],
    resources: { "settings.json": "{}\n" },
    answers: [
      fauxAssistantMessage(fauxToolCall("pause", {}), { stopReason: "toolUse" }),
      fauxAssistantMessage("First answer."),
      fauxAssistantMessage("Second answer."),
    ],
  });
  const before = await agent.capture();
  await agent.runtime.turns.submit(agent.turn("turn-1", "first", { resource_root: before }));
  await pause.started;
  await agent.set(agentPage, "busy", "queue");
  const after = await agent.capture();
  await agent.runtime.turns.submit(agent.turn("turn-2", "second", { resource_root: after }));
  let activity = agent.runtime.activity("turn-2", "discord-private");
  for (let attempt = 0; activity === undefined && attempt < 300; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 10));
    activity = agent.runtime.activity("turn-2", "discord-private");
  }
  expect(activity).toMatchObject({ phase: "queued" });
  expect(await readFile(join(before, "settings.json"), "utf8")).toBe("{}\n");
  pause.release();
  expect(await agent.outcome("turn-1")).toMatchObject({ output: "First answer." });
  expect(await agent.outcome("turn-2")).toMatchObject({ output: "Second answer." });
});

it("shows an agent without resources what it runs with, offering no change", async () => {
  const agent = await fixture({ builtIns: [echo], defaults: ["echo"] });
  const { settings } = await agent.runtime.settings({ subject: agentPage });
  expect(settings.slice(1)).toMatchObject([
    { id: "busy", choices: [], value: { kind: "one", id: "steer" }, inherited: true },
    { id: "extensions", choices: [], value: { kind: "many", ids: ["echo"] }, inherited: true },
  ]);
  await expect(agent.set(agentPage, "busy", "queue")).rejects.toMatchObject({ status: 422 });
});

it("refuses a change drawn before the agent's default extensions changed", async () => {
  let agent = await fixture({
    durable: true,
    defaults: ["schedules"],
    resources: { "settings.json": "{}\n" },
  });
  const { settings } = await agent.runtime.settings({ subject: agentPage });
  const drawn = settings.find(({ id }) => id === "extensions");
  agent = await agent.restart({ defaults: ["schedules", "subagents"] });
  fixtures.splice(0, 1, agent);
  await expect(
    agent.runtime.settings({
      subject: agentPage,
      change: {
        setting: "extensions",
        value: { kind: "many", ids: ["schedules"] },
        expected: drawn?.revision ?? "",
      },
    }),
  ).rejects.toMatchObject({ status: 409 });
  expect(await readFile(settingsFile(agent), "utf8")).toBe("{}\n");
});

it("shows a selection the resources no longer hold as unavailable, and lets a change remove it", async () => {
  const agent = await fixture({
    resources: {
      "settings.json": JSON.stringify({
        extensions: ["schedules", "extensions/gone.js", "mcp:nowhere"],
        busy: "queue",
      }),
    },
  });
  const extensions = async () =>
    (await agent.runtime.settings({ subject: agentPage })).settings.find(
      ({ id }) => id === "extensions",
    );
  expect(await extensions()).toMatchObject({
    value: { kind: "many", ids: ["schedules", "extensions/gone.js", "mcp:nowhere"] },
    choices: [
      { id: "schedules", name: "schedules" },
      { id: "subagents", name: "subagents" },
      { id: "extensions/gone.js", name: "extensions/gone.js (unavailable)" },
      { id: "mcp:nowhere", name: "mcp:nowhere (unavailable)" },
    ],
  });
  await expect(
    agent.set(agentPage, "extensions", ["schedules", "extensions/gone.js"]),
  ).rejects.toMatchObject({
    status: 422,
    message: expect.stringContaining("extensions/gone.js is not an extension") as unknown,
  });
  await agent.set(agentPage, "extensions", ["schedules"]);
  expect(JSON.parse(await readFile(settingsFile(agent), "utf8"))).toEqual({
    extensions: ["schedules"],
    busy: "queue",
  });
  await writeFile(settingsFile(agent), JSON.stringify({ extensions: { add: ["mcp:nowhere"] } }));
  expect((await extensions())?.value).toEqual({ kind: "many", ids: ["mcp:nowhere"] });
  await agent.set(agentPage, "extensions", null);
  expect(JSON.parse(await readFile(settingsFile(agent), "utf8"))).toEqual({});
});

it("offers only module files whose names the CLI can list as the agent's own extensions", async () => {
  const agent = await fixture({
    resources: {
      "settings.json": "{}\n",
      "extensions/a,b.js": "export default {};",
      "extensions/notes.js": "export default {};",
      "extensions/folder.js/index.js": "export default {};",
    },
  });
  const { settings } = await agent.runtime.settings({ subject: agentPage });
  expect(settings.find(({ id }) => id === "extensions")?.choices.map(({ id }) => id)).toEqual([
    "schedules",
    "subagents",
    "extensions/notes.js",
  ]);
  await expect(agent.set(agentPage, "extensions", ["extensions/folder.js"])).rejects.toMatchObject({
    status: 422,
  });
  expect(await readFile(settingsFile(agent), "utf8")).toBe("{}\n");
});
