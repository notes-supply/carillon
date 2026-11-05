import {
  fauxAssistantMessage,
  fauxText,
  fauxThinking,
  fauxToolCall,
  Type,
  type FauxResponseFactory,
} from "@earendil-works/pi-ai";
import { defineExtension, defineTool } from "@earendil-works/pi-durable";
import { spawnSync } from "node:child_process";
import { cp, mkdir, mkdtemp, readFile, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { fileURLToPath } from "node:url";
import { afterAll, afterEach, beforeAll, expect, it } from "vitest";
import { startFixture, textsOf, type Fixture } from "./fixture.ts";
import { BUILT_IN_SKILLS } from "./resources.ts";

const shipped = fileURLToPath(new URL("conversation-history", BUILT_IN_SKILLS));
const skill = await readFile(join(shipped, "SKILL.md"), "utf8");
const blocks = (language: string) =>
  [...skill.matchAll(/^```(\w+)\n([\s\S]*?)^```$/gmu)]
    .filter(([, fenced]) => fenced === language)
    .map(([, , body = ""]) => body);
const [sqliteCommand = "", bunCommand = ""] = blocks("sh");
const queries = blocks("sql")
  .flatMap((block) => block.split(/;\n\n/u))
  .map((sql) => sql.trim());
const query = (marker: string) => {
  const found = queries.find((sql) => sql.includes(marker));
  if (found === undefined) throw new Error(`SKILL.md has no query with ${marker}`);
  return found;
};

let home = "";
let skillDirectory = "";
beforeAll(async () => {
  home = join(await mkdtemp(join(tmpdir(), "carillon-history-")), "home with spaces");
  skillDirectory = join(home, "app", "lib", "pi-skills", "conversation-history");
  await cp(shipped, skillDirectory, { recursive: true });
});
afterAll(() => rm(join(home, ".."), { recursive: true, force: true }));

const fixtures: Fixture[] = [];
afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.close()));
});

async function fixture(...args: Parameters<typeof startFixture>) {
  const started = await startFixture(...args);
  fixtures.push(started);
  return started;
}

const storeOf = (agent: Fixture) => join(agent.componentDirectory, "agents", "default.sqlite");

type Row = Record<string, string>;

function runDocumented(command: string, store: string, sql: string): Row[] {
  const filled = command
    .replaceAll("<this skill's directory>", "$SKILL")
    .replaceAll("<store>", "$STORE")
    .replaceAll("<query>", "$QUERY");
  const result = spawnSync("sh", ["-c", filled], {
    encoding: "utf8",
    env: { ...process.env, SKILL: skillDirectory, STORE: store, QUERY: sql, CARILLON_HOME: home },
  });
  expect(result.status, result.stderr).toBe(0);
  const [header = [], ...rows] = result.stdout
    .split("\n")
    .filter((line) => line.startsWith("│"))
    .map((line) =>
      line
        .split("│")
        .slice(1, -1)
        .map((cell) => cell.trim()),
    );
  return rows.map((row) =>
    Object.fromEntries(header.map((name, index) => [name, row[index] ?? ""])),
  );
}

const history = (agent: Fixture, sql: string) => runDocumented(sqliteCommand, storeOf(agent), sql);

function read(agent: Fixture, sql: string) {
  const store = new DatabaseSync(storeOf(agent), { readOnly: true });
  try {
    return store.prepare(sql).all();
  } finally {
    store.close();
  }
}

function durableTotals(agent: Fixture) {
  const items = read(
    agent,
    `SELECT doc.owner_id AS conversation, item.key, json_extract(item.value, '$.totalTokens') AS total
     FROM documents AS doc
     JOIN document_revisions AS revision ON revision.document_id = doc.id
       AND revision.seq = (SELECT max(seq) FROM document_revisions WHERE document_id = doc.id)
     JOIN json_each(revision.content) AS bucket
     JOIN json_each(bucket.value) AS item
     WHERE json_extract(doc.record, '$.kind') = 'pi.usage'`,
  );
  const add = <K>(totals: Map<K, number>, key: K, tokens: number) =>
    totals.set(key, (totals.get(key) ?? 0) + tokens);
  const byKey = new Map<string, number>();
  const byConversation = new Map<number, number>();
  let total = 0;
  for (const item of items) {
    const tokens = Number(item["total"]);
    add(byKey, String(item["key"]), tokens);
    add(byConversation, Number(item["conversation"]), tokens);
    total += tokens;
  }
  return { byKey, byConversation, total };
}

const sumOf = (rows: Row[], column: string) =>
  rows.reduce((sum, row) => sum + Number(row[column]), 0);

it("reads a conversation's place, its words and its usage through the documented command", async () => {
  const agent = await fixture({
    durable: true,
    answers: [fauxAssistantMessage([fauxThinking("Private."), fauxText("Plant in May.")])],
  });
  const origin = { guild_id: "900", channel_id: "555", parent_channel_id: "100" };
  await agent.ask("turn-1", "When do tomatoes go out?", {
    audience_id: "discord-900-555",
    native: { origin },
  });
  const [conversation] = history(agent, query("FROM conversation_list"));
  expect(conversation).toMatchObject({
    guild: "900",
    channel: "555",
    thread_of: "100",
    messages: "1",
  });
  expect(
    history(
      agent,
      query("FROM messages WHERE conversation =").replace(
        "= 4",
        `= ${conversation?.["conversation"]}`,
      ),
    ),
  ).toMatchObject([
    { role: "user", text: "When do tomatoes go out?" },
    { role: "assistant", text: "Plant in May." },
  ]);
  const [usage] = history(agent, query("GROUP BY day, model"));
  expect(usage?.["model"]).toBe(`${agent.model.provider.id}/scripted`);
  expect(Number(usage?.["total"])).toBe(durableTotals(agent).total);
});

