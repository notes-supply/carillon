import { lstat, mkdir, readFile, readlink, readdir, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { expect, it } from "vitest";
import { profile } from "./setup.ts";
import { cli, fakeT3, installation, loaded, serve } from "./setup-fixtures.ts";

async function prepared() {
  const fake = await fakeT3();
  const installed = await installation();
  const { root } = installed;
  const home = join(root, "t3-home");
  fake.instances.set("carillon_claude", { driver: "claudeAgent", config: { homePath: home } });
  fake.instances.set("carillon_codex", {
    driver: "codex",
    config: { homePath: join(root, "codex-home") },
  });
  await writeFile(join(root, "credentials", "t3.token"), fake.token);
  await writeFile(
    join(root, "providers", "runtime-t3.toml"),
    [
      `url = "${fake.url}"`,
      'token_file = "credentials/t3.token"',
      'profiles = ["carillon_claude", "carillon_codex"]',
      "[agents.assistant]",
      'project_id = "agent"',
      "[agents.assistant.model]",
      'instance_id = "carillon_claude"',
      'model = "claude-sonnet-5"',
      "[agents.code]",
      'project_id = "agent"',
      "[agents.code.model]",
      'instance_id = "codex"',
      'model = "gpt-6-astra"',
    ].join("\n"),
  );
  return { ...installed, fake, home };
}

it("startup reconciles every saved profile at T3's homePath before serving", async () => {
  const { root, config, home } = await prepared();
  const resources = join(root, "agent");
  await mkdir(join(resources, "skills", "status"), { recursive: true });
  await writeFile(join(resources, "skills", "status", "SKILL.md"), "Status");
  const started = await serve(config);
  expect(started.status, started.stderr).toBe(0);
  expect(started.stdout).toContain("runtime-t3 ready");
  expect(await readlink(join(home, "CLAUDE.md"))).toBe(join(resources, "AGENTS.md"));
  expect(await readlink(join(home, "skills", "status"))).toBe(join(resources, "skills", "status"));
  expect(await readlink(join(root, "codex-home", "AGENTS.md"))).toBe(join(resources, "AGENTS.md"));
  expect(
    await lstat(join(root, "data", "carillon", "profiles")).catch(() => undefined),
  ).toBeUndefined();
});

it("check leaves another installation's live profile links untouched without reaching T3", async () => {
  const { root, config, home, fake } = await prepared();
  const defaultHome = join(root, "data", "carillon", "profiles", "claude");
  const live = join(root, "installation-a");
  for (const directory of [home, defaultHome]) {
    await mkdir(join(directory, "skills"), { recursive: true });
    await symlink(join(live, "AGENTS.md"), join(directory, "CLAUDE.md"));
    await symlink(join(live, "skills", "status"), join(directory, "skills", "status"));
  }
  await fake.close();
  const checked = await cli(["check", "--config", config], "");
  expect(checked.status, checked.stderr).toBe(0);
  for (const directory of [home, defaultHome]) {
    expect(await readlink(join(directory, "CLAUDE.md"))).toBe(join(live, "AGENTS.md"));
    expect(await readlink(join(directory, "skills", "status"))).toBe(
      join(live, "skills", "status"),
    );
    expect(await readdir(join(directory, "skills"))).toEqual(["status"]);
  }
  expect(await readdir(join(root, "agent"))).toEqual(["AGENTS.md"]);
});

it("setting up an existing profile preserves the serving installation's links and T3's home", async () => {
  const { root, config, fake } = await prepared();
  const home = join(root, "data", "carillon", "profiles", "claude");
  const live = join(root, "installation-a");
  await mkdir(join(home, "skills"), { recursive: true });
  await symlink(join(live, "AGENTS.md"), join(home, "CLAUDE.md"));
  await symlink(join(live, "skills", "status"), join(home, "skills", "status"));
  fake.instances.set("carillon_claude", { driver: "claudeAgent", config: { homePath: home } });
  const runtime = await loaded(config);
  const endpoint = { url: fake.url, token: fake.token };
  expect(await profile(runtime, endpoint, { harness: "claude" })).toEqual({
    instanceId: "carillon_claude",
    home,
  });
  expect(await readlink(join(home, "CLAUDE.md"))).toBe(join(live, "AGENTS.md"));
  expect(await readlink(join(home, "skills", "status"))).toBe(join(live, "skills", "status"));
  expect(await readdir(join(root, "agent"))).toEqual(["AGENTS.md"]);

  const moved = join(root, "moved-home");
  fake.instances.set("carillon_claude", { driver: "claudeAgent", config: { homePath: moved } });
  const made = await cli(
    ["profile", "--config", config, "--url", fake.url, "--harness", "claude"],
    `${fake.token}\n`,
  );
  expect(made.status, made.stderr).toBe(0);
  expect(JSON.parse(made.stdout)).toEqual({ instanceId: "carillon_claude", home: moved });
  expect(await readdir(moved)).toEqual([]);
});

it.each([
  ["missing", undefined],
  ["no homePath", { driver: "codex" }],
  ["empty homePath", { driver: "codex", config: { homePath: "" } }],
  ["wrong driver", { driver: "claudeAgent", config: { homePath: "/unused" } }],
  ["malformed", { driver: "codex", config: { homePath: 42 } }],
])("startup refuses a saved profile with %s before serving", async (_label, instance) => {
  const { config, fake } = await prepared();
  if (instance === undefined) fake.instances.delete("carillon_codex");
  else fake.instances.set("carillon_codex", instance);
  const started = await serve(config);
  expect(started.status).toBe(1);
  expect(started.stdout).not.toContain("runtime-t3 ready");
  expect(started.stderr).toMatch(/carillon_codex|unreadable provider settings/u);
});

it("a harness file conflict prevents startup from accepting any turn", async () => {
  const { root, config, home, fake } = await prepared();
  await mkdir(home);
  await writeFile(join(home, "CLAUDE.md"), "Native instructions");
  const started = await serve(config);
  expect(started.status).toBe(1);
  expect(started.stdout).not.toContain("runtime-t3 ready");
  expect(started.stderr).toContain("is not a link to");
  expect(await readFile(join(home, "CLAUDE.md"), "utf8")).toBe("Native instructions");
  expect(fake.commands).toEqual([]);
  expect(
    await lstat(join(root, "data", "carillon", "profiles")).catch(() => undefined),
  ).toBeUndefined();
});
