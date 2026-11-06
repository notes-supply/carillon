import {
  fauxAssistantMessage,
  fauxProvider,
  fauxToolCall,
  type FauxResponseFactory,
  type FauxResponseStep,
  type TranscriptContext,
} from "@earendil-works/pi-ai";
import { createDiscordExtension } from "@carillon/discord-pi";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { join } from "node:path";
import { afterEach, expect, it, onTestFinished } from "vitest";
import { ConversationInfo } from "./documents.ts";
import {
  fakeClock,
  heldAnswer,
  pauseTool,
  startFixture,
  textsOf,
  type Fixture,
} from "./fixture.ts";

const fixtures: Fixture[] = [];
afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.close()));
});

async function fixture(...args: Parameters<typeof startFixture>) {
  const started = await startFixture(...args);
  fixtures.push(started);
  return started;
}

const TASK = "Name three primes.";

/** The key a background subagent's report opens its turn under. */
function reportKey(id: string | undefined): unknown {
  return expect.stringMatching(new RegExp(`^default:[0-9a-f-]{36}:subagent:${String(id)}$`, "u"));
}

/** Waits until the runtime has asked core to open `count` turns. */
async function opened(agent: Fixture, count: number): Promise<void> {
  for (let attempt = 0; agent.openings().length < count && attempt < 100; attempt += 1)
    await new Promise((resolve) => setTimeout(resolve, 20));
  expect(agent.openings()).toHaveLength(count);
}

/** What one of a subagent's model requests ran on, and the provider body it would send. */
interface ChildRequest {
  readonly model: string;
  readonly thinking: string;
  readonly payload: unknown;
}

async function requestOf(
  options: Parameters<FauxResponseFactory>[1],
  model: Parameters<FauxResponseFactory>[3],
): Promise<ChildRequest> {
  return {
    model: `${model.provider}/${model.id}`,
    thinking: options?.reasoning ?? "off",
    payload: await options?.onPayload?.({ input: [] }, model),
  };
}

/**
 * The parent delegates `TASK` with `choices` when asked to, in the
 * background or waiting, and says what the tool or a report said; the
 * subagent records each request in `requests` and answers the task once
 * `child` lets it, and the parent's answer after a tool call can be held
 * until it is stopped.
 */
function scripted({
  child = Promise.resolve(),
  holdAfterTool = false,
  choices = {},
}: { child?: Promise<void>; holdAfterTool?: boolean; choices?: Record<string, string> } = {}) {
  const asked = Promise.withResolvers<void>();
  const requests: ChildRequest[] = [];
  const model: FauxResponseFactory = async (context, options, state, definition) => {
    const last = context.messages.findLast((message) => message.role !== "system");
    const texts = textsOf(last);
    const said = texts.at(-1) ?? "";
    if (last?.role === "toolResult")
      return holdAfterTool
        ? heldAnswer("unused", new Promise<void>(() => undefined))(
            context,
            options,
            state,
            definition,
          )
        : fauxAssistantMessage(`Tool: ${texts.join(" ")}`);
    if (said === "Delegate." || said === "Delegate and wait.")
      return fauxAssistantMessage(
        fauxToolCall("subagent", {
          task: TASK,
          ...choices,
          ...(said === "Delegate." ? {} : { wait: true }),
        }),
        { stopReason: "toolUse" },
      );
    if (said === TASK) {
      requests.push(await requestOf(options, definition));
      asked.resolve();
      return heldAnswer("2, 3 and 5.", child)(context, options, state, definition);
    }
    if (texts[0]?.startsWith("[Carillon] The subagent") === true)
      return fauxAssistantMessage(`Reported: ${said}`);
    return fauxAssistantMessage(`Unexpected: ${said}`);
  };
  return { answers: Array.from({ length: 20 }, () => model), asked: asked.promise, requests };
}

/** Adds `openai-test/gpt`, a faux model on OpenAI's Responses API, to the agent's catalogue. */
function responsesModel(agent: Fixture, answers: FauxResponseStep[]): void {
  const openai = fauxProvider({
    api: "openai-responses",
    provider: "openai-test",
    models: [{ id: "gpt", reasoning: true }],
  });
  openai.setResponses(answers);
  agent.models.setProvider(openai.provider);
}

it("runs a subagent in the background and brings its answer back in a turn it opens", async () => {
  const { answers } = scripted();
  const agent = await fixture({ defaults: ["subagents"], answers });
  const started = await agent.ask("turn-1", "Delegate.");
  const id = /Subagent (\d+) is working on it in the background/u.exec(started)?.[1];
  expect(id, started).toBeDefined();
  expect(await agent.outcome("opened-1")).toMatchObject({
    outcome: "answered",
    output: "Reported: 2, 3 and 5.",
  });
  expect(agent.openings()).toEqual([
    {
      interface_id: "discord-private",
      agent_id: "default",
      audience_id: "conversation",
      key: reportKey(id),
      label: "Subagent report",
    },
  ]);
});

