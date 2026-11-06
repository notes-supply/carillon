import {
  fauxAssistantMessage,
  fauxText,
  fauxThinking,
  fauxToolCall,
  type FauxResponseFactory,
} from "@earendil-works/pi-ai";
import { defineExtension, defineTool } from "@earendil-works/pi-durable";
import { spawnSync } from "node:child_process";
import { cp, mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { fileURLToPath } from "node:url";
import { Type } from "typebox";
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
    `SELECT doc.owner_id AS conversation, item.key, json_extract(item.value, '$.totalTokens') AS total,
       json_extract(item.value, '$.cacheRead') AS cache_read,
       json_extract(item.value, '$.input') + json_extract(item.value, '$.cacheRead')
         + json_extract(item.value, '$.cacheWrite') AS prompt
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
  let cacheRead = 0;
  let prompt = 0;
  for (const item of items) {
    const tokens = Number(item["total"]);
    add(byKey, String(item["key"]), tokens);
    add(byConversation, Number(item["conversation"]), tokens);
    total += tokens;
    cacheRead += Number(item["cache_read"]);
    prompt += Number(item["prompt"]);
  }
  return { byKey, byConversation, total, cacheRead, prompt };
}

const sumOf = (rows: Row[], column: string) =>
  rows.reduce((sum, row) => sum + Number(row[column]), 0);

const search = (...terms: string[]) =>
  query("WITH search").replace("('invoice')", terms.map((term) => `('${term}')`).join(", "));

function windowsOf(agent: Fixture, terms: string[]) {
  const said = history(
    agent,
    "SELECT conversation, position, entry, text FROM messages ORDER BY conversation, position",
  );
  const matches = (row: Row) =>
    terms.some((term) => (row["text"] ?? "").toLowerCase().includes(term.toLowerCase()));
  const latest = new Map<string, number>();
  for (const row of said)
    if (matches(row)) latest.set(row["conversation"] ?? "", Number(row["entry"]));
  return said
    .filter((row, index) =>
      [said[index - 1], row, said[index + 1]].some(
        (near) =>
          near !== undefined && near["conversation"] === row["conversation"] && matches(near),
      ),
    )
    .toSorted(
      (a, b) =>
        (latest.get(b["conversation"] ?? "") ?? 0) - (latest.get(a["conversation"] ?? "") ?? 0),
    )
    .map((row) => [row["conversation"], row["position"], matches(row) ? ">" : ""]);
}

const windows = (rows: Row[]) =>
  rows.map((row) => [row["conversation"], row["position"], row["hit"]]);

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
  const { byKey, byConversation, total, cacheRead, prompt } = durableTotals(agent);
  expect(byKey.get("estimate")).toBe(1000);
  expect(cacheRead).toBeGreaterThan(0);
  const [conversation] = byConversation.keys();
  const responses = String(
    read(agent, "SELECT 1 FROM entries WHERE json_extract(record, '$.kind') = 'pi.assistant'")
      .length,
  );
  const rate = String(Math.round((1000 * cacheRead) / prompt) / 10);
  expect(history(agent, query("cache_hit_pct"))).toEqual([
    { conversation: String(conversation), responses, total: String(total), cache_hit_pct: rate },
    { conversation: "all", responses, total: String(total), cache_hit_pct: rate },
  ]);
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
  const [{ subagent = "" } = {}] = read(
    agent,
    "SELECT id AS subagent FROM conversations WHERE owner_conversation_id IS NOT NULL",
  );
  expect(history(agent, query("WITH search"))).toMatchObject([
    { conversation, position: "1", subagent: "", hit: ">", text: "Check the invoices." },
    {
      conversation,
      position: "2",
      subagent: String(subagent),
      hit: ">",
      text: "Find invoice K-729.",
    },
    { conversation, position: "3", subagent: String(subagent), hit: "", text: "It is due Friday." },
  ]);
  expect(history(agent, query("GROUP BY conversation"))).toMatchObject([
    { conversation, total: String(durableTotals(agent).total) },
  ]);
});

it("shows each match with its neighbours, once, from a conversation's start to its end", async () => {
  const late = `${"Long. ".repeat(60)}Prune kiwi in winter.`;
  const agent = await fixture({
    durable: true,
    answers: ["In spring.", "Plant in May.", "Kiwi vines want frost cover.", late].map((text) =>
      fauxAssistantMessage(text),
    ),
  });
  for (const [turn, said] of ["When is kiwi season?", "Tomatoes?", "Thanks.", "Noted."].entries())
    await agent.ask(`turn-${String(turn)}`, said);
  const found = history(agent, search("KIWI"));
  expect(found.map(({ position, role, hit }) => [position, role, hit])).toEqual([
    ["1", "user", ">"],
    ["2", "assistant", ""],
    ["5", "user", ""],
    ["6", "assistant", ">"],
    ["7", "user", ""],
    ["8", "assistant", ">"],
  ]);
  expect(found.at(-1)?.["text"]).toContain("Prune kiwi in winter.");
});

it("shows a message matching any of several terms once, with its neighbours", async () => {
  const turns: [audience: string, said: string, answer: string][] = [
    ["billing", "Send the invoice with the billing note.", "Sent."],
    ["billing", "Thanks.", "The billing run is monthly."],
    ["billing", "Good.", "Invoices go out on the first."],
    ["billing", "And taxes?", "Taxes are separate."],
    ["garden", "When is kiwi season?", "In spring."],
    ["garden", "Is billing done?", "Yes, every invoice."],
    ["billing", "One more invoice question.", "Ask away."],
  ];
  const agent = await fixture({
    durable: true,
    answers: turns.map(([, , answer]) => fauxAssistantMessage(answer)),
  });
  for (const [turn, [audience, said]] of turns.entries())
    await agent.ask(`turn-${String(turn)}`, said, { audience_id: audience });
  const terms = ["invoice", "billing"];
  const found = history(agent, search(...terms));
  expect(windows(found)).toEqual(windowsOf(agent, terms));
});

it("says how many matches the default rows left out", async () => {
  const conversations = 12;
  const agent = await fixture({
    durable: true,
    answers: Array.from({ length: 2 * conversations }, () => fauxAssistantMessage("Noted.")),
  });
  for (let turn = 0; turn < 2 * conversations; turn += 1)
    await agent.ask(`turn-${String(turn)}`, `invoice ${String(turn)}`, {
      audience_id: `audience-${String(turn % conversations)}`,
    });
  const found = history(agent, search("invoice"));
  expect(windows(found.slice(0, -1))).toEqual(windowsOf(agent, ["invoice"]).slice(0, 40));
  expect(found.at(-1)).toMatchObject({
    conversation: "",
    hit: "more",
    text: "4 match(es) not shown, in 2 conversation(s)",
  });
});

it("counts only conversations with a left-out match, not one whose context alone was cut", async () => {
  const turns: [audience: string, said: string][] = [
    ["oldest", "invoice last"],
    ["split", "hello"],
    ["split", "invoice split"],
    ...Array.from({ length: 19 }, (_, index): [string, string] => [`a${String(index)}`, "invoice"]),
  ];
  const agent = await fixture({
    durable: true,
    answers: turns.map(() => fauxAssistantMessage("Noted.")),
  });
  for (const [turn, [audience, said]] of turns.entries())
    await agent.ask(`turn-${String(turn)}`, said, { audience_id: audience });
  expect(history(agent, search("invoice")).at(-1)).toMatchObject({
    hit: "more",
    text: "1 match(es) not shown, in 1 conversation(s)",
  });
});

it("opens each conversation with the person's first words, past Carillon's notes and quotes", async () => {
  const agent = await fixture({
    durable: true,
    answers: [fauxAssistantMessage("A file."), fauxAssistantMessage("Peppers too.")],
  });
  const attachments = join(agent.componentDirectory, "..", "attachments");
  await mkdir(join(attachments, "capture"), { recursive: true });
  await writeFile(join(attachments, "capture", "0"), "seeds");
  await agent.ask("turn-1", "", {
    attachment_root: attachments,
    attachments: [{ path: "capture/0", name: "seeds.txt", size: 5, sha256: "0".repeat(64) }],
  });
  await agent.ask(
    "turn-2",
    "In reply to your message, sent 2026-11-05T10:00:00Z:\n> A file.\n\nAnd what about peppers?\nThey sprout slowly.",
  );
  expect(history(agent, query("FROM conversation_list"))).toMatchObject([
    { opening: "And what about peppers? They sprout slowly." },
  ]);
});

it("ends an opening at the person's words, before a later note, past every Carillon quote", async () => {
  const opened = [
    'In reply to your message, sent 2026-11-05T10:00:00+00:00, with files ["seeds.txt"]:\n\nThe file reply.',
    'In reply to a message from agent "code", sent 2026-11-05T14:03:00+00:00 from <#300>, forwarding a message:\n> b\n>\n> c\n\nThe forwarded reply.',
    'In reply to a message from "Ada" (Discord user 202), sent 2026-11-05T14:00:00+00:00:\n> hi\n\nThe reply to Ada.',
    "In reply to a message from another agent, sent 2026-11-05T14:03:27+00:00:\n> b\n\nThe reply to another agent.",
    'Forwarding a message, sent 2026-11-04T09:00:00+00:00, with files ["notes.txt"]:\n\nThe forward.',
    "You sent them this direct message at 2026-11-05T14:03:00+00:00 from <#300>:\n> Hi\n\n" +
      'Agent "code" sent them this direct message at 2026-11-05T14:03:00+00:00:\n> Hello\n\n' +
      "Another agent sent them this direct message at 2026-11-05T14:03:00+00:00:\n\nThe direct reply.",
    "Here is my list:\n\n- tomatoes",
    "In reply to your message, sent now:\n\nA quote Carillon never writes.",
    "In reply to your message, sent 2 invoices:\n\nMy own words came first.",
    "You sent them this direct message at 3 pm:\n\nAlso mine.",
  ];
  const agent = await fixture({
    durable: true,
    answers: Array.from({ length: opened.length + 1 }, () => fauxAssistantMessage("Noted.")),
  });
  const attachments = join(agent.componentDirectory, "..", "attachments");
  await mkdir(join(attachments, "capture"), { recursive: true });
  await writeFile(join(attachments, "capture", "0"), "seeds");
  await agent.ask("attached", "Please check these seeds.", {
    audience_id: "attached",
    attachment_root: attachments,
    attachments: [{ path: "capture/0", name: "seeds.txt", size: 5, sha256: "0".repeat(64) }],
  });
  for (const [turn, said] of opened.entries())
    await agent.ask(`turn-${String(turn)}`, said, { audience_id: `audience-${String(turn)}` });
  expect(
    new Set(history(agent, query("FROM conversation_list")).map((row) => row["opening"])),
  ).toEqual(
    new Set([
      "Please check these seeds.",
      "The file reply.",
      "The forwarded reply.",
      "The reply to Ada.",
      "The reply to another agent.",
      "The forward.",
      "The direct reply.",
      "Here is my list:",
      "In reply to your message, sent now:",
      "In reply to your message, sent 2 invoices:",
      "You sent them this direct message at 3 pm:",
    ]),
  );
});

it("lists the conversations of over a thousand messages about as fast as it searches them", async () => {
  const turns = 600;
  const agent = await fixture({
    durable: true,
    answers: Array.from({ length: turns }, () => fauxAssistantMessage("No change.")),
  });
  for (let turn = 0; turn < turns; turn += 1)
    await agent.ask(
      `turn-${String(turn)}`,
      `${turn % 40 === 0 ? "invoice" : "Message"} ${String(turn)}`,
      {
        audience_id: `audience-${String(turn % 20)}`,
      },
    );
  const timed = (sql: string) => {
    const times = Array.from({ length: 5 }, () => {
      const start = performance.now();
      history(agent, sql);
      return performance.now() - start;
    }).toSorted((a, b) => a - b);
    return times[2] ?? Infinity;
  };
  expect(history(agent, query("FROM conversation_list"))).toHaveLength(20);
  expect(timed(query("FROM conversation_list"))).toBeLessThan(2 * timed(search("invoice")));
}, 60_000);

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
  const searched = search("tomatoes", "may");
  const found = [
    { position: "1", hit: ">", text: "When do tomatoes go out?" },
    { position: "2", hit: ">", text: "Plant in May." },
  ];
  expect(runDocumented(sqliteCommand, store, searched)).toMatchObject(found);
  expect(runDocumented(bunCommand, store, searched)).toMatchObject(found);
  const opening = [{ opening: "When do tomatoes go out?" }];
  expect(runDocumented(bunCommand, store, query("FROM conversation_list"))).toMatchObject(opening);
});
