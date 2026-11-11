import {
  fauxAssistantMessage,
  fauxToolCall,
  type FauxResponseFactory,
  type Message,
} from "@earendil-works/pi-ai";
import { defineExtension, defineTool } from "@earendil-works/pi-durable";
import { existsSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { Type } from "typebox";
import { afterEach, expect, it, vi } from "vitest";
import { startFixture, textsOf, type Fixture } from "./fixture.ts";

const fixtures: Fixture[] = [];
afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.close()));
});

async function eventually<T>(read: () => T | undefined): Promise<T> {
  for (let attempt = 0; attempt < 300; attempt += 1) {
    const value = read();
    if (value !== undefined) return value;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error("the expected state never came");
}

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

const HOOKS = [
  ["pi.tool", "beforeTool"],
  ["pi.generation", "beforeRequest"],
  ["pi.generation", "afterRun"],
] as const;

/**
 * An agent whose user extension has one hook that never returns, marking
 * each of its calls in the file `calls`. Each of its turns calls `echo`,
 * twice at once when told to, then answers with what the tool said. Told to
 * delegate, it starts a subagent instead.
 */
async function hanging(task: string, handler: string, extensions: readonly string[] = []) {
  const requests: (readonly Message[])[] = [];
  const model: FauxResponseFactory = (context) => {
    requests.push(context.messages);
    const last = context.messages.findLast((message) => message.role !== "system");
    if (last?.role === "toolResult") return fauxAssistantMessage(`Tool: ${textsOf(last).join()}`);
    const told = textsOf(last).at(-1) ?? "";
    const calls = told.endsWith("Delegate.")
      ? [fauxToolCall("subagent", { task: "Make a response.", wait: true })]
      : Array.from({ length: told.endsWith("Twice.") ? 2 : 1 }, () => fauxToolCall("echo", {}));
    return fauxAssistantMessage(calls, { stopReason: "toolUse" });
  };
  const agent = await startFixture({
    builtIns: [echo],
    answers: Array.from({ length: 12 }, () => model),
    hookDeadlineMs: 200,
    resources: {
      "extensions/slow.js": "export default {};",
      "settings.json": JSON.stringify({
        extensions: ["echo", ...extensions, "extensions/slow.js"],
      }),
    },
  });
  fixtures.push(agent);
  const folder = agent.resources ?? "";
  const calls = join(agent.workspace, "calls");
  await writeFile(
    join(folder, "extensions/slow.js"),
    `import { appendFileSync } from "node:fs";
export default { hooks: [{ task: "${task}", handlers: { ${handler}() {
  appendFileSync(${JSON.stringify(calls)}, ".");
  return new Promise(() => undefined);
} } }] };`,
  );
  const root = await agent.capture();
  return {
    agent,
    note: `[Carillon] The ${task} hook ${handler} of your extension extensions/slow.js in ${folder} did not finish within 0.2 seconds, so it is off until your resources change. Tell the person.`,
    turn: (id: string, input: string) => agent.turn(id, input, { resource_root: root }),
    ask: (id: string, input: string) => agent.ask(id, input, { resource_root: root }),
    called: () => (existsSync(calls) ? true : undefined),
    calls: () => readFile(calls, "utf8"),
    /** What the person or the runtime said last in each request. */
    said: () =>
      requests.map((messages) =>
        textsOf(messages.findLast((message) => message.role === "user")).join("\n\n"),
      ),
  };
}

it.each(HOOKS)(
  "answers past a %s %s hook that never returns, turns it off and tells the agent",
  async (task, handler) => {
    const logged = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const off = () => logged.mock.calls.filter(([line]) => String(line).includes("is off"));
    const { ask, calls, said, note } = await hanging(task, handler);
    // A blocked call would answer with pi-durable's refusal, not the tool's output.
    expect(await ask("turn-1", "hello")).toBe("Tool: echo");
    // An `afterRun` hook is asked after the answer, so it is off a little later.
    await eventually(() => (off().length > 0 ? true : undefined));
    expect(await ask("turn-2", "again")).toBe("Tool: echo");
    expect(await calls()).toBe(".");
    expect(said()[0]).toBe("hello");
    expect(said().at(-1)).toBe(`${note}\n\nagain`);
    expect(off().map(([line]) => String(line))).toEqual([
      expect.stringMatching(
        new RegExp(
          `extensions/slow\\.js: its ${task} hook ${handler} did not finish within 0\\.2 seconds and is off`,
          "u",
        ),
      ),
    ]);
  },
);

it.each(HOOKS.slice(0, 2))(
  "stops a message whose run waits in a %s %s hook that never returns",
  async (task, handler) => {
    const { agent, turn, ask, called } = await hanging(task, handler);
    await agent.runtime.turns.submit(turn("turn-1", "hello"));
    await eventually(called);
    await agent.runtime.turns.control("turn-1", "discord-private", "stop");
    expect(await agent.outcome("turn-1")).toEqual({ outcome: "stopped" });
    expect(await ask("turn-2", "again")).toBe("Tool: echo");
  },
);

it("answers and stops messages sent while an afterRun hook that never returns holds the run", async () => {
  const { agent, turn, ask, called } = await hanging("pi.generation", "afterRun");
  expect(await ask("turn-1", "hello")).toBe("Tool: echo");
  await eventually(called);
  await agent.runtime.turns.submit(turn("turn-2", "again"));
  await agent.runtime.turns.submit(turn("turn-3", "more"));
  await agent.runtime.turns.control("turn-3", "discord-private", "stop");
  expect(await agent.outcome("turn-3")).toEqual({ outcome: "stopped" });
  expect(await agent.outcome("turn-2")).toMatchObject({ outcome: "answered" });
});

it.each(HOOKS)("closes while a %s %s hook never returns", async (task, handler) => {
  const { agent, turn, called } = await hanging(task, handler);
  await agent.runtime.turns.submit(turn("turn-1", "hello"));
  await eventually(called);
  await expect(agent.runtime.close()).resolves.toBeUndefined();
});

it("tells the agent once about a hook two tool calls waited in at once", async () => {
  const { ask, calls, said, note } = await hanging("pi.tool", "beforeTool");
  expect(await ask("turn-1", "Twice.")).toBe("Tool: echo");
  await ask("turn-2", "again");
  expect(await calls()).toBe("..");
  expect(said().at(-1)).toBe(`${note}\n\nagain`);
});

it("tells the agent a hook is off beside why its newer resources did not load", async () => {
  const { agent, ask, said, note } = await hanging("pi.tool", "beforeTool");
  await ask("turn-1", "hello");
  await agent.ask("turn-2", "again", {
    resource_root: await agent.snapshot({ "settings.json": "{" }),
  });
  expect(said().at(-1)).toMatch(/^\[Carillon\] Your resources in \S+ did not load/u);
  expect(said().at(-1)?.endsWith(`\n\n${note}\n\nagain`)).toBe(true);
});

it("tells a run the runtime starts that a hook is off, also when core cannot capture the agent's files", async () => {
  const { agent, ask, said, note } = await hanging("pi.generation", "beforeRequest", ["subagents"]);
  await ask("turn-1", "hello");
  await ask("turn-2", "Delegate.");
  expect(said()).toContain(`${note}\n\nMake a response.`);
  agent.core.answer(503);
  await ask("turn-3", "Delegate.");
  expect(said().at(-3)).toMatch(/^\[Carillon\] Carillon could not copy your resources/u);
  expect(said().at(-3)).toContain(`answered 503\n\n${note}\n\nMake a response.`);
});