it("returns a waited-for subagent's answer from its call, run on the caller's model and thinking", async () => {
  const { answers, requests } = scripted();
  const agent = await fixture({ agentModel: "reasoner", defaults: ["subagents"], answers });
  const reasoner = `${agent.model.provider.id}/reasoner`;
  expect(await agent.ask("turn-1", "Delegate and wait.")).toBe(
    `Tool: The subagent runs ${reasoner} with thinking medium, at standard speed. 2, 3 and 5.`,
  );
  expect(requests).toEqual([{ model: reasoner, thinking: "medium", payload: { input: [] } }]);
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(agent.openings()).toEqual([]);
});

it("leaves a background subagent working when the answer that started it is stopped", async () => {
  const child = Promise.withResolvers<void>();
  const { answers, asked } = scripted({ child: child.promise, holdAfterTool: true });
  const agent = await fixture({ defaults: ["subagents"], answers });
  await agent.runtime.turns.submit(agent.turn("turn-1", "Delegate."));
  await asked;
  await agent.runtime.turns.control("turn-1", "discord-private", "stop");
  expect(await agent.outcome("turn-1")).toEqual({ outcome: "stopped" });
  child.resolve();
  expect(await agent.outcome("opened-1")).toMatchObject({
    outcome: "answered",
    output: "Reported: 2, 3 and 5.",
  });
});

it("offers a subagent neither subagents, schedules nor attach", async () => {
  const offered: string[][] = [];
  const offer = (context: TranscriptContext) =>
    offered.push(
      context.messages.flatMap((message) =>
        message.role === "system" ? (message.toolsAdded ?? []).map((tool) => tool.name) : [],
      ),
    );
  const agent = await fixture({
    defaults: ["subagents", "schedules"],
    answers: [
      (context) => {
        offer(context);
        return fauxAssistantMessage(fauxToolCall("subagent", { task: TASK, wait: true }), {
          stopReason: "toolUse",
        });
      },
      (context) => {
        offer(context);
        return fauxAssistantMessage("2, 3 and 5.");
      },
      fauxAssistantMessage("Done."),
    ],
  });
  expect(await agent.ask("turn-1", "Delegate and wait.")).toBe("Done.");
  const [parent, child] = offered;
  expect(parent).toContain("subagent");
  expect(parent).toContain("schedule");
  expect(parent).toContain("attach");
  expect(child).toEqual([]);
});

it("brings a background subagent's answer back once, on the model it chose, across restarts in its run and its report", async () => {
  const child = Promise.withResolvers<void>();
  const { answers, asked, requests } = scripted({
    child: child.promise,
    choices: { model: "faux/reasoner", thinking: "low" },
  });
  let agent = await fixture({ durable: true, defaults: ["subagents"], answers });
  expect(await agent.ask("turn-1", "Delegate.")).toMatch(
    /^Tool: The subagent runs faux\/reasoner with thinking low, at standard speed\. Subagent \d+ is working/u,
  );
  await asked;
  agent = await agent.restart();
  fixtures.splice(0, 1, agent);
  const stall = agent.core.stall("/v1/runtime/turns");
  child.resolve();
  await stall.reached;
  agent = await agent.restart();
  fixtures.splice(0, 1, agent);
  stall.release();
  expect(await agent.outcome("opened-1")).toMatchObject({
    outcome: "answered",
    output: "Reported: 2, 3 and 5.",
  });
  const [first, second] = agent.openings();
  expect(second).toEqual(first);
  expect(
    agent.core.received.filter((call) => call.path === "/v1/runtime/turns/opened-1/outcome"),
  ).toHaveLength(1);
  expect(requests).not.toHaveLength(0);
  for (const request of requests)
    expect(request).toEqual({ model: "faux/reasoner", thinking: "low", payload: { input: [] } });
});

it("gives a background subagent's report up once core refuses it for good", async () => {
  const clock = fakeClock("2026-10-24T08:00:00Z");
  const child = Promise.withResolvers<void>();
  const { answers } = scripted({ child: child.promise });
  const agent = await fixture({ clock, defaults: ["subagents"], answers });
  await agent.ask("turn-1", "Delegate.");
  agent.core.answer(409);
  child.resolve();
  await opened(agent, 1);
  clock.advance(60_000);
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(agent.openings()).toHaveLength(1);
});

