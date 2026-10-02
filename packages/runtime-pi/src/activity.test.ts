import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ModelRuntime, SettingsManager } from "@earendil-works/pi-coding-agent";
import {
  createAssistantMessageEventStream,
  type AssistantMessage,
  type Model,
  type ToolCall,
} from "@earendil-works/pi-ai";
import type { TurnActivity } from "@carillon/protocol";
import { TurnActivitySchema } from "@carillon/protocol";
import { Check } from "typebox/value";
import { afterEach, expect, it, vi } from "vitest";
import { createPiRuntime } from "@carillon/runtime-pi";
import { commandHeader, describe } from "./activity.ts";

afterEach(() => {
  vi.restoreAllMocks();
});

it.each([
  ["cargo test -p carillon-discord --all-targets", "cargo test -p carillon-discord --all-targets"],
  ["  git log --oneline -5\n", "git log --oneline -5"],
  ['rg -n "fn delivery_loop" crates/', 'rg -n "fn delivery_loop" crates/'],
  ["ls -la ~/projects && du -sh ~/projects/*", "ls -la ~/projects && du -sh ~/projects/*"],
  ["systemctl --user restart carillon.target", "systemctl --user restart carillon.target"],
  ["set -e; [ -e out ] || grep -e TODO -r src", "set -e; [ -e out ] || grep -e TODO -r src"],
  ["cat My\\ Notes.txt | wc -l", "cat My\\ Notes.txt | wc -l"],
  ['git commit -m "tidy the card text"', 'git commit -m "tidy the card text"'],
])("shows an ordinary command as the model wrote it: %j", (command, shown) => {
  expect(commandHeader(command)).toBe(shown);
});

it.each([
  // Bodies and later lines are omitted, not shortened.
  ["cat > notes.txt <<'EOF'\nthe body\nEOF", "cat > notes.txt …", "the body"],
  ["cd /srv/app\nexport TOKEN=hunter2\n./deploy", "cd /srv/app …", "hunter2"],
  ["python3 <<<'print(1)'", "python3 …", "print"],
  // Inline scripts, substitutions and long payloads stop the preview.
  ["python3 -c 'import os; print(os.environ[\"KEY\"])'", "python3 -c …", "environ"],
  ['bash -c "curl -s localhost"', "bash -c …", "curl"],
  ["export KEY=$(cat ~/.secret)", "export KEY=[redacted]", "secret"],
  ["cd app && bash -lc 'make deploy'", "cd app && bash -lc …", "deploy"],
  ["perl -i -pe 's/a/b/' notes.txt", "perl -i -pe …", "s/a/b/"],
  [`python3 -c"print('private body')"`, "python3 -c …", "private"],
  ["node -e'process.exit(1)' && ruby -e\"puts 1\"", "node -e …", "exit"],
  ["echo `whoami`", "echo …", "whoami"],
  [`curl -d '{"message":"${"x".repeat(80)}"}' https://api.example.invalid`, "curl -d …", "message"],
  // Credentials in the header are redacted before any cut.
  [
    'curl -H "Authorization: Bearer abc123" "https://api.example.invalid/v1/items?token=short"',
    "curl -H",
    "abc123",
  ],
  ["mysql -u admin --password hunter2 app", "mysql -u admin --password …", "hunter2"],
  ["curl -u admin:hunter2 https://example.invalid", "curl -u …", "hunter2"],
  ["PGPASSWORD=hunter2 psql app", "PGPASSWORD=[redacted] psql app", "hunter2"],
  [`deploy --api-key ${"a1B2".repeat(10)} --region eu`, "deploy --api-key …", "a1B2"],
  ["git clone https://user:hunter2@example.invalid/repo.git", "git clone https://", "hunter2"],
  // A pasted one-line script shows only its program.
  [`node ${"x".repeat(500)}`, "node", "xxx"],
])("withholds the risky part of %j", (command, shown, hidden) => {
  const preview = commandHeader(command) ?? "";
  expect(preview.startsWith(shown), preview).toBe(true);
  expect(preview).not.toContain(hidden);
  expect(preview.length).toBeLessThanOrEqual(122);
});

