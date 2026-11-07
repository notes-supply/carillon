import {
  fauxAssistantMessage,
  fauxProvider,
  fauxToolCall,
  type FauxResponseFactory,
  type FauxResponseStep,
  type Message,
} from "@earendil-works/pi-ai";
import { defineExtension, defineTool } from "@earendil-works/pi-durable";
import { displayableExplanation } from "@carillon/runtime/failure";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { Type } from "typebox";
import { afterEach, expect, it, vi } from "vitest";
import { startFixture, textsOf, type Fixture } from "./fixture.ts";
import { BUILT_IN_SKILLS } from "./resources.ts";

const fixtures: Fixture[] = [];
afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.close()));
});

/** An agent that answers `count` requests, keeping what each sent. */
async function agent(count = 1) {
  const requests: (readonly Message[])[] = [];
  const answers: FauxResponseStep[] = Array.from({ length: count }, () => (context) => {
    requests.push(context.messages);
    return fauxAssistantMessage("Done.");
  });
  // Skills reach the prompt where a tool can read them.
  const started = await startFixture({
    builtIns: [defineExtension({ name: "coding" })],
    answers,
  });
  fixtures.push(started);
  return { ...started, requests };
}

const skill = (name: string, body: string, hidden = false) => ({
  [`skills/${name}/SKILL.md`]: `---\nname: ${name}\ndescription: The ${name} skill.\n${hidden ? "disable-model-invocation: true\n" : ""}---\n\n${body}\n`,
});

const resources = {
  "settings.json": JSON.stringify({ extensions: ["coding"] }),
  ...skill("triage", "Sort the inbox by urgency."),
  ...skill("hooks-howto", "Hooks run around each tool call.", true),
};

/** Pi's block for the snapshot's skill. */
const block = (root: string, name: string, body: string) =>
  `<skill name="${name}" location="${join(root, "skills", name, "SKILL.md")}">\nReferences are relative to ${join(root, "skills", name)}.\n\n${body}\n</skill>`;

const triage = (root: string) => block(root, "triage", "Sort the inbox by urgency.");
const howto = (root: string) => block(root, "hooks-howto", "Hooks run around each tool call.");

/** What the person's last message said to the model, text by text. */
function said(messages: readonly Message[] | undefined): string[] {
  return textsOf(messages?.findLast((message) => message.role === "user"));
}

it("places a skill the message invokes before it, as Pi's /skill:name does", async () => {
  const runtime = await agent(4);
  const root = await runtime.snapshot(resources);
  await runtime.ask("turn-1", "/skill:triage start with the oldest", { resource_root: root });
  await runtime.ask("turn-2", "/skill:triage", { resource_root: root, audience_id: "two" });
  await runtime.ask("turn-3", "Please use $triage.", { resource_root: root, audience_id: "three" });
  await runtime.ask("turn-4", "/skill:triage\nstart with\nthe oldest", {
    resource_root: root,
    audience_id: "four",
  });
  const [slash, bare, mention, lines] = runtime.requests.map(said);
  expect(slash).toEqual([`${triage(root)}\n\nstart with the oldest`]);
  expect(bare).toEqual([triage(root)]);
  expect(mention).toEqual([`${triage(root)}\n\nPlease use $triage.`]);
  expect(lines).toEqual([`${triage(root)}\n\nstart with\nthe oldest`]);
});

it("reads only the person's own text, placing the interface's context first", async () => {
  const runtime = await agent(3);
  const root = await runtime.snapshot(resources);
  const context =
    'In reply to a message from "$triage" (Discord user 201), sent 2026-11-05T14:03:27+00:00, with files ["$triage.png"]:\n> use $hooks-howto';
  const native = { context, origin: { channel_id: "300" } };
  await runtime.ask("turn-1", "/skill:triage review it", { resource_root: root, native });
  await runtime.ask("turn-2", "What does it mean?", {
    resource_root: root,
    audience_id: "two",
    native,
  });
  await runtime.ask("turn-3", "", { resource_root: root, audience_id: "three", native });
  const [command, plain, bare] = runtime.requests.map(said);
  expect(command).toEqual([`${context}\n\n${triage(root)}\n\nreview it`]);
  expect(plain).toEqual([`${context}\n\nWhat does it mean?`]);
  expect(bare).toEqual([context]);
  const unknown = runtime.turn("turn-4", "/skill:triag", { resource_root: root, native });
  await expect(runtime.runtime.turns.submit(unknown)).rejects.toMatchObject({ status: 422 });
});

