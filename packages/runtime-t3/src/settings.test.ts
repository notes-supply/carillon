import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, it } from "vitest";
import { profileIds } from "./profiles.ts";
import { loadSettings } from "./settings.ts";

const directories: string[] = [];
afterEach(async () => {
  await Promise.all(directories.splice(0).map((path) => rm(path, { recursive: true })));
});

/** A settings file naming `url`, and the installation it belongs to. */
async function settings(url: string): Promise<{ path: string; root: string }> {
  const root = await mkdtemp(join(tmpdir(), "carillon-t3-settings-"));
  directories.push(root);
  const path = join(root, "runtime-t3.toml");
  await writeFile(
    path,
    [
      `url = ${JSON.stringify(url)}`,
      'token_file = "credentials/t3.token"',
      "[agents.code]",
      'project_id = "project"',
      "[agents.code.model]",
      'instance_id = "claudeAgent"',
      'model = "claude-fable-5-1"',
    ].join("\n"),
  );
  return { path, root };
}

it("takes T3's address only as an HTTP origin, before anything connects or logs it", async () => {
  for (const url of [
    "https://t3.example#token-value",
    "https://t3.example?token=token-value",
    "https://user:token-value@t3.example",
    "https://t3.example/token-value",
    "ftp://t3.example",
    "t3.example",
  ]) {
    const { path, root } = await settings(url);
    const refusal = await loadSettings(path, root).then(
      () => undefined,
      (failure: unknown) => (failure instanceof Error ? failure.message : String(failure)),
    );
    expect(refusal, url).toMatch(/\/url must be an HTTP or HTTPS origin/u);
    expect(refusal, url).not.toContain("token-value");
  }
  for (const [url, origin] of [
    ["http://127.0.0.1:3773", "http://127.0.0.1:3773"],
    ["https://t3.example", "https://t3.example"],
    ["https://t3.example:8443", "https://t3.example:8443"],
    ["http://[::1]:3773", "http://[::1]:3773"],
    ["https://t3.example/", "https://t3.example"],
  ] as const) {
    const { path, root } = await settings(url);
    expect((await loadSettings(path, root)).url, url).toBe(origin);
  }
});

it("reads the profiles and each agent's settings, refusing a repeated or unknown profile or an agent ID outside the grammar", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-t3-settings-"));
  directories.push(root);
  const path = join(root, "runtime-t3.toml");
  const agent = (id: string, instance: string) => [
    `[agents.${id}]`,
    'project_id = "project"',
    `[agents.${id}.model]`,
    `instance_id = "${instance}"`,
    'model = "m"',
  ];
  const file = (...lines: string[]) =>
    writeFile(
      path,
      ['url = "http://127.0.0.1:3773"', 'token_file = "credentials/t3.token"', ...lines].join("\n"),
    );
  await file(
    `profiles = ${JSON.stringify(profileIds())}`,
    ...agent("assistant", "carillon_codex"),
    ...agent("code", "claudeAgent"),
  );
  const loaded = await loadSettings(path, root);
  expect(loaded.profiles).toEqual(new Set(profileIds()));
  expect(loaded.agents.get("assistant")).toEqual({
    projectId: "project",
    selection: { instanceId: "carillon_codex", model: "m", options: [] },
  });
  for (const [lines, refusal] of [
    [['profiles = ["carillon_claude", "carillon_claude"]'], /\/profiles/u],
    [['profiles = ["carillon_claude", "carillon_claud"]'], /\/profiles/u],
    [agent('"not/an/agent"', "claudeAgent"), /\/agents/u],
  ] as const) {
    await file(...lines);
    await expect(loadSettings(path, root)).rejects.toThrow(refusal);
  }
});