// Synthetic secrets whose parts are separated by quoted or escaped spaces:
// no part of the value may appear, whichever syntax carries it.
it.each([
  ["curl --password 'alpha bravo' https://example.invalid", "curl --password …"],
  ["curl --cookie 'alpha bravo' https://example.invalid", "curl --cookie …"],
  ['mysql --password "alpha bravo" app', "mysql --password …"],
  ["mysql --password alpha\\ bravo app", "mysql --password …"],
  ["curl --password='alpha bravo' https://example.invalid", "curl --password …"],
  ["curl --password=alpha\\ bravo https://example.invalid", "curl --password …"],
  ["curl -u 'admin:alpha bravo' https://example.invalid", "curl -u …"],
  [
    "curl --user=admin:alpha\\ bravo https://example.invalid",
    "curl --user=[redacted] https://example.invalid",
  ],
  ["TOKEN=alpha\\ bravo make deploy", "TOKEN=[redacted] make deploy"],
  ['TOKEN="alpha bravo $(date)" make deploy', "TOKEN=[redacted] make deploy"],
  ["curl -H 'X-Api-Key: alpha bravo' https://example.invalid", "curl -H …"],
  ['curl -H "Authorization: Bearer alpha bravo" https://example.invalid', "curl -H …"],
  ['curl --user "admin:alpha bravo" https://example.invalid', "curl --user …"],
  // Substitution syntax is literal inside single quotes: the value is redacted whole.
  [
    "AUTHORIZATION='alpha bravo$(printf charlie)' curl https://example.invalid",
    "AUTHORIZATION=[redacted] curl https://example.invalid",
  ],
  // A quoted credential crossing the first line is redacted before the line is taken.
  ["TOKEN='alpha\nbravo' make deploy", "TOKEN=[redacted] make deploy"],
])("withholds every word of a spaced secret in %j", (command, shown) => {
  const preview = commandHeader(command);
  expect(preview).toBe(shown);
  for (const part of ["alpha", "bravo"]) expect(preview).not.toContain(part);
});

it("shows nothing for a comment, an empty command or a non-string", () => {
  for (const command of ["# rotate the key: hunter2\nrotate", " \n ", undefined, 42])
    expect(commandHeader(command)).toBeUndefined();
});

it("describes tool targets without exposing contents or arbitrary arguments", () => {
  const workspace = "/home/owner/workspace";
  expect(describe("read", { path: "/home/owner/workspace/src/main.rs" }, workspace)).toBe(
    "src/main.rs",
  );
  expect(describe("write", { path: "/etc/hosts", content: "secret body" }, workspace)).toBe(
    "/etc/hosts",
  );
  expect(
    describe("edit", { path: "a.ts", edits: [{ oldText: "x", newText: "y" }] }, workspace),
  ).toBe("a.ts");
  expect(describe("grep", { pattern: "TODO", path: "src" }, workspace)).toBe('"TODO" in src');
  expect(describe("find", { pattern: "*.md" }, workspace)).toBe("*.md");
  expect(describe("grep", { pattern: "api_key=sk-live-123" }, workspace)).toBe(
    '"api_key=[redacted]',
  );
  expect(
    describe(
      "discord_messages",
      {
        operation: {
          action: "messages.send",
          channel_id: "300",
          content: "@everyone private text",
          mention_users: ["200"],
        },
      },
      workspace,
    ),
  ).toBe("messages.send channel 300");
  expect(
    describe(
      "discord_members",
      { operation: { action: "members.role", user_id: "2", role_id: "3" } },
      workspace,
    ),
  ).toBe("members.role user 2 role 3");
  for (const args of [
    { query: "private question" },
    { operation: { action: "<b>" } },
    "text",
    null,
  ])
    expect(describe("web_search", args, workspace)).toBeUndefined();
  expect(describe("read", { path: 7 }, workspace)).toBeUndefined();
});

const model: Model<"openai-responses"> = {
  id: "fixture",
  name: "Fixture",
  api: "openai-responses",
  provider: "openai",
  baseUrl: "https://example.invalid",
  reasoning: false,
  input: ["text"],
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  contextWindow: 16_000,
  maxTokens: 100,
};