it("invokes each named skill once, in the order first named, and nothing in code", async () => {
  const runtime = await agent(2);
  const root = await runtime.snapshot(resources);
  const message = "$hooks-howto then $triage, again $triage and $hooks-howto";
  await runtime.ask("turn-1", message, { resource_root: root });
  await runtime.ask("turn-2", "/skill:triage first, $hooks-howto and $triage", {
    resource_root: root,
    audience_id: "two",
  });
  const [mentions, both] = runtime.requests.map(said);
  expect(mentions).toEqual([`${howto(root)}\n\n${triage(root)}\n\n${message}`]);
  expect(both).toEqual([`${triage(root)}\n\n${howto(root)}\n\nfirst, $hooks-howto and $triage`]);
});

it.each([
  ["a code span", "Run `echo $triage` for me"],
  ["a longer code span", "Run ``echo `$triage` `` for me"],
  ["a fenced block", "Look:\n```sh\necho $triage\n```\nthanks"],
  ["a fence opened mid-line", "look: ```\n$triage\n```"],
  ["a code span across lines", "Explain `echo\n$triage\n` please"],
  ["a longer code span across lines", "Explain ``echo `x`\n$triage\n`` please"],
  ["a word", "pay US$triage or a$triage"],
  ["a doubled sign", "costs $$triage"],
  ["a longer word", "$triaged $triage-next $triage_x"],
  ["a word of astral letters", "\u{10400}$triage $triage\u{10400}"],
  ["an unknown name", "echo $HOME and $PATH"],
  ["a skill command not at the start", "try /skill:triage"],
])("leaves a message naming a skill in %s as it is", async (_place, message) => {
  const runtime = await agent();
  const root = await runtime.snapshot(resources);
  await runtime.ask("turn-1", message, { resource_root: root });
  expect(said(runtime.requests[0])).toEqual([message]);
});

it.each([
  ["after code ends", "```\n$triage\n```\n`$triage` and ($triage)"],
  ["after a fence on one line", "```echo hi``` and $triage"],
  ["after a fence of CRLF lines", "```sh\r\necho hi\r\n```\r\nPlease $triage"],
  ["after a backtick never closed", "it costs `5 and $triage"],
  ["in indented text, which Discord does not show as code", "Example:\n\n    echo $triage"],
  ["in a tilde fence, which Discord does not show as code", "~~~\n$triage\n~~~"],
  ["in the person's own quote", "> use $triage\n>>> and more"],
])("invokes a skill named %s", async (_place, message) => {
  const runtime = await agent();
  const root = await runtime.snapshot(resources);
  await runtime.ask("turn-1", message, { resource_root: root });
  expect(said(runtime.requests[0])).toEqual([`${triage(root)}\n\n${message}`]);
});

it.each([
  ["/skill:triag the inbox", 'No skill is named "triag".'],
  ["/skill:", "Name a skill after /skill:."],
  ["/skill: triage", "Name a skill after /skill:."],
])("refuses %j, listing the skills", async (message, refusal) => {
  const runtime = await agent();
  const root = await runtime.snapshot(resources);
  const turn = runtime.turn("turn-1", message, { resource_root: root });
  await expect(runtime.runtime.turns.submit(turn)).rejects.toMatchObject({ status: 422 });
  await expect(runtime.runtime.turns.submit(turn)).rejects.toSatisfy(
    (failure: Error) =>
      displayableExplanation(failure.message) ===
      `${refusal} The skills are carillon-hooks, conversation-history, hooks-howto, triage.`,
  );
  expect(runtime.model.state.callCount).toBe(0);
});