const estimate = defineExtension({
  name: "estimate",
  tools: [
    defineTool({
      name: "estimate",
      description: "Estimates.",
      parameters: Type.Object({}),
      execute: () =>
        Promise.resolve({
          output: [{ type: "text", text: "About a week." }],
          usage: {
            input: 900,
            output: 100,
            cacheRead: 0,
            cacheWrite: 0,
            totalTokens: 1000,
            cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
          },
        }),
    }),
  ],
});

it("counts compaction summaries and tools' model calls in every usage total, as pi-durable does", async () => {
  const agent = await fixture({
    durable: true,
    builtIns: [estimate],
    defaults: ["estimate"],
    answers: [
      fauxAssistantMessage(fauxToolCall("estimate", {}), { stopReason: "toolUse" }),
      fauxAssistantMessage("Long ".repeat(25_000)),
      fauxAssistantMessage([], { stopReason: "error", errorMessage: "prompt is too long" }),
      fauxAssistantMessage("The story so far."),
      fauxAssistantMessage("Answered after compacting."),
    ],
  });
  await agent.ask("turn-1", "hello");
  expect(await agent.ask("turn-2", "again")).toBe("Answered after compacting.");
  expect(
    read(agent, "SELECT 1 FROM entries WHERE json_extract(record, '$.kind') = 'pi.compaction'"),
  ).toHaveLength(1);
  const { byKey, byConversation, total } = durableTotals(agent);
  expect(byKey.get("estimate")).toBe(1000);
  const byDay = history(agent, query("GROUP BY day, model"));
  expect(sumOf(byDay, "total")).toBe(total);
  for (const [model, tokens] of byKey)
    expect(
      sumOf(
        byDay.filter((row) => row["model"] === model),
        "total",
      ),
    ).toBe(tokens);
  expect(
    history(agent, query("GROUP BY conversation")).map((row) => [
      Number(row["conversation"]),
      Number(row["total"]),
    ]),
  ).toEqual([...byConversation]);
});

it("counts subagent runs under the conversation that started them, and finds their tasks", async () => {
  const model: FauxResponseFactory = (context) => {
    const last = context.messages.findLast((message) => message.role !== "system");
    const said = textsOf(last).at(-1) ?? "";
    if (last?.role === "toolResult") return fauxAssistantMessage(`Tool: ${said}`);
    if (said === "Check the invoices.")
      return fauxAssistantMessage(
        fauxToolCall("subagent", { task: "Find invoice K-729.", wait: true }),
        { stopReason: "toolUse" },
      );
    if (said === "Find invoice K-729.") return fauxAssistantMessage("It is due Friday.");
    return fauxAssistantMessage(`Unexpected: ${said}`);
  };
  const agent = await fixture({
    durable: true,
    defaults: ["subagents"],
    answers: Array.from({ length: 10 }, () => model),
  });
  expect(await agent.ask("turn-1", "Check the invoices.")).toBe("Tool: It is due Friday.");
  const conversations = history(agent, query("FROM conversation_list"));
  expect(conversations).toMatchObject([{ messages: "1" }]);
  const [{ conversation = "" } = {}] = conversations;
  const found = history(agent, query("LIKE '%invoice%'"));
  expect(found.map(({ text }) => text)).toEqual(
    expect.arrayContaining(["Check the invoices.", "Find invoice K-729."]),
  );
  expect(found.every((row) => row["conversation"] === conversation)).toBe(true);
  expect(history(agent, query("GROUP BY conversation"))).toMatchObject([
    { conversation, total: String(durableTotals(agent).total) },
  ]);
});

it("runs the documented commands from a Carillon home whose path has spaces", async () => {
  const agent = await fixture({ durable: true, answers: [fauxAssistantMessage("Plant in May.")] });
  await agent.ask("turn-1", "When do tomatoes go out?");
  const store = join(home, "state", "components", "runtime-pi", "agents", "default.sqlite");
  await mkdir(join(store, ".."), { recursive: true });
  const live = new DatabaseSync(storeOf(agent), { readOnly: true });
  try {
    live.exec(`VACUUM INTO '${store}'`);
  } finally {
    live.close();
  }
  const bun = spawnSync("sh", ["-c", "command -v bun"], { encoding: "utf8" }).stdout.trim();
  await mkdir(join(home, "app", "bin"), { recursive: true });
  await symlink(bun, join(home, "app", "bin", "carillon-runtime-pi"));
  const sql = "SELECT role, text FROM messages ORDER BY entry";
  const said = [
    { role: "user", text: "When do tomatoes go out?" },
    { role: "assistant", text: "Plant in May." },
  ];
  expect(runDocumented(sqliteCommand, store, sql)).toEqual(said);
  expect(runDocumented(bunCommand, store, sql)).toMatchObject(said);
});
