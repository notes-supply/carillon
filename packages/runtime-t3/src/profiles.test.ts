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
import { join } from "node:path";
import { expect, it } from "vitest";
import { configure, profile } from "./setup.ts";
import { loadSettings } from "./settings.ts";
import { cli, fakeT3, installation, loaded } from "./setup-fixtures.ts";

it("makes a Carillon profile: a T3 instance whose home links to the assistant's resources", async () => {
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
  expect(await readlink(join(home, "CLAUDE.md"))).toBe(join(root, "agent", "AGENTS.md"));
  expect((await lstat(join(home, "skills"))).isDirectory()).toBe(true);
  expect(await readlink(join(home, "skills", "status"))).toBe(
    join(root, "agent", "skills", "status"),
  );
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
  expect(await readlink(join(codexHome, "AGENTS.md"))).toBe(join(root, "agent", "AGENTS.md"));
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
  fake.instances.set("carillon_codex", { driver: "codex", config: { homePath: "/elsewhere" } });
  await expect(profile(runtime, endpoint, { harness: "codex" })).rejects.toThrow(
    /not this profile/u,
  );
  expect(fake.instances.get("carillon_codex")).toEqual({
    driver: "codex",
    config: { homePath: "/elsewhere" },
  });
  // So is a home holding the owner's own file where the link should be.
  await rm(join(codexHome, "AGENTS.md"));
  await writeFile(join(codexHome, "AGENTS.md"), "the owner's own");
  await expect(profile(runtime, endpoint, { harness: "codex" })).rejects.toThrow(
    /is not a link to/u,
  );
  expect((await lstat(join(codexHome, "AGENTS.md"))).isFile()).toBe(true);
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
  const { root, config } = await installation();
  const data = join(root, "data");
  const home = join(data, "carillon", "profiles", "claude");
  await writeFile(join(root, "credentials", "t3.token"), "saved-token\n");
  await writeFile(
    join(root, "providers", "runtime-t3.toml"),
    [
      'url = "http://127.0.0.1:1"',
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

it("reconciles skill links through startup checks, preserves harness skills, and follows restored resources", async () => {
  const { root, config, home } = await savedProfile();
  const resources = join(root, "agent");
  await mkdir(join(resources, "skills", "first"), { recursive: true });
  await writeFile(join(resources, "skills", "first", "SKILL.md"), "First skill");
  await mkdir(home, { recursive: true });
  await symlink(join(resources, "AGENTS.md"), join(home, "CLAUDE.md"));
  await symlink(join(resources, "skills"), join(home, "skills"));
  const check = async () => {
    const checked = await cli(["check", "--config", config], "");
    expect(checked.status, checked.stderr).toBe(0);
  };
  await check();
  const skills = join(home, "skills");
  expect((await lstat(skills)).isDirectory()).toBe(true);
  expect(await readlink(join(skills, "first"))).toBe(join(resources, "skills", "first"));
  await mkdir(join(skills, ".system"));
  await writeFile(join(skills, ".system", "native"), "The harness's own");
  await symlink(join(root, "workspace"), join(skills, "external"));
  expect(await lstat(join(resources, "skills", ".system")).catch(() => undefined)).toBeUndefined();
  await rm(join(resources, "skills", "first"), { recursive: true });
  await mkdir(join(resources, "skills", "second"));
  await writeFile(join(resources, "skills", "second", "SKILL.md"), "Second skill");
  await check();
  expect((await readdir(skills)).toSorted()).toEqual([".system", "external", "second"]);
  expect(await readlink(join(skills, "second"))).toBe(join(resources, "skills", "second"));

  const restored = join(root, "restored-agent");
  await rename(resources, restored);
  await writeFile(
    config,
    (await readFile(config, "utf8")).replace('resources = "agent"', 'resources = "restored-agent"'),
  );
  await check();
  expect(await readlink(join(home, "CLAUDE.md"))).toBe(join(restored, "AGENTS.md"));
  expect(await readlink(join(skills, "second"))).toBe(join(restored, "skills", "second"));
  await check();
  expect((await readdir(skills)).toSorted()).toEqual([".system", "external", "second"]);
  expect(await readFile(join(skills, ".system", "native"), "utf8")).toBe("The harness's own");
  expect(await readlink(join(skills, "external"))).toBe(join(root, "workspace"));

  await mkdir(join(restored, "skills", ".system"));
  const conflict = await cli(["check", "--config", config], "");
  expect(conflict.status).toBe(1);
  expect(conflict.stderr).toContain("is not a link to");
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