it("invokes an invocation-only skill that the prompt never offers", async () => {
  const runtime = await agent();
  const root = await runtime.snapshot(resources);
  await runtime.ask("turn-1", "/skill:hooks-howto log each answer", { resource_root: root });
  const [messages = []] = runtime.requests;
  const prompt = messages.flatMap((message) =>
    message.role === "system" ? Object.values(message.sections ?? {}) : [],
  );
  expect(prompt.join("\n")).toContain("<name>triage</name>");
  expect(prompt.join("\n")).not.toContain("hooks-howto");
  expect(said(messages)).toEqual([`${howto(root)}\n\nlog each answer`]);
});

it("keeps a message's images and files as they are beside the invoked skill", async () => {
  const runtime = await agent();
  const root = await runtime.snapshot(resources);
  const store = join(runtime.componentDirectory, "..", "attachments");
  const picture = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
    "base64",
  );
  await mkdir(join(store, "capture"), { recursive: true });
  await writeFile(join(store, "capture", "0"), picture);
  await runtime.ask("turn-1", "$triage what is this?", {
    resource_root: root,
    attachment_root: store,
    attachments: [
      {
        path: "capture/0",
        name: "picture.png",
        size: picture.length,
        content_type: "image/png",
        sha256: "0".repeat(64),
      },
    ],
  });
  const message = runtime.requests[0]?.findLast((item) => item.role === "user");
  const [input, note, image, ...rest] =
    message?.role === "user" && typeof message.content !== "string" ? message.content : [];
  expect(input).toEqual({ type: "text", text: `${triage(root)}\n\n$triage what is this?` });
  expect(note?.type === "text" && note.text).toContain('name: "picture.png"');
  expect(image).toEqual({ type: "image", data: picture.toString("base64"), mimeType: "image/png" });
  expect(rest).toEqual([]);
});

it("lists the agent's skills as commands, invocation-only ones included", async () => {
  const runtime = await startFixture({ resources });
  fixtures.push(runtime);
  const query = { interface_id: "discord-private", agent_id: "default", channel_id: "300" };
  expect(await runtime.runtime.commands(query)).toEqual({
    commands: [
      expect.objectContaining({ name: "skill:carillon-hooks" }),
      expect.objectContaining({ name: "skill:conversation-history" }),
      { name: "skill:hooks-howto", description: "The hooks-howto skill." },
      { name: "skill:triage", description: "The triage skill." },
    ],
  });
  const long = "a".repeat(95);
  await mkdir(join(runtime.resources ?? "", "skills", long), { recursive: true });
  await writeFile(
    join(runtime.resources ?? "", "skills", long, "SKILL.md"),
    `---\nname: ${long}\ndescription: Too long a command.\n---\nBody.`,
  );
  expect((await runtime.runtime.commands(query)).commands).toHaveLength(4);
  await expect(runtime.runtime.commands({ ...query, agent_id: "other" })).rejects.toMatchObject({
    status: 422,
  });
});

const HOOKS = fileURLToPath(new URL("carillon-hooks/SKILL.md", BUILT_IN_SKILLS));

/** The skill's examples, each a user extension module. */
async function examples(): Promise<string[]> {
  const text = await readFile(HOOKS, "utf8");
  return [...text.matchAll(/^```ts\n([\s\S]*?)^```$/gmu)].map(([, code = ""]) => code);
}

it("ships the hooks skill invocation-only, and invokes it", async () => {
  const runtime = await agent();
  const root = await runtime.snapshot({
    "settings.json": JSON.stringify({ extensions: ["coding"] }),
  });
  await runtime.ask("turn-1", "/skill:carillon-hooks guard my pushes", { resource_root: root });
  const [messages = []] = runtime.requests;
  const prompt = messages.flatMap((message) =>
    message.role === "system" ? Object.values(message.sections ?? {}) : [],
  );
  expect(prompt.join("\n")).toContain("<name>conversation-history</name>");
  expect(prompt.join("\n")).not.toContain("carillon-hooks");
  const [text = ""] = said(messages);
  expect(text.startsWith(`<skill name="carillon-hooks" location="${HOOKS}">\n`)).toBe(true);
  expect(text).toContain("## Hooks");
  expect(text).not.toContain("disable-model-invocation");
  expect(text.endsWith("</skill>\n\nguard my pushes")).toBe(true);
  const { commands } = await runtime.runtime.commands({
    interface_id: "discord-private",
    agent_id: "default",
    channel_id: "300",
  });
  const hooks = commands.find(({ name }) => name === "skill:carillon-hooks");
  expect(hooks?.description?.length, "Discord's menu shows 100 characters").toBeLessThanOrEqual(
    100,
  );
});

