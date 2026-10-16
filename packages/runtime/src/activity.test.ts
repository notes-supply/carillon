import { ACTIVITY_REPORT_LIMIT, NOTE_LENGTH, type WorkItem } from "@carillon/protocol";
import { expect, it } from "vitest";
import { commandHeader, describe, finalReport, keepNote, summarizeWork } from "./activity.ts";

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

it("orders work live first and counts what the snapshot leaves out exactly", () => {
  const item = (id: string, state: WorkItem["state"]): WorkItem => ({
    id,
    kind: "delegation",
    label: id,
    state,
  });
  expect(
    summarizeWork(
      [
        item("done", "succeeded"),
        item("lost", "unknown"),
        item("live", "running"),
        item("next", "queued"),
      ],
      false,
    ),
  ).toEqual({
    items: [
      item("live", "running"),
      item("next", "queued"),
      item("lost", "unknown"),
      item("done", "succeeded"),
    ],
    partial: false,
  });
  const many = Array.from({ length: 20 }, (_, index) => item(`w${String(index)}`, "running"));
  const summary = summarizeWork(many, true);
  expect(summary.items).toHaveLength(16);
  expect(summary).toMatchObject({ partial: true, omitted: 4 });
});

it("keeps the latest notes by native identity, updated in place and bounded", () => {
  const notes = new Map<string, string>();
  for (let index = 1; index <= 9; index += 1)
    keepNote(notes, `item-${String(index)}`, `thought ${String(index)}`);
  expect([...notes.keys()]).toEqual(
    [2, 3, 4, 5, 6, 7, 8, 9].map((index) => `item-${String(index)}`),
  );
  // Streamed text replaces its own note without moving it.
  keepNote(notes, "item-2", "thought 2,\nnow longer");
  expect([...notes.entries()][0]).toEqual(["item-2", "thought 2, now longer"]);
  // Redacted, one line, cut at the limit from the beginning; nothing for blank text.
  keepNote(notes, "item-9", `Using token=sk-${"a".repeat(40)} then ${"word ".repeat(200)}`);
  const long = notes.get("item-9") ?? "";
  expect(long).toMatch(/^Using token=\[redacted\] then word/u);
  expect(long.length).toBeLessThanOrEqual(NOTE_LENGTH);
  expect(long.endsWith("…")).toBe(true);
  keepNote(notes, "item-10", " \n ");
  expect(notes.has("item-10")).toBe(false);
});

it("bounds a final report's serialized size, dropping notes, then finished work, then old calls", () => {
  const small = {
    phase: "model" as const,
    running: [],
    recent: [{ tool: "read", detail: "a.txt", ok: true }],
    tool_calls: 1,
    notes: ["short"],
  };
  expect(finalReport(small)).toEqual(small);
  // Three bytes a character: the counts are bounded, but not the bytes.
  const wide = (length: number) => "語".repeat(length);
  const item = (index: number, state: WorkItem["state"]): WorkItem => ({
    id: `node:${String(index)}`,
    kind: "delegation",
    label: wide(200),
    state,
    actor: wide(100),
  });
  const big = {
    phase: "tools" as const,
    headline: wide(100),
    running: [{ tool: "bash", detail: "make", started_at: 1 }],
    recent: Array.from({ length: 5 }, (_, index) => ({
      tool: "read",
      detail: `${wide(100)}${String(index)}`,
      ok: true,
    })),
    tool_calls: 40,
    notes: Array.from({ length: 8 }, () => wide(500)),
    work: summarizeWork(
      Array.from({ length: 16 }, (_, index) => item(index, index < 2 ? "running" : "succeeded")),
      false,
    ),
  };
  const report = finalReport(big);
  expect(Buffer.byteLength(JSON.stringify(report))).toBeLessThanOrEqual(ACTIVITY_REPORT_LIMIT);
  expect(report.partial).toBe(true);
  expect(report.notes).toBeUndefined();
  expect(report.recent).toEqual(big.recent);
  expect(report.running).toEqual(big.running);
  expect(report.headline).toBe(big.headline);
  expect(report.tool_calls).toBe(40);
  const kept = report.work?.items ?? [];
  expect(kept.slice(0, 2).map((work) => work.state)).toEqual(["running", "running"]);
  expect(report.work?.omitted).toBeGreaterThan(0);
  expect(kept.length + (report.work?.omitted ?? 0)).toBe(16);
});

it("keeps live work and running calls longest when only they are left to trim", () => {
  const wide = "語".repeat(200);
  const live: WorkItem[] = Array.from({ length: 16 }, (_, index) => ({
    id: `node:live:${String(index)}`,
    kind: "delegation",
    label: wide,
    state: "running",
    actor: wide,
  }));
  const report = finalReport({
    phase: "tools",
    headline: wide,
    running: Array.from({ length: 8 }, (_, index) => ({
      tool: "bash",
      detail: `${wide}${String(index)}`.slice(0, 120),
      started_at: index,
    })),
    recent: [{ tool: "read", detail: wide.slice(0, 120), ok: true }],
    tool_calls: 9,
    notes: [wide],
    work: summarizeWork(live, false),
  });
  expect(Buffer.byteLength(JSON.stringify(report))).toBeLessThanOrEqual(ACTIVITY_REPORT_LIMIT);
  expect(report.partial).toBe(true);
  expect(report.recent).toEqual([]);
  expect(report.headline).toBeUndefined();
  expect(report.running).toHaveLength(8);
  expect((report.work?.items.length ?? 0) + (report.work?.omitted ?? 0)).toBe(16);
  expect(report.work?.items.length).toBeGreaterThan(0);
});