function assistant(content: AssistantMessage["content"]): AssistantMessage {
  return {
    role: "assistant",
    content,
    api: model.api,
    provider: model.provider,
    model: model.id,
    usage: {
      input: 0,
      output: 0,
      cacheRead: 0,
      cacheWrite: 0,
      totalTokens: 0,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
    },
    stopReason: content.some((block) => block.type === "toolCall") ? "toolUse" : "stop",
    timestamp: Date.now(),
  };
}

/** A model response that starts only when `gate` resolves, streaming its text before its tool calls. */
function scripted(gate: Promise<unknown>, text: string, calls: ToolCall[]) {
  const message = assistant([{ type: "text", text }, ...calls]);
  const stream = createAssistantMessageEventStream();
  void gate.then(() => {
    stream.push({ type: "start", partial: message });
    stream.push({ type: "text_start", contentIndex: 0, partial: message });
    stream.push({ type: "text_delta", contentIndex: 0, delta: text, partial: message });
    stream.push({ type: "text_end", contentIndex: 0, content: text, partial: message });
    stream.push({
      type: "done",
      reason: calls.length > 0 ? "toolUse" : "stop",
      message,
    });
  });
  return stream;
}

async function piRuntime(root: string) {
  const models = await ModelRuntime.create({
    authPath: join(root, "auth.json"),
    modelsPath: null,
    modelsStorePath: join(root, "models.json"),
    allowModelNetwork: false,
    refreshOnCreate: false,
  });
  const catalogue = vi.spyOn(models, "getAvailable").mockResolvedValue([model]);
  vi.spyOn(models, "getAuth").mockResolvedValue({ auth: { apiKey: "test-only" } });
  vi.spyOn(models, "hasConfiguredAuth").mockReturnValue(true);
  const runtime = await createPiRuntime({
    workspace: root,
    stateDirectory: join(root, "state"),
    modelRuntime: models,
    settings: SettingsManager.inMemory({ defaultProvider: model.provider, defaultModel: model.id }),
  });
  return { models, catalogue, runtime };
}

const request = (root: string, correlation_id: string) => ({
  correlation_id,
  interface_id: "discord-private",
  agent_id: "default",
  audience_id: "conversation",
  input: "check the workspace",
  resource_root: join(root, "resources"),
  attachment_root: join(root, "attachments"),
});

it("reports a real turn's phases, slow and parallel tools, failures and bursts until it settles", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-activity-"));
  await mkdir(join(root, "resources"));
  await writeFile(join(root, "notes.txt"), "notes\n");
  const { models, runtime } = await piRuntime(root);
  const firstResponse = Promise.withResolvers<undefined>();
  const finalResponse = Promise.withResolvers<undefined>();
  const wait = (name: string) => `until [ -e ${name} ]; do sleep 0.02; done`;
  const reads = Array.from({ length: 10 }, (_, index) => ({
    type: "toolCall" as const,
    id: `read-${String(index)}`,
    name: "read",
    arguments: { path: join(root, "notes.txt") },
  }));
  let calls = 0;
  vi.spyOn(models, "streamSimple").mockImplementation(() => {
    calls += 1;
    return calls === 1
      ? scripted(firstResponse.promise, "Checking the workspace first.\nThen summarising.", [
          { type: "toolCall", id: "slow", name: "bash", arguments: { command: wait("one") } },
          { type: "toolCall", id: "slower", name: "bash", arguments: { command: wait("two") } },
          { type: "toolCall", id: "missing", name: "read", arguments: { path: "missing.txt" } },
          ...reads,
        ])
      : scripted(finalResponse.promise, "All done.", []);
  });
  const activity = () => runtime.activity("turn-1", "discord-private");
  try {
    const turn = runtime.executeTurn(request(root, "turn-1"));
    // Model-only work: nothing running and no commentary yet.
    await expect.poll(() => activity()?.phase).toBe("model");
    expect(activity()).toEqual({ phase: "model", running: [], recent: [], tool_calls: 0 });
    // Other interfaces and other turns are not described.
    expect(runtime.activity("turn-1", "other-interface")).toBeUndefined();
    expect(runtime.activity("turn-2", "discord-private")).toBeUndefined();

    firstResponse.resolve(undefined);
    await expect.poll(() => activity()?.running.length).toBe(2);
    await expect.poll(() => activity()?.recent.length).toBe(5);
    const working = activity();
    expect(Check(TurnActivitySchema, working)).toBe(true);
    expect(working).toMatchObject({
      phase: "tools",
      headline: "Checking the workspace first.",
      running: [
        { tool: "bash", detail: wait("one") },
        { tool: "bash", detail: wait("two") },
      ],
      tool_calls: 13,
    });
    // The failed call was recorded before the burst pushed it out of the recent list.
    expect(working?.recent).toEqual(
      Array.from({ length: 5 }, () => ({ tool: "read", detail: "notes.txt", ok: true })),
    );
    expect(JSON.stringify(working)).not.toContain("notes\\n");

    await writeFile(join(root, "one"), "");
    await expect.poll(() => activity()?.running.length).toBe(1);
    expect(activity()?.recent.at(-1)).toEqual({ tool: "bash", detail: wait("one"), ok: true });
    await writeFile(join(root, "two"), "");
    // Model work after the tools: nothing is shown as still executing.
    await expect.poll(() => activity()?.running.length).toBe(0);
    expect(activity()?.phase).toBe("model");

    finalResponse.resolve(undefined);
    expect((await turn).output).toContain("All done.");
    expect(activity()).toBeUndefined();
  } finally {
    firstResponse.resolve(undefined);
    finalResponse.resolve(undefined);
    await runtime.close();
    await rm(root, { recursive: true, force: true });
  }
});