it("aborts a waited-for subagent when the answer waiting for it is stopped", async () => {
  const asked = Promise.withResolvers<void>();
  const aborted = Promise.withResolvers<void>();
  const agent = await fixture({
    defaults: ["subagents"],
    answers: [
      fauxAssistantMessage(fauxToolCall("subagent", { task: TASK, wait: true }), {
        stopReason: "toolUse",
      }),
      (_context, options) => {
        asked.resolve();
        return new Promise((_resolve, reject) => {
          options?.signal?.addEventListener("abort", () => {
            aborted.resolve();
            reject(new Error("aborted"));
          });
        });
      },
    ],
  });
  await agent.runtime.turns.submit(agent.turn("turn-1", "Delegate and wait."));
  await asked.promise;
  await agent.runtime.turns.control("turn-1", "discord-private", "stop");
  expect(await agent.outcome("turn-1")).toEqual({ outcome: "stopped" });
  await aborted.promise;
});

it("lets a subagent use Discord in the conversation it serves", async () => {
  const operations: unknown[] = [];
  const provider = createServer((request, response) => {
    let body = "";
    request.on("data", (part: Buffer) => (body += part.toString()));
    request.on("end", () => {
      operations.push(JSON.parse(body));
      response.setHeader("content-type", "application/json");
      response.end(JSON.stringify({ guild_id: "1" }));
    });
  });
  provider.listen(0, "127.0.0.1");
  await once(provider, "listening");
  onTestFinished(() => new Promise<void>((done) => provider.close(() => done())));
  const address = provider.address();
  if (address === null || typeof address === "string") throw new Error("expected a TCP listener");
  const token = "operator-test-token";
  let tokenFile = "";
  const discord = createDiscordExtension({
    guidance: "Discord guidance.",
    async context(api, context) {
      const info = await api.snapshot(ConversationInfo, api.conversationId, context);
      return {
        interface_id: info?.interface_id,
        native: info?.native,
        connection: {
          url: `http://127.0.0.1:${String(address.port)}`,
          token_file: tokenFile,
          token_hash: createHash("sha256").update(token).digest("hex"),
        },
      };
    },
  });
  const agent = await fixture({
    builtIns: [discord],
    defaults: ["subagents", "discord"],
    answers: [
      fauxAssistantMessage(fauxToolCall("subagent", { task: TASK, wait: true }), {
        stopReason: "toolUse",
      }),
      fauxAssistantMessage(fauxToolCall("enable_discord", {}), { stopReason: "toolUse" }),
      (context) => {
        const result = context.messages.findLast((message) => message.role === "toolResult");
        return fauxAssistantMessage(textsOf(result)[0] ?? "");
      },
      (context) => {
        const result = context.messages.findLast((message) => message.role === "toolResult");
        return fauxAssistantMessage(textsOf(result).join(""));
      },
    ],
  });
  tokenFile = join(agent.componentDirectory, "operator.token");
  await writeFile(tokenFile, token);
  const origin = {
    guild_id: "1",
    channel_id: "2",
    source_channel_id: "2",
    message_id: "3",
    user_id: "4",
  };
  await agent.runtime.turns.submit(
    agent.turn("turn-1", "Delegate and wait.", { native: { origin } }),
  );
  const outcome = await agent.outcome("turn-1");
  expect(outcome.outcome === "answered" && outcome.output).toContain(
    '"origin":{"guild_id":"1","channel_id":"2"',
  );
  expect(operations).toEqual([{ action: "server.info" }]);
});

it("runs a background subagent on the model and thinking its call chooses", async () => {
  const { answers, requests } = scripted({ choices: { model: "faux/reasoner", thinking: "low" } });
  const agent = await fixture({ defaults: ["subagents"], answers });
  expect(await agent.ask("turn-1", "Delegate.")).toMatch(
    /^Tool: The subagent runs faux\/reasoner with thinking low, at standard speed\. Subagent \d+ is working/u,
  );
  expect(await agent.outcome("opened-1")).toMatchObject({ output: "Reported: 2, 3 and 5." });
  expect(requests).toEqual([{ model: "faux/reasoner", thinking: "low", payload: { input: [] } }]);
});

it("moves a thinking level the subagent's model lacks to the nearest it has, and says so", async () => {
  const { answers, requests } = scripted({ choices: { model: "faux/reasoner", thinking: "max" } });
  const agent = await fixture({ defaults: ["subagents"], answers });
  expect(await agent.ask("turn-1", "Delegate and wait.")).toBe(
    "Tool: The subagent runs faux/reasoner with thinking high (faux/reasoner does not support thinking max), at standard speed. 2, 3 and 5.",
  );
  expect(requests).toEqual([{ model: "faux/reasoner", thinking: "high", payload: { input: [] } }]);
});

