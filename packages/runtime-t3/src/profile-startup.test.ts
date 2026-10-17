import { lstat, mkdir, readFile, readlink, readdir, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { expect, it } from "vitest";
import { profileIds } from "./profiles.ts";
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
  const opencodeHome = join(root, "opencode-home");
  fake.instances.set("carillon_opencode", {
    driver: "opencode",
    environment: [
      { name: "OPENCODE_CONFIG_DIR", value: opencodeHome, sensitive: false },
      { name: "OPENCODE_DISABLE_CLAUDE_CODE", value: "1", sensitive: false },
    ],
  });
  await writeFile(join(root, "credentials", "t3.token"), fake.token);
  await writeFile(
    join(root, "providers", "runtime-t3.toml"),
    [
      `url = "${fake.url}"`,
      'token_file = "credentials/t3.token"',
      `profiles = ${JSON.stringify(profileIds())}`,
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
  return { ...installed, fake, home, opencodeHome };
}

it("startup reconciles every saved profile at the home T3 keeps before serving", async () => {
  const { root, config, home, opencodeHome } = await prepared();
  const resources = join(root, "agent");
  await mkdir(join(resources, "skills", "status"), { recursive: true });
  await writeFile(join(resources, "skills", "status", "SKILL.md"), "Status");
  const started = await serve(config);
  expect(started.status, started.stderr).toBe(0);
  expect(started.stdout).toContain("runtime-t3 ready");
  expect(await readlink(join(home, "CLAUDE.md"))).toBe(join(resources, "AGENTS.md"));
  expect(await readlink(join(home, "skills", "status"))).toBe(join(resources, "skills", "status"));
  expect(await readlink(join(root, "codex-home", "AGENTS.md"))).toBe(join(resources, "AGENTS.md"));
  expect(await readlink(join(opencodeHome, "AGENTS.md"))).toBe(join(resources, "AGENTS.md"));
  expect(await readlink(join(opencodeHome, "skills", "status"))).toBe(
    join(resources, "skills", "status"),
  );
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
  expect(
    await profile(runtime, endpoint, {
      kind: "own-account",
      harness: "claude",
      credentials: { kind: "account" },
    }),
  ).toEqual({ instanceId: "carillon_claude", home });
  expect(await readlink(join(home, "CLAUDE.md"))).toBe(join(live, "AGENTS.md"));
  expect(await readlink(join(home, "skills", "status"))).toBe(join(live, "skills", "status"));
  expect(await readdir(join(root, "agent"))).toEqual(["AGENTS.md"]);
});

it("profile returns the home T3 keeps for native sign-in when it differs from setup's default", async () => {
  const { config, home, opencodeHome, fake } = await prepared();
  for (const [harness, expected] of [
    ["claude", { instanceId: "carillon_claude", home }],
    ["opencode", { instanceId: "carillon_opencode", home: opencodeHome }],
  ] as const) {
    const made = await cli(
      ["profile", "--config", config, "--url", fake.url, "--harness", harness],
      `${fake.token}\n`,
    );
    expect(made.status, made.stderr).toBe(0);
    const result: unknown = JSON.parse(made.stdout);
    expect(result).toEqual(expected);
    expect(await readdir(expected.home)).toEqual([]);
  }
});

const opencodeFlag = { name: "OPENCODE_DISABLE_CLAUDE_CODE", value: "1" };

const malformedEnvironment = {
  driver: "opencode",
  environment: [{ name: "OPENCODE_CONFIG_DIR", value: 42 }, opencodeFlag],
};

it.each([
  ["carillon_codex", "missing", undefined],
  ["carillon_codex", "no homePath", { driver: "codex" }],
  ["carillon_codex", "empty homePath", { driver: "codex", config: { homePath: "" } }],
  ["carillon_codex", "wrong driver", { driver: "claudeAgent", config: { homePath: "/unused" } }],
  ["carillon_codex", "malformed", { driver: "codex", config: { homePath: 42 } }],
  ["carillon_opencode", "missing", undefined],
  ["carillon_opencode", "no home variable", { driver: "opencode", environment: [opencodeFlag] }],
  [
    "carillon_opencode",
    "blank home variable",
    { driver: "opencode", environment: [{ name: "OPENCODE_CONFIG_DIR", value: "" }, opencodeFlag] },
  ],
  [
    "carillon_opencode",
    "relative home",
    {
      driver: "opencode",
      environment: [{ name: "OPENCODE_CONFIG_DIR", value: "opencode-home" }, opencodeFlag],
    },
  ],
  [
    "carillon_opencode",
    "tilde home",
    {
      driver: "opencode",
      environment: [{ name: "OPENCODE_CONFIG_DIR", value: "~/opencode-home" }, opencodeFlag],
    },
  ],
  [
    "carillon_opencode",
    "duplicated home variable",
    {
      driver: "opencode",
      environment: [
        { name: "OPENCODE_CONFIG_DIR", value: "/a" },
        { name: "OPENCODE_CONFIG_DIR", value: "/b" },
        opencodeFlag,
      ],
    },
  ],
  [
    "carillon_opencode",
    "secret home variable",
    {
      driver: "opencode",
      environment: [{ name: "OPENCODE_CONFIG_DIR", value: "/a", sensitive: true }, opencodeFlag],
    },
  ],
  [
    "carillon_opencode",
    "wrong driver",
    { driver: "codex", environment: [{ name: "OPENCODE_CONFIG_DIR", value: "/a" }, opencodeFlag] },
  ],
  [
    "carillon_opencode",
    "flag against Claude's files absent",
    { driver: "opencode", environment: [{ name: "OPENCODE_CONFIG_DIR", value: "/a" }] },
  ],
  [
    "carillon_opencode",
    "flag against Claude's files off",
    {
      driver: "opencode",
      environment: [
        { name: "OPENCODE_CONFIG_DIR", value: "/a" },
        { name: "OPENCODE_DISABLE_CLAUDE_CODE", value: "0" },
      ],
    },
  ],
  [
    "carillon_opencode",
    "serverUrl",
    {
      driver: "opencode",
      config: { serverUrl: "http://127.0.0.1:4096" },
      environment: [{ name: "OPENCODE_CONFIG_DIR", value: "/a" }, opencodeFlag],
    },
  ],
  [
    "carillon_opencode",
    "malformed serverUrl",
    {
      driver: "opencode",
      config: { serverUrl: 42 },
      environment: [{ name: "OPENCODE_CONFIG_DIR", value: "/a" }, opencodeFlag],
    },
  ],
  ["carillon_opencode", "malformed environment", malformedEnvironment],
])(
  "startup refuses %s with %s before serving, writing no link",
  async (instanceId, _label, instance) => {
    const { root, config, fake, home, opencodeHome } = await prepared();
    if (instance === undefined) fake.instances.delete(instanceId);
    else fake.instances.set(instanceId, instance);
    const started = await serve(config);
    expect(started.status).toBe(1);
    expect(started.stdout).not.toContain("runtime-t3 ready");
    expect(started.stderr).toMatch(new RegExp(`${instanceId}|unreadable provider settings`, "u"));
    for (const directory of [home, join(root, "codex-home"), opencodeHome])
      await expect(lstat(directory)).rejects.toMatchObject({ code: "ENOENT" });
  },
);

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

it.each(profileIds())("startup refuses unsaved %s for plain T3 Code", async (instanceId) => {
  const { root, config, fake } = await prepared();
  await writeFile(
    config,
    (await readFile(config, "utf8")).replace(/\[\[agents\]\]\nid = "assistant"[^[]*/u, ""),
  );
  await writeFile(
    join(root, "providers", "runtime-t3.toml"),
    [
      `url = "${fake.url}"`,
      'token_file = "credentials/t3.token"',
      "[agents.code]",
      'project_id = "agent"',
      "[agents.code.model]",
      `instance_id = "${instanceId}"`,
      'model = "profile-model"',
    ].join("\n"),
  );
  const started = await serve(config);
  expect(started.status).toBe(1);
  expect(started.stdout).not.toContain("runtime-t3 ready");
  expect(started.stderr).toContain("/agents/code/model");
});