it("refuses only the force pushes the hooks skill's guard names", async () => {
  const [guard = ""] = await examples();
  const forced = [
    "git push --force origin main",
    "git push -f",
    "git push -fu origin main",
    "git push origin +main",
    "git -C . push -f",
    "cd repo && git push --force",
  ];
  const allowed = ["git push --force-with-lease origin main", "git push origin main; rm -f x"];
  const commands: string[] = [];
  const bash = defineTool({
    name: "bash",
    description: "Runs a command.",
    parameters: Type.Object({ command: Type.String() }),
    execute: ({ command }) => {
      commands.push(command);
      return Promise.resolve({ output: [{ type: "text", text: "ran" }] });
    },
  });
  const results: string[] = [];
  const runtime = await startFixture({
    builtIns: [defineExtension({ name: "coding", tools: [bash] })],
    answers: [
      fauxAssistantMessage(
        [...forced, ...allowed].map((command) => fauxToolCall("bash", { command })),
        { stopReason: "toolUse" },
      ),
      (context) => {
        for (const message of context.messages)
          if (message.role === "toolResult") results.push(...textsOf(message));
        return fauxAssistantMessage("Done.");
      },
    ],
  });
  fixtures.push(runtime);
  const root = await runtime.snapshot({
    "extensions/guard.ts": guard,
    "settings.json": JSON.stringify({ extensions: ["coding", "extensions/guard.ts"] }),
  });
  await runtime.ask("turn-1", "push it", { resource_root: root });
  expect(commands).toEqual(allowed);
  const refused: unknown = expect.stringContaining("Force pushes are off for this agent.");
  expect(results).toEqual([...forced.map(() => refused), ...allowed.map(() => "ran")]);
});

it("logs each response and asks ChatGPT models for longer answers, as the hooks skill shows", async () => {
  const [, logger = "", payload = ""] = await examples();
  const sent: unknown[] = [];
  const body = { model: "gpt", text: { verbosity: "low", format: { type: "text" } } };
  const answer: FauxResponseFactory = async (_context, options, _state, model) => {
    sent.push(await options?.onPayload?.(body, model));
    return fauxAssistantMessage("Done.");
  };
  const logged = vi.spyOn(console, "error").mockImplementation(() => undefined);
  const runtime = await startFixture({ answers: [answer] });
  fixtures.push(runtime);
  const chatgpt = fauxProvider({
    api: "openai-codex-responses",
    provider: "chatgpt",
    models: [{ id: "codex", name: "Codex" }],
  });
  chatgpt.setResponses([answer]);
  runtime.models.setProvider(chatgpt.provider);
  const root = await runtime.snapshot({
    "extensions/log.ts": logger,
    "extensions/verbose.ts": payload,
    "settings.json": JSON.stringify({ extensions: ["extensions/log.ts", "extensions/verbose.ts"] }),
  });
  await runtime.ask("turn-1", "hello", { resource_root: root });
  await runtime.set({ kind: "agent", agent_id: "default" }, "model", "chatgpt/codex");
  await runtime.ask("turn-2", "hello again", { resource_root: root });
  const lines = logged.mock.calls.map(([line]: unknown[]) => line);
  logged.mockRestore();
  expect(sent).toEqual([
    body,
    { model: "gpt", text: { verbosity: "high", format: { type: "text" } } },
  ]);
  expect(lines).toEqual(
    expect.arrayContaining([
      expect.stringMatching(
        new RegExp(`^${runtime.model.provider.id}/scripted: stop, \\d+ tokens$`, "u"),
      ),
      expect.stringMatching(/^chatgpt\/codex: stop, \d+ tokens$/u),
    ]),
  );
});
