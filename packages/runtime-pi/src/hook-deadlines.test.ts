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
 * An agent whose user extension has hooks that never return, each marking
 * its calls with its name in the file `calls`. Each of its turns calls
 * `echo`, twice at once when told to, then answers with what the tool said.
 * Told to delegate, it starts a background subagent instead.
 */
async function hanging(
  hooks: readonly (readonly [task: string, handler: string])[],
  extensions: readonly string[] = [],
) {
  const requests: (readonly Message[])[] = [];
  const model: FauxResponseFactory = (context) => {
    requests.push(context.messages);
    const last = context.messages.findLast((message) => message.role !== "system");
    if (last?.role === "toolResult") return fauxAssistantMessage(`Tool: ${textsOf(last).join()}`);
    const told = textsOf(last).at(-1) ?? "";
    const calls = told.endsWith("Delegate.")
      ? [fauxToolCall("subagent", { task: "Make a response." })]
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
  const registrations = hooks.map(
    ([task, handler]) => `{ task: "${task}", handlers: { ${handler}() {
  appendFileSync(${JSON.stringify(calls)}, "${handler} ");
  return new Promise(() => undefined);
} } }`,
  );
  await writeFile(
    join(folder, "extensions/slow.js"),
    `import { appendFileSync } from "node:fs";\nexport default { hooks: [${registrations.join()}] };`,
  );
  const root = await agent.capture();
  const [[task, handler] = []] = hooks;
  return {
    agent,
    /** What the prompt says once the first of the hooks was late. */
    note: `[Carillon] The ${task} hook ${handler} of your extension extensions/slow.js in ${folder} did not finish within 0.2 seconds, so that extension's hooks are off until you change your files or Carillon restarts. Tell the person, unless you already have.`,
    turn: (id: string, input: string) => agent.turn(id, input, { resource_root: root }),
    ask: (id: string, input: string) => agent.ask(id, input, { resource_root: root }),
    called: () => (existsSync(calls) ? true : undefined),
    calls: () => readFile(calls, "utf8"),
    /** Each request: what the person or the runtime said last, and its prompt's sections. */
    seen: () =>
      requests.map((messages) => {
        const sections: Record<string, string> = {};
        for (const message of messages)
          if (message.role === "system")
            for (const [key, value] of Object.entries(message.sections ?? {}))
              if (value === null) delete sections[key];
              else sections[key] = value;
        const said = textsOf(messages.findLast((message) => message.role === "user"));
        return { said: said.join("\n\n"), sections };
      }),
  };
}

it.each(HOOKS)(
  "answers past a %s %s hook that never returns, turns it off and tells the agent",
  async (task, handler) => {
    const logged = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const off = () => logged.mock.calls.filter(([line]) => String(line).includes("are off"));
    const { ask, calls, seen, note } = await hanging([[task, handler]]);
    // A blocked call would answer with pi-durable's refusal, not the tool's output.
    expect(await ask("turn-1", "hello")).toBe("Tool: echo");
    // An `afterRun` hook is asked after the answer, so it is off a little later.
    await eventually(() => (off().length > 0 ? true : undefined));
    expect(await ask("turn-2", "again")).toBe("Tool: echo");
    expect(await calls()).toBe(`${handler} `);
    const [first, second] = seen();
    // While no hook is off, the prompt is the one an agent without this deadline has.
    expect(first?.said).toBe("hello");
    expect(Object.keys(first?.sections ?? {})).toEqual(["cwd", "conversation_store"]);
    // The request after the tool round is in the run the hook held.
    expect(second?.sections["hooks_off"]).toBe(handler === "afterRun" ? undefined : note);
    expect(seen().at(-1)).toMatchObject({ said: "again", sections: { hooks_off: note } });
    // The log names the file as settings do, not its copy in the snapshot.
    expect(off().map(([line]) => String(line))).toEqual([
      `extension extensions/slow.js: its ${task} hook ${handler} did not finish within 0.2 seconds, so its hooks are off`,
    ]);
  },
);

it.each(HOOKS.slice(0, 2))(
  "stops a message whose run waits in a %s %s hook that never returns",
  async (task, handler) => {
    const { agent, turn, ask, called } = await hanging([[task, handler]]);
    await agent.runtime.turns.submit(turn("turn-1", "hello"));
    await eventually(called);
    await agent.runtime.turns.control("turn-1", "discord-private", "stop");
    expect(await agent.outcome("turn-1")).toEqual({ outcome: "stopped" });
    expect(await ask("turn-2", "again")).toBe("Tool: echo");
  },
);

it("answers and stops messages sent while an afterRun hook that never returns holds the run, telling the one answered", async () => {
  const { agent, turn, ask, called, seen, note } = await hanging([["pi.generation", "afterRun"]]);
  expect(await ask("turn-1", "hello")).toBe("Tool: echo");
  await eventually(called);
  await agent.runtime.turns.submit(turn("turn-2", "again"));
  await agent.runtime.turns.submit(turn("turn-3", "more"));
  await agent.runtime.turns.control("turn-3", "discord-private", "stop");
  expect(await agent.outcome("turn-3")).toEqual({ outcome: "stopped" });
  expect(await agent.outcome("turn-2")).toMatchObject({ outcome: "answered" });
  expect(seen().find(({ said }) => said === "again")?.sections["hooks_off"]).toBe(note);
});

it.each(HOOKS)("closes while a %s %s hook never returns", async (task, handler) => {
  const { agent, turn, called } = await hanging([[task, handler]]);
  await agent.runtime.turns.submit(turn("turn-1", "hello"));
  await eventually(called);
  await expect(agent.runtime.close()).resolves.toBeUndefined();
});

it("tells the agent once about a hook two tool calls waited in at once", async () => {
  const { ask, calls, seen, note } = await hanging([["pi.tool", "beforeTool"]]);
  expect(await ask("turn-1", "Twice.")).toBe("Tool: echo");
  expect(await calls()).toBe("beforeTool beforeTool ");
  expect(seen().at(-1)?.sections["hooks_off"]).toBe(note);
});

it("tells a subagent and the turn its report opens that a hook is off", async () => {
  const { agent, ask, seen, note } = await hanging(
    [["pi.generation", "beforeRequest"]],
    ["subagents"],
  );
  await ask("turn-1", "hello");
  await ask("turn-2", "Delegate.");
  await agent.outcome("opened-1");
  const told = (start: string) =>
    seen().find(({ said }) => said.startsWith(start))?.sections["hooks_off"];
  expect(told("Make a response.")).toBe(note);
  expect(told("[Carillon] The subagent you started in the background finished")).toBe(note);
});

it("turns off every hook of an extension once one of its handlers is late", async () => {
  const { ask, calls, seen, note } = await hanging([
    ["pi.tool", "beforeTool"],
    ["pi.tool", "afterTool"],
    ["pi.generation", "afterTools"],
    ["pi.generation", "afterRun"],
  ]);
  expect(await ask("turn-1", "hello")).toBe("Tool: echo");
  expect(await calls()).toBe("beforeTool ");
  expect(seen().at(-1)?.sections["hooks_off"]).toBe(note);
});

it("runs a hook its handlers object inherits, as pi-durable does", async () => {
  const said: string[] = [];
  const agent = await startFixture({
    answers: [
      (context) => {
        said.push(textsOf(context.messages.findLast((message) => message.role === "user")).join());
        return fauxAssistantMessage("Answer.");
      },
    ],
    resources: {
      "extensions/class.js": `class Handlers {
  beforeRequest({ messages }) {
    return { messages: [...messages, { role: "user", content: "inherited hook ran", timestamp: 0 }] };
  }
}
export default { hooks: [{ task: "pi.generation", handlers: Object.freeze(new Handlers()) }] };`,
      "settings.json": JSON.stringify({ extensions: ["extensions/class.js"] }),
    },
  });
  fixtures.push(agent);
  await agent.ask("turn-1", "hello", { resource_root: await agent.capture() });
  expect(said).toEqual(["inherited hook ran"]);
});
