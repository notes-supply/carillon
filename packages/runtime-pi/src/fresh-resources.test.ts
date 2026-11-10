import {
  fauxAssistantMessage,
  fauxToolCall,
  type FauxResponseFactory,
  type Message,
} from "@earendil-works/pi-ai";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import { afterEach, expect, it } from "vitest";
import { fakeClock, startFixture, textsOf, type Fixture } from "./fixture.ts";

const fixtures: Fixture[] = [];
afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.close()));
});

async function fixture(...args: Parameters<typeof startFixture>) {
  const started = await startFixture(...args);
  fixtures.push(started);
  return started;
}

/** A user extension whose prompt section says which version of it runs. */
const marker = (version: string) =>
  `export default { sections: [{ key: "marker", render: () => "${version}" }] };`;

const resources = {
  "extensions/marker.js": marker("v1"),
  "settings.json": JSON.stringify({
    extensions: { add: ["schedules", "subagents", "extensions/marker.js"] },
  }),
};

/** The marker section in effect at a request, and its last user text. */
interface Seen {
  readonly marker: string | undefined;
  readonly said: string;
}

function seen(messages: readonly Message[]): Seen {
  let section: string | null | undefined;
  for (const message of messages)
    if (message.role === "system" && message.sections !== undefined && "marker" in message.sections)
      section = message.sections["marker"];
  const last = messages.findLast((message) => message.role === "user");
  return {
    marker: section?.replace(/^<marker>\n|\n<\/marker>$/gu, ""),
    said: textsOf(last).join("\n\n"),
  };
}

/**
 * Schedules a run or starts a subagent when asked, and answers anything
 * else; records what each request saw.
 */
function scripted() {
  const requests: Seen[] = [];
  const model: FauxResponseFactory = (context) => {
    requests.push(seen(context.messages));
    const last = context.messages.findLast((message) => message.role !== "system");
    const said = textsOf(last).at(-1) ?? "";
    if (last?.role === "toolResult") return fauxAssistantMessage("Done.");
    if (said === "Schedule it.")
      return fauxAssistantMessage(
        fauxToolCall("schedule", { prompt: "Check the log.", at: "2026-10-24T09:00:00Z" }),
        { stopReason: "toolUse" },
      );
    if (said === "Delegate and wait." || said === "Delegate.")
      return fauxAssistantMessage(
        fauxToolCall("subagent", { task: "Make a response.", wait: said !== "Delegate." }),
        { stopReason: "toolUse" },
      );
    return fauxAssistantMessage(`Ran: ${said}`);
  };
  return { requests, answers: Array.from({ length: 10 }, () => model) };
}

it("runs a schedule on the agent's files as they are when it is due, without a message from a person", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  const { requests, answers } = scripted();
  const agent = await fixture({ clock, resources, answers });
  await agent.ask("turn-1", "Schedule it.", { resource_root: await agent.capture() });
  await writeFile(join(agent.resources ?? "", "extensions/marker.js"), marker("v2"));
  clock.advance(60 * 60_000);
  expect(await agent.outcome("opened-1")).toMatchObject({
    outcome: "answered",
    output: "Ran: Check the log.",
  });
  expect(requests.map(({ marker }) => marker)).toEqual(["v1", "v1", "v2"]);
});

it("runs a subagent on the agent's files as they are when it starts", async () => {
  const { requests, answers } = scripted();
  const agent = await fixture({ resources, answers });
  const root = await agent.capture();
  await writeFile(join(agent.resources ?? "", "extensions/marker.js"), marker("v2"));
  expect(await agent.ask("turn-1", "Delegate and wait.", { resource_root: root })).toBe("Done.");
  expect(requests).toEqual([
    { marker: "v1", said: "Delegate and wait." },
    { marker: "v2", said: "Make a response." },
    { marker: "v2", said: "Delegate and wait." },
  ]);
});

it("runs a background subagent on the agent's files as they are when it starts", async () => {
  const { requests, answers } = scripted();
  const agent = await fixture({ resources, answers });
  const root = await agent.capture();
  await writeFile(join(agent.resources ?? "", "extensions/marker.js"), marker("v2"));
  expect(await agent.ask("turn-1", "Delegate.", { resource_root: root })).toBe("Done.");
  await agent.outcome("opened-1");
  expect(requests.find(({ said }) => said === "Make a response.")?.marker).toBe("v2");
});

it("runs a schedule on the last resources that loaded when the agent's files do not load as it is due, saying why", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  const { requests, answers } = scripted();
  const agent = await fixture({ clock, resources, answers });
  await agent.ask("turn-1", "Schedule it.", { resource_root: await agent.capture() });
  await writeFile(join(agent.resources ?? "", "extensions/marker.js"), "export default {");
  clock.advance(60 * 60_000);
  expect(await agent.outcome("opened-1")).toMatchObject({ outcome: "answered" });
  const run = requests.at(-1);
  expect(run?.marker).toBe("v1");
  expect(run?.said).toMatch(
    new RegExp(
      `^\\[Carillon\\] Your resources in ${agent.resources ?? ""} did not load, so you run on your previous resources until you fix them\\. Tell the person\\.\\n\\nThe agent's extension extensions/marker\\.js could not be loaded: [^]+\\n\\nCheck the log\\.$`,
      "u",
    ),
  );
});

it("runs a subagent on the last resources that loaded when the agent's files do not load as it starts, saying why", async () => {
  const { requests, answers } = scripted();
  const agent = await fixture({ resources, answers });
  const root = await agent.capture();
  await writeFile(join(agent.resources ?? "", "settings.json"), "{");
  expect(await agent.ask("turn-1", "Delegate and wait.", { resource_root: root })).toBe("Done.");
  expect(requests[1]).toEqual({
    marker: "v1",
    said: `[Carillon] Your resources in ${agent.resources ?? ""} did not load, so you run on your previous resources until you fix them. Tell the person.\n\nThe agent's settings.json is not valid JSON: ${jsonError("{")}\n\nMake a response.`,
  });
});

it("runs a subagent on the last resources that loaded when core cannot capture the agent's files, without blaming them", async () => {
  const { requests, answers } = scripted();
  const agent = await fixture({ resources, answers });
  const root = await agent.capture();
  await writeFile(join(agent.resources ?? "", "extensions/marker.js"), marker("v2"));
  agent.core.answer(503);
  expect(await agent.ask("turn-1", "Delegate and wait.", { resource_root: root })).toBe("Done.");
  expect(requests[1]?.marker).toBe("v1");
  expect(requests[1]?.said).toMatch(
    new RegExp(
      `^\\[Carillon\\] Carillon could not copy your resources in ${agent.resources ?? ""}, so you run on your previous resources\\. Your files may be fine\\. Tell the person\\.\\n\\ncore did not capture [^]+answered 503\\n\\nMake a response\\.$`,
      "u",
    ),
  );
});

function jsonError(text: string): string {
  try {
    JSON.parse(text);
  } catch (failure) {
    if (failure instanceof Error) return failure.message;
  }
  throw new Error("valid JSON");
}
