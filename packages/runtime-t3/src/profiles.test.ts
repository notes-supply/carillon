import {
  lstat,
  mkdir,
  readFile,
  readdir,
  readlink,
  rename,
  rm,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { dirname, join } from "node:path";
import { expect, it } from "vitest";
import { configure, discover, profile } from "./setup.ts";
import { reconcileProfile } from "./profiles.ts";
import { loadSettings } from "./settings.ts";
import { cli, fakeT3, installation, loaded, serve } from "./setup-fixtures.ts";

it("makes a Carillon profile and its sign-in home, leaving resource links for startup", async () => {
  const fake = await fakeT3();
  const { root, workspace, config } = await installation();
  const runtime = await loaded(config);
  const endpoint = { url: fake.url, token: fake.token };
  await mkdir(join(root, "agent", "skills", "status"), { recursive: true });
  await writeFile(join(root, "agent", "skills", "status", "SKILL.md"), "Carillon status");
  // Profiles live under the XDG data directory, by harness.
  const home = join(root, "data", "carillon", "profiles", "claude");
  expect(await profile(runtime, endpoint, { harness: "claude", key: "sk-test" })).toEqual({
    instanceId: "carillon_claude",
    home,
  });
  expect(await readdir(home)).toEqual([]);
  expect((await stat(home)).mode & 0o777).toBe(0o700);
  // T3 holds the instance, its key a secret of its own, never the runtime's.
  expect(fake.instances.get("carillon_claude")).toEqual({
    driver: "claudeAgent",
    displayName: "Claude · Carillon",
    enabled: true,
    config: { homePath: home },
    environment: [{ name: "ANTHROPIC_API_KEY", value: "sk-test", sensitive: true }],
  });
  expect(await readFile(runtime.providerFile, "utf8")).not.toContain("sk-test");
  expect((await loadSettings(runtime.providerFile, root)).profiles).toEqual(
    new Set(["carillon_claude"]),
  );

  // A compatible endpoint's address is the harness's, its token a secret.
  const proxied = await profile(runtime, endpoint, {
    harness: "codex",
    key: "proxy-token",
    endpoint: "https://proxy.example",
  });
  const codexHome = join(root, "data", "carillon", "profiles", "codex");
  expect(proxied).toEqual({ instanceId: "carillon_codex", home: codexHome });
  expect(fake.instances.get("carillon_codex")).toMatchObject({
    driver: "codex",
    environment: [
      { name: "OPENAI_BASE_URL", value: "https://proxy.example", sensitive: false },
      { name: "OPENAI_API_KEY", value: "proxy-token", sensitive: true },
    ],
  });
  expect(await readdir(codexHome)).toEqual([]);
  fake.instances.delete("carillon_codex");

  // Making it again keeps what is there.
  await profile(runtime, endpoint, { harness: "claude" });
  expect(fake.instances.size).toBe(1);

  // The assistant then runs on it, and plain T3 Code no longer may.
  fake.providers = [
    ...fake.providers,
    {
      instanceId: "carillon_claude",
      driver: "claudeAgent",
      enabled: true,
      installed: true,
      status: "ready",
      models: [{ slug: "claude-sonnet-5", name: "Claude Sonnet 5", capabilities: null }],
    },
  ];
  fake.projects = [{ id: "agent", title: "Agent", workspaceRoot: workspace }];
  const sonnet = { projectId: "agent", model: "claude-sonnet-5" };
  await configure(runtime, endpoint, {
    ...sonnet,
    agentId: "assistant",
    instanceId: "carillon_claude",
  });
  await expect(
    configure(runtime, endpoint, { ...sonnet, agentId: "code", instanceId: "carillon_claude" }),
  ).rejects.toThrow(/no usable model/u);

  // What is not this profile is refused, with T3 left as it was.
  fake.instances.set("carillon_codex", { driver: "claudeAgent", config: { homePath: codexHome } });
  await expect(profile(runtime, endpoint, { harness: "codex" })).rejects.toThrow(
    /not this profile/u,
  );
  expect(fake.instances.get("carillon_codex")).toEqual({
    driver: "claudeAgent",
    config: { homePath: codexHome },
  });
  fake.instances.set("carillon_codex", { driver: "codex", config: { homePath: codexHome } });
  await writeFile(join(codexHome, "AGENTS.md"), "the owner's own");
  await profile(runtime, endpoint, { harness: "codex" });
  expect(await readFile(join(codexHome, "AGENTS.md"), "utf8")).toBe("the owner's own");
  // Only an agent with resources has something to share.
  await writeFile(
    config,
    (await readFile(config, "utf8")).replace(/\[\[agents\]\]\nid = "assistant"[^[]*/u, ""),
  );
  await expect(profile(await loaded(config), endpoint, { harness: "claude" })).rejects.toThrow(
    /has resources/u,
  );
});

async function savedProfile(): Promise<{ root: string; config: string; home: string }> {
  const fake = await fakeT3();
  const { root, config } = await installation();
  const data = join(root, "data");
  const home = join(data, "carillon", "profiles", "claude");
  fake.instances.set("carillon_claude", { driver: "claudeAgent", config: { homePath: home } });
  await writeFile(join(root, "credentials", "t3.token"), fake.token);
  await writeFile(
    join(root, "providers", "runtime-t3.toml"),
    [
      `url = "${fake.url}"`,
      'token_file = "credentials/t3.token"',
      'profiles = ["carillon_claude"]',
      "[agents.assistant]",
      'project_id = "agent"',
      "[agents.assistant.model]",
      'instance_id = "carillon_claude"',
      'model = "m"',
      "[agents.code]",
      'project_id = "agent"',
      "[agents.code.model]",
      'instance_id = "codex"',
      'model = "m"',
    ].join("\n"),
  );
  return { root, config, home };
}

it("reconciles skill links at startup, preserves harness skills, and follows restored resources", async () => {
  const { root, config, home } = await savedProfile();
  const resources = join(root, "agent");
  await mkdir(join(resources, "skills", "first"), { recursive: true });
  await writeFile(join(resources, "skills", "first", "SKILL.md"), "First skill");
  await mkdir(home, { recursive: true });
  await symlink(join(resources, "AGENTS.md"), join(home, "CLAUDE.md"));
  await mkdir(join(resources, "skills", ".system"));
  await writeFile(join(resources, "skills", ".system", "native"), "Old leaked harness data");
  await writeFile(join(resources, "skills", "marker"), "Old leaked harness marker");
  await symlink(join(resources, "skills"), join(home, "skills"));
  const start = async () => {
    const checked = await serve(config);
    expect(checked.status, checked.stderr).toBe(0);
  };
  await start();
  const skills = join(home, "skills");
  expect((await lstat(skills)).isDirectory()).toBe(true);
  expect(await readlink(join(skills, "first"))).toBe(join(resources, "skills", "first"));
  await mkdir(join(skills, ".system"));
  await writeFile(join(skills, ".system", "native"), "The harness's own");
  await symlink(join(root, "workspace"), join(skills, "external"));
  expect(await readFile(join(resources, "skills", ".system", "native"), "utf8")).toBe(
    "Old leaked harness data",
  );
  expect(await lstat(join(skills, "marker")).catch(() => undefined)).toBeUndefined();
  await rm(join(resources, "skills", "first"), { recursive: true });
  await mkdir(join(resources, "skills", "second"));
  await writeFile(join(resources, "skills", "second", "SKILL.md"), "Second skill");
  await start();
  expect((await readdir(skills)).toSorted()).toEqual([".system", "external", "second"]);
  expect(await readlink(join(skills, "second"))).toBe(join(resources, "skills", "second"));

  const restored = join(root, "restored-agent");
  await rename(resources, restored);
  await writeFile(
    config,
    (await readFile(config, "utf8")).replace('resources = "agent"', 'resources = "restored-agent"'),
  );
  await start();
  expect(await readlink(join(home, "CLAUDE.md"))).toBe(join(restored, "AGENTS.md"));
  expect(await readlink(join(skills, "second"))).toBe(join(restored, "skills", "second"));
  await start();
  expect((await readdir(skills)).toSorted()).toEqual([".system", "external", "second"]);
  expect(await readFile(join(skills, ".system", "native"), "utf8")).toBe("The harness's own");
  expect(await readlink(join(skills, "external"))).toBe(join(root, "workspace"));

  await mkdir(join(skills, "conflict"));
  await writeFile(join(skills, "conflict", "native"), "Keep this skill");
  await mkdir(join(restored, "skills", "conflict"));
  await writeFile(join(restored, "skills", "conflict", "SKILL.md"), "Carillon's skill");
  const conflict = await serve(config);
  expect(conflict.status).toBe(1);
  expect(conflict.stderr).toContain("is not a link to");
  expect(await readFile(join(skills, "conflict", "native"), "utf8")).toBe("Keep this skill");
  expect(await readFile(join(skills, ".system", "native"), "utf8")).toBe("The harness's own");
});

it("refuses more than one assistant on the runtime before startup", async () => {
  const { config } = await savedProfile();
  await writeFile(
    config,
    (await readFile(config, "utf8")).replace('id = "code"', 'id = "code"\nresources = "agent"'),
  );
  const checked = await cli(["check", "--config", config], "");
  expect(checked.status).toBe(1);
  expect(checked.stderr).toContain("runtime-t3 may serve only one agent with resources");
});

it("refuses supplied credentials on an existing profile without reporting success or changing its environment", async () => {
  const fake = await fakeT3();
  const { config } = await installation();
  const runtime = await loaded(config);
  await profile(
    runtime,
    { url: fake.url, token: fake.token },
    { harness: "claude", key: "original-key" },
  );
  const before = await readFile(runtime.providerFile, "utf8");
  for (const extra of [[], ["--endpoint", "https://proxy.example"]]) {
    const refused = await cli(
      [
        "profile",
        "--config",
        config,
        "--url",
        fake.url,
        "--harness",
        "claude",
        "--with-key",
        ...extra,
      ],
      `${fake.token}\nreplacement-key\n`,
    );
    expect(refused.status).toBe(1);
    expect(refused.stdout).toBe("");
    expect(refused.stderr).toContain("change its environment in T3 settings");
    expect(refused.stderr).not.toContain("replacement-key");
    expect(fake.instances.get("carillon_claude")).toMatchObject({
      environment: [{ name: "ANTHROPIC_API_KEY", value: "original-key", sensitive: true }],
    });
    expect(await readFile(runtime.providerFile, "utf8")).toBe(before);
  }
});

it("concurrent reconciles create and replace shared links without losing native skills", async () => {
  const { root, config, home } = await savedProfile();
  const resources = join(root, "agent");
  for (const name of ["first", "second", "third"]) {
    await mkdir(join(resources, "skills", name), { recursive: true });
    await writeFile(join(resources, "skills", name, "SKILL.md"), name);
  }
  const overlap = async (source: string) => {
    const results = await Promise.allSettled(
      Array.from({ length: 16 }, () => reconcileProfile("claude", home, source)),
    );
    for (const result of results) expect(result).toEqual({ status: "fulfilled", value: undefined });
    expect(await readlink(join(home, "CLAUDE.md"))).toBe(join(source, "AGENTS.md"));
    for (const name of ["first", "second", "third"])
      expect(await readlink(join(home, "skills", name))).toBe(join(source, "skills", name));
    expect((await readdir(join(home, "skills"))).toSorted()).toEqual(["first", "second", "third"]);
  };
  await overlap(resources);
  await rm(join(home, "skills"), { recursive: true });
  await symlink(join(resources, "skills"), join(home, "skills"));
  await overlap(resources);
  const restored = join(root, "restored-agent");
  await rename(resources, restored);
  await overlap(restored);
  await writeFile(
    config,
    (await readFile(config, "utf8")).replace('resources = "agent"', 'resources = "restored-agent"'),
  );
  const checks = await Promise.all([
    cli(["check", "--config", config], ""),
    cli(["check", "--config", config], ""),
  ]);
  for (const checked of checks) expect(checked.status, checked.stderr).toBe(0);
  expect((await readdir(home)).toSorted()).toEqual(["CLAUDE.md", "skills"]);
});

it("refuses missing resources instead of recreating them", async () => {
  const { root, home } = await savedProfile();
  const missing = join(root, "missing-agent");
  await expect(reconcileProfile("claude", home, missing)).rejects.toThrow(
    /must be an existing directory/u,
  );
  await expect(stat(missing)).rejects.toMatchObject({ code: "ENOENT" });
});

it.each([
  ["skill directory", "skills/status", "directory"],
  ["skill manifest", "skills/status/SKILL.md", "file"],
  ["nested skill file", "skills/status/scripts/check.sh", "file"],
  ["nested skill directory", "skills/status/scripts", "directory"],
  ["skills root", "skills", "directory"],
  ["instructions", "AGENTS.md", "file"],
  ["dangling skill directory", "skills/status", "missing"],
  ["dangling skill manifest", "skills/status/SKILL.md", "missing"],
])("refuses a symlinked %s before changing profile links", async (name, relative, targetKind) => {
  const { root, config, home } = await savedProfile();
  const resources = join(root, "agent");
  const external = join(root, "external-skill");
  await mkdir(external);
  const manifest = join(external, "SKILL.md");
  await writeFile(manifest, "External skill");
  const path = join(resources, relative);
  await mkdir(dirname(path), { recursive: true });
  if (name.startsWith("nested"))
    await writeFile(join(resources, "skills", "status", "SKILL.md"), "Status skill");
  if (relative === "AGENTS.md") await rm(path);
  await symlink(
    targetKind === "missing" ? join(root, "missing") : targetKind === "file" ? manifest : external,
    path,
  );
  const checked = await serve(config);
  expect(checked.status, checked.stderr).toBe(1);
  expect(checked.stderr).toContain(`resource symlinks are not supported: ${path}`);
  await expect(lstat(join(home, "CLAUDE.md"))).rejects.toMatchObject({ code: "ENOENT" });
  await expect(lstat(join(home, "skills"))).rejects.toMatchObject({ code: "ENOENT" });
});

it("discovers profile existence from T3 even when the saved list disagrees", async () => {
  const fake = await fakeT3();
  const { config } = await installation();
  const runtime = await loaded(config);
  const endpoint = { url: fake.url, token: fake.token };
  await profile(runtime, endpoint, { harness: "claude", key: "original-key" });
  expect((await discover(endpoint)).profiles).toEqual(["carillon_claude"]);
  fake.instances.delete("carillon_claude");
  expect((await loadSettings(runtime.providerFile, runtime.root)).profiles).toEqual(
    new Set(["carillon_claude"]),
  );
  expect((await discover(endpoint)).profiles).toEqual([]);
});

it("removes only its abandoned temporary skill links after a crashed reconciliation", async () => {
  const { root, config, home } = await savedProfile();
  const source = join(root, "agent", "skills");
  await mkdir(join(source, "status"), { recursive: true });
  await writeFile(join(source, "status", "SKILL.md"), "Status skill");
  await mkdir(join(home, "skills"), { recursive: true });
  const temporary = `.status.${crypto.randomUUID()}.tmp`;
  const old = join(root, "old-agent");
  await symlink(join(old, "AGENTS.md"), join(home, "CLAUDE.md"));
  const stale = `.removed.${crypto.randomUUID()}.tmp`;
  await symlink(join(source, "status"), join(home, "skills", temporary));
  await symlink(join(old, "skills", "removed"), join(home, "skills", stale));
  const native = `.native.${crypto.randomUUID()}.tmp`;
  await symlink(join(root, "native"), join(home, "skills", native));
  await writeFile(join(home, "skills", `.status.${crypto.randomUUID()}.tmp`), "native file");
  const before = await readdir(join(home, "skills"));
  const checked = await serve(config);
  expect(checked.status, checked.stderr).toBe(0);
  expect((await readdir(join(home, "skills"))).toSorted()).toEqual(
    [...before.filter((name) => name !== temporary && name !== stale), "status"].toSorted(),
  );
  expect(await readlink(join(home, "skills", "status"))).toBe(join(source, "status"));
  expect(await readlink(join(home, "skills", native))).toBe(join(root, "native"));
});