it("records a failed tool and the call that recovers from it", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-activity-"));
  await mkdir(join(root, "resources"));
  await writeFile(join(root, "notes.txt"), "notes\n");
  const { models, runtime } = await piRuntime(root);
  const hold = Promise.withResolvers<undefined>();
  const snapshots: (TurnActivity | undefined)[] = [];
  let calls = 0;
  vi.spyOn(models, "streamSimple").mockImplementation(() => {
    calls += 1;
    snapshots.push(runtime.activity("turn-1", "discord-private"));
    const read = (id: string, path: string): ToolCall => ({
      type: "toolCall",
      id,
      name: "read",
      arguments: { path },
    });
    if (calls === 1) return scripted(Promise.resolve(), "Reading.", [read("a", "missing.txt")]);
    if (calls === 2) return scripted(Promise.resolve(), "Retrying.", [read("b", "notes.txt")]);
    return scripted(hold.promise, "Done.", []);
  });
  try {
    const turn = runtime.executeTurn(request(root, "turn-1"));
    await expect.poll(() => calls).toBe(3);
    expect(runtime.activity("turn-1", "discord-private")).toMatchObject({
      phase: "model",
      headline: "Retrying.",
      running: [],
      recent: [
        { tool: "read", detail: "missing.txt", ok: false },
        { tool: "read", detail: "notes.txt", ok: true },
      ],
      tool_calls: 2,
    });
    expect(snapshots[1]?.recent).toEqual([{ tool: "read", detail: "missing.txt", ok: false }]);
    hold.resolve(undefined);
    await turn;
  } finally {
    hold.resolve(undefined);
    await runtime.close();
    await rm(root, { recursive: true, force: true });
  }
});

it("removes a turn's activity when preparation fails before its session exists", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-activity-"));
  await mkdir(join(root, "resources"));
  const { catalogue, runtime } = await piRuntime(root);
  const loading = Promise.withResolvers<readonly Model<typeof model.api>[]>();
  catalogue.mockReturnValue(loading.promise);
  try {
    const turn = runtime.executeTurn(request(root, "turn-1"));
    const failed = expect(turn).rejects.toThrow("catalogue unavailable");
    await expect
      .poll(() => runtime.activity("turn-1", "discord-private"))
      .toEqual({
        phase: "starting",
        running: [],
        recent: [],
        tool_calls: 0,
      });
    loading.reject(new Error("catalogue unavailable"));
    await failed;
    expect(runtime.activity("turn-1", "discord-private")).toBeUndefined();
  } finally {
    await runtime.close();
    await rm(root, { recursive: true, force: true });
  }
});