it("refuses a model Pi does not offer, naming those it does", async () => {
  const { answers, requests } = scripted({ choices: { model: "nowhere/none" } });
  const agent = await fixture({ defaults: ["subagents"], answers });
  const said = await agent.ask("turn-1", "Delegate and wait.");
  expect(said).toContain("[error] Pi offers no model nowhere/none. Choose one of: ");
  expect(said).toContain("faux/scripted");
  expect(said).toContain("faux/reasoner");
  expect(requests).toEqual([]);
});

it("asks OpenAI's Responses API for priority processing for a fast subagent, and only then", async () => {
  for (const speed of ["standard", "fast"]) {
    const { answers, requests } = scripted({ choices: { model: "openai-test/gpt", speed } });
    const agent = await fixture({ defaults: ["subagents"], answers });
    responsesModel(agent, answers);
    expect(await agent.ask("turn-1", "Delegate and wait.")).toBe(
      `Tool: The subagent runs openai-test/gpt with thinking off, at ${speed} speed. 2, 3 and 5.`,
    );
    expect(requests).toEqual([
      {
        model: "openai-test/gpt",
        thinking: "off",
        payload: speed === "fast" ? { input: [], service_tier: "priority" } : { input: [] },
      },
    ]);
  }
});

it("refuses a fast subagent on a model whose API has no service tier", async () => {
  const { answers, requests } = scripted({ choices: { speed: "fast" } });
  const agent = await fixture({ defaults: ["subagents"], answers });
  expect(await agent.ask("turn-1", "Delegate and wait.")).toContain(
    `[error] faux/scripted runs the ${agent.model.api} API, which has no faster service tier; only OpenAI's Responses APIs do.`,
  );
  expect(requests).toEqual([]);
});

it("keeps a subagent on its own model while its parent's turns move the parent to another", async () => {
  const pause = pauseTool();
  const requests: ChildRequest[] = [];
  const model: FauxResponseFactory = async (context, options, _state, definition) => {
    const last = context.messages.findLast((message) => message.role !== "system");
    const said = textsOf(last).at(-1) ?? "";
    const first = textsOf(context.messages.find((message) => message.role === "user"))[0];
    if (first === TASK) {
      requests.push(await requestOf(options, definition));
      return last?.role === "toolResult"
        ? fauxAssistantMessage("2, 3 and 5.")
        : fauxAssistantMessage(fauxToolCall("pause", {}), { stopReason: "toolUse" });
    }
    if (said === "Delegate.")
      return fauxAssistantMessage(
        fauxToolCall("subagent", { task: TASK, model: "faux/reasoner", thinking: "low" }),
        { stopReason: "toolUse" },
      );
    if (last?.role === "toolResult") return fauxAssistantMessage("Started.");
    return fauxAssistantMessage(`${definition.id} thinking ${options?.reasoning ?? "off"}`);
  };
  const agent = await fixture({
    builtIns: [pause.extension],
    defaults: ["subagents", "pause"],
    answers: Array.from({ length: 20 }, () => model),
  });
  expect(await agent.ask("turn-1", "Delegate.")).toBe("Started.");
  await pause.started;
  const subject = { kind: "agent", agent_id: "default" } as const;
  await agent.set(subject, "model", "faux/reasoner");
  await agent.set(subject, "model.thinking", "high");
  expect(await agent.ask("turn-2", "hello")).toBe("reasoner thinking high");
  pause.release();
  expect(await agent.outcome("opened-1")).toMatchObject({ outcome: "answered" });
  expect(requests).toEqual([
    { model: "faux/reasoner", thinking: "low", payload: { input: [] } },
    { model: "faux/reasoner", thinking: "low", payload: { input: [] } },
  ]);
});

it("answers a waited-for subagent's call once, from the child it chose, across a restart", async () => {
  const child = Promise.withResolvers<void>();
  const { answers, asked, requests } = scripted({
    child: child.promise,
    choices: { model: "faux/reasoner", thinking: "low" },
  });
  let agent = await fixture({ durable: true, defaults: ["subagents"], answers });
  await agent.runtime.turns.submit(agent.turn("turn-1", "Delegate and wait."));
  await asked;
  agent = await agent.restart();
  fixtures.splice(0, 1, agent);
  child.resolve();
  expect(await agent.outcome("turn-1")).toMatchObject({
    outcome: "answered",
    output:
      "Tool: The subagent runs faux/reasoner with thinking low, at standard speed. 2, 3 and 5.",
  });
  expect(requests).not.toHaveLength(0);
  for (const request of requests)
    expect(request).toEqual({ model: "faux/reasoner", thinking: "low", payload: { input: [] } });
});
