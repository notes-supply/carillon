import { spawn } from "node:child_process";
import { once } from "node:events";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  readlink,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadInstallation, type Installation } from "@carillon/runtime/installation";
import { afterEach, expect, it } from "vitest";
import { startFakeT3, type FakeT3 } from "./fake.ts";
import { loadSettings } from "./settings.ts";
import { configure, discover, profile } from "./setup.ts";
import { T3Failure } from "./t3.ts";

const cleanups: (() => Promise<void>)[] = [];
afterEach(async () => {
  await Promise.all(cleanups.splice(0).map((cleanup) => cleanup()));
});

async function fakeT3(token?: string): Promise<FakeT3> {
  const fake = await startFakeT3(token);
  cleanups.push(() => fake.close());
  return fake;
}

const HASH = "0".repeat(64);

/**
 * An installation registering runtime-t3, with its assistant, whose
 * resources are `agent/`, and plain T3 Code, `code`, working in `workspace/`.
 */
async function installation(): Promise<{ root: string; workspace: string; config: string }> {
  const root = await mkdtemp(join(tmpdir(), "carillon-t3-setup-"));
  cleanups.push(() => rm(root, { recursive: true }));
  const workspace = join(root, "workspace");
  await mkdir(workspace);
  await mkdir(join(root, "agent"));
  await writeFile(join(root, "agent", "AGENTS.md"), "Carillon's instructions\n");
  await mkdir(join(root, "credentials"));
  await mkdir(join(root, "providers"));
  const config = join(root, "carillon.toml");
  await writeFile(
    config,
    [
      'data_dir = "state"',
      "[core]",
      `operator_token_hash = "${HASH}"`,
      "[[agents]]",
      'id = "assistant"',
      'name = "Carillon"',
      'runtime = "runtime-t3"',
      `workspace = "${workspace}"`,
      'resources = "agent"',
      "[[agents]]",
      'id = "code"',
      'name = "T3 Code"',
      'runtime = "runtime-t3"',
      `workspace = "${workspace}"`,
      "[[providers]]",
      'id = "runtime-t3"',
      'role = "runtime"',
      'version = "0.0.0"',
      'url = "http://127.0.0.1:1"',
      `token_hash = "${HASH}"`,
      "enabled = true",
    ].join("\n"),
  );
  return { root, workspace, config };
}

async function loaded(config: string): Promise<Installation> {
  const result = await loadInstallation(config, "runtime-t3");
  if (typeof result === "string") throw new Error(`runtime-t3 is ${result}`);
  return result;
}

it("discovers T3's projects and usable models", async () => {
  const fake = await fakeT3();
  fake.projects = [{ id: "p1", title: "Home", workspaceRoot: "/home/someone" }];
  const offer = await discover({ url: fake.url, token: fake.token });
  expect(offer.projects).toEqual([{ id: "p1", title: "Home", workspace: "/home/someone" }]);
  // Only enabled, installed, usable instances are offered.
  expect(offer.models.map(({ provider, model }) => `${provider}/${model}`)).toEqual([
    "claudeAgent/claude-sonnet-5",
    "claudeAgent/claude-haiku-4-5",
    "codex/gpt-6-astra",
  ]);
  await expect(discover({ url: fake.url, token: "wrong" })).rejects.toSatisfy(
    (failure) => failure instanceof T3Failure && failure.kind === "refused",
  );
  expect(fake.refreshes).toEqual([]);
});

it("has T3 check instances signed in since it last looked before listing models", async () => {
  // T3 keeps what it last found of an instance's sign-in, so a profile
  // signed in after T3 made it is checked again on request, and only it.
  const fake = await fakeT3();
  await discover({ url: fake.url, token: fake.token }, undefined, ["carillon_codex"]);
  expect(fake.refreshes).toEqual([{ instanceId: "carillon_codex" }]);
});

it("writes the runtime's files only for a project rooted at the agent's workspace and a model T3 offers", async () => {
  const fake = await fakeT3();
  const { root, workspace, config } = await installation();
  const runtime = await loaded(config);
  const endpoint = { url: fake.url, token: fake.token };
  fake.projects = [
    { id: "elsewhere", title: "Elsewhere", workspaceRoot: tmpdir() },
    { id: "agent", title: "Agent", workspaceRoot: workspace },
  ];
  const sonnet = {
    agentId: "code",
    projectId: "agent",
    instanceId: "claudeAgent",
    model: "claude-sonnet-5",
  };
  for (const [choice, refusal] of [
    [{ ...sonnet, projectId: "missing" }, /no project/u],
    [{ ...sonnet, projectId: "elsewhere" }, /rooted/u],
    [{ ...sonnet, instanceId: "cursor", model: "auto" }, /no usable model/u],
    [{ ...sonnet, agentId: "elsewhere" }, /not an agent of runtime-t3/u],
    // The assistant, the agent with resources, runs on Carillon's profiles
    // alone, and there are none yet.
    [{ ...sonnet, agentId: "assistant" }, /among Carillon's profiles/u],
  ] as const) {
    await expect(configure(runtime, endpoint, choice)).rejects.toThrow(refusal);
    expect(await readdir(join(root, "credentials"))).toEqual([]);
    expect(await readdir(join(root, "providers"))).toEqual([]);
  }

  const project = await configure(runtime, endpoint, sonnet);
  expect(project.title).toBe("Agent");
  const settings = await loadSettings(runtime.providerFile, root);
  expect(settings.url).toBe(fake.url);
  expect(settings.agents.get("code")).toEqual({
    projectId: "agent",
    selection: { instanceId: "claudeAgent", model: "claude-sonnet-5", options: [] },
  });
  expect((await readFile(settings.tokenFile, "utf8")).trim()).toBe(fake.token);
  for (const file of [settings.tokenFile, runtime.providerFile])
    expect((await stat(file)).mode & 0o777).toBe(0o600);
  // The settings name the credential file, never the token.
  expect(await readFile(runtime.providerFile, "utf8")).not.toContain(fake.token);
});

it("keeps the title on a rerun, and the model's options while the model stays the same", async () => {
  const { workspace, config } = await installation();
  const runtime = await loaded(config);
  const sonnet = {
    agentId: "code",
    projectId: "agent",
    instanceId: "claudeAgent",
    model: "claude-sonnet-5",
  };
  // Settings the operator tuned as documented, with another agent's.
  await writeFile(
    runtime.providerFile,
    [
      'url = "http://127.0.0.1:1"',
      'token_file = "credentials/t3.token"',
      'title = "Notes"',
      "[agents.code]",
      'project_id = "agent"',
      "[agents.code.model]",
      'instance_id = "claudeAgent"',
      'model = "claude-sonnet-5"',
      "[agents.code.model.options]",
      'effort = "high"',
      "thinking = true",
      "[agents.other]",
      'project_id = "other"',
      "[agents.other.model]",
      'instance_id = "codex"',
      'model = "gpt-6-astra"',
    ].join("\n"),
  );

  // Another server with a new token, the same model: everything tuned stays.
  const moved = await fakeT3("rotated-token");
  moved.projects = [{ id: "agent", title: "Agent", workspaceRoot: workspace }];
  await configure(runtime, { url: moved.url, token: moved.token }, sonnet);
  let settings = await loadSettings(runtime.providerFile, runtime.root);
  expect(settings).toMatchObject({ url: moved.url, title: "Notes" });
  expect(settings.agents.get("code")?.selection).toEqual({
    instanceId: "claudeAgent",
    model: "claude-sonnet-5",
    options: [
      { id: "effort", value: "high" },
      { id: "thinking", value: true },
    ],
  });
  expect(settings.agents.get("other")?.projectId).toBe("other");
  expect((await readFile(settings.tokenFile, "utf8")).trim()).toBe("rotated-token");

  // Another model starts from its own defaults; the title stays, and so does
  // the token's file, the token being the same.
  const tokenFile = settings.tokenFile;
  await configure(
    runtime,
    { url: moved.url, token: moved.token },
    { ...sonnet, model: "claude-haiku-4-5" },
  );
  settings = await loadSettings(runtime.providerFile, runtime.root);
  expect(settings.tokenFile).toBe(tokenFile);
  expect(settings.title).toBe("Notes");
  expect(settings.agents.get("code")?.selection).toEqual({
    instanceId: "claudeAgent",
    model: "claude-haiku-4-5",
    options: [],
  });
});

it("refuses settings that do not read as valid before writing anything", async () => {
  const fake = await fakeT3();
  const { root, workspace, config } = await installation();
  const runtime = await loaded(config);
  fake.projects = [{ id: "agent", title: "Agent", workspaceRoot: workspace }];
  const valid = [
    `url = "${fake.url}"`,
    'token_file = "credentials/t3.token"',
    "[agents.code]",
    'project_id = "agent"',
    "[agents.code.model]",
    'instance_id = "claudeAgent"',
    'model = "claude-sonnet-5"',
  ];
  for (const malformed of [
    // Not TOML, the line before holding something that must not be repeated.
    'note = "secret-in-file"\ntitle =\n',
    // TOML, but not the runtime's settings.
    ['title = ""', ...valid].join("\n"),
    [...valid, "[agents.code.model.options]", "effort = 3"].join("\n"),
  ]) {
    await writeFile(runtime.providerFile, malformed);
    await expect(
      configure(
        runtime,
        { url: fake.url, token: fake.token },
        {
          agentId: "code",
          projectId: "agent",
          instanceId: "claudeAgent",
          model: "claude-sonnet-5",
        },
      ),
    ).rejects.toSatisfy(
      (failure) =>
        failure instanceof Error &&
        failure.message.startsWith(runtime.providerFile) &&
        failure.message.endsWith(
          "nothing was changed: correct or remove it, then configure again",
        ) &&
        !failure.message.includes("secret-in-file"),
    );
    expect(await readFile(runtime.providerFile, "utf8")).toBe(malformed);
    expect(await readdir(join(root, "credentials"))).toEqual([]);
  }
});

it("gives a new token a file of its own and leaves settings naming a token written before them", async () => {
  const fake = await fakeT3();
  const { root, workspace, config } = await installation();
  const runtime = await loaded(config);
  fake.projects = [{ id: "agent", title: "Agent", workspaceRoot: workspace }];
  const choice = {
    agentId: "code",
    projectId: "agent",
    instanceId: "claudeAgent",
    model: "claude-sonnet-5",
  };
  await configure(runtime, { url: fake.url, token: fake.token }, choice);
  const first = await loadSettings(runtime.providerFile, root);
  const previous = await readFile(runtime.providerFile, "utf8");
  const credentials = join(root, "credentials");
  const providers = join(root, "providers");
  const tokens = async () => (await readdir(credentials)).sort();
  const kept = await tokens();
  const moved = await fakeT3("rotated-token");
  moved.projects = fake.projects;
  const rotated = { url: moved.url, token: moved.token };
  /** Configuring the rotated token with `directory` at `mode`: how it failed. */
  const failing = async (directory: string, mode: number): Promise<string> => {
    await chmod(directory, mode);
    try {
      return await configure(runtime, rotated, choice).then(
        () => "configured",
        (failure: unknown) => (failure instanceof Error ? failure.message : String(failure)),
      );
    } finally {
      await chmod(directory, 0o700);
    }
  };

  // Settings that would not be valid are refused before any token is written.
  await expect(
    configure(runtime, { url: `${moved.url}/t3`, token: moved.token }, choice),
  ).rejects.toThrow(/^the T3 settings and the token they name were not changed: .*\/url must be/u);
  expect(await readFile(runtime.providerFile, "utf8")).toBe(previous);
  expect(await tokens()).toEqual(kept);

  // A token file that cannot be written leaves the settings as they were.
  expect(await failing(credentials, 0o500)).toMatch(
    /^the T3 settings and the token they name were not changed: /u,
  );
  expect(await readFile(runtime.providerFile, "utf8")).toBe(previous);
  expect(await tokens()).toEqual(kept);

  // Settings that cannot replace the file leave it naming the old token; the
  // new token's file is kept, named by nothing.
  expect(await failing(providers, 0o500)).toMatch(
    /^the T3 settings and the token they name were not changed: /u,
  );
  expect(await readFile(runtime.providerFile, "utf8")).toBe(previous);
  expect((await tokens()).length).toBe(kept.length + 1);

  // Settings that replace the file but whose directory cannot be synced are
  // in place, naming the new token's file, which was synced first; a crash
  // could bring back only the previous settings, naming the old token's file,
  // which nothing changed.
  expect(await failing(providers, 0o300)).toMatch(
    /runtime-t3\.toml: changed, but not confirmed durable: /u,
  );
  const replaced = await loadSettings(runtime.providerFile, root);
  expect(replaced.tokenFile).not.toBe(first.tokenFile);
  expect((await readFile(replaced.tokenFile, "utf8")).trim()).toBe("rotated-token");
  expect((await readFile(first.tokenFile, "utf8")).trim()).toBe(fake.token);
});

it("checks the saved settings and T3 token as startup reads them, reaching no T3", async () => {
  const { root, config } = await installation();
  const providerFile = join(root, "providers", "runtime-t3.toml");
  const tokenFile = join(root, "credentials", "t3.token");
  // Nothing listens at this address: the check reads files only.
  const agent = (id: string, instance: string) => [
    `[agents.${id}]`,
    'project_id = "agent"',
    `[agents.${id}.model]`,
    `instance_id = "${instance}"`,
    'model = "m"',
  ];
  const settings = (token: string, ...agents: string[][]) =>
    [
      'url = "http://127.0.0.1:1"',
      `token_file = "credentials/${token}"`,
      'profiles = ["carillon_claude"]',
      ...agents.flat(),
    ].join("\n");
  const usable = settings(
    "t3.token",
    agent("assistant", "carillon_claude"),
    agent("code", "claudeAgent"),
  );
  await writeFile(tokenFile, "saved-token\n");
  await writeFile(providerFile, usable);
  const passed = await cli(["check", "--config", config], "");
  expect(passed.status, passed.stderr).toBe(0);

  // An agent's workspace, apart from the installation, must be a directory
  // for the runtime to start, whatever its settings and token say.
  const workspace = await mkdtemp(join(tmpdir(), "carillon-t3-workspace-"));
  cleanups.push(() => rm(workspace, { recursive: true, force: true }));
  const saved = await readFile(config, "utf8");
  await writeFile(config, saved.replace(/^workspace = ".*"$/mu, `workspace = "${workspace}"`));
  expect((await cli(["check", "--config", config], "")).status).toBe(0);
  for (const unusable of [
    () => rm(workspace, { recursive: true }),
    () => writeFile(workspace, ""),
  ]) {
    await unusable();
    const refused = await cli(["check", "--config", config], "");
    expect(refused.status).toBe(1);
    expect(refused.stderr).toBe(
      `error: ${workspace}: Carillon's workspace must be an existing directory\n`,
    );
  }
  await rm(workspace);
  await writeFile(config, saved);

  // Every agent of the runtime has its settings, and the settings name no other agent.
  for (const [mismatched, reason] of [
    [saved.replace('id = "code"', 'id = "unset"'), /unset has no T3 settings/u],
    [
      saved.replace(/\[\[agents\]\]\nid = "code"[^[]*/u, ""),
      /\/agents\/code is not an agent of runtime-t3/u,
    ],
  ] as const) {
    await writeFile(config, mismatched);
    const refused = await cli(["check", "--config", config], "");
    expect(refused.status).toBe(1);
    expect(refused.stderr).toMatch(reason);
  }
  await writeFile(config, saved);

  // The assistant, the agent with resources, runs on the profiles alone,
  // and plain T3 Code never does.
  for (const [swapped, reason] of [
    [
      settings("t3.token", agent("assistant", "claudeAgent"), agent("code", "codex")),
      /\/agents\/assistant\/model must be on one of \/profiles/u,
    ],
    [
      settings("t3.token", agent("assistant", "carillon_claude"), agent("code", "carillon_claude")),
      /\/agents\/code\/model must not be on one of \/profiles/u,
    ],
  ] as const) {
    await writeFile(providerFile, swapped);
    const refused = await cli(["check", "--config", config], "");
    expect(refused.status).toBe(1);
    expect(refused.stderr).toMatch(reason);
  }
  await writeFile(providerFile, usable);

  // A runtime that does not start, disabled or not registered as in a Pi
  // installation, passes with no settings at all.
  const registry = await readFile(config, "utf8");
  await rm(providerFile);
  for (const [changed, state] of [
    [registry.replace("enabled = true", "enabled = false"), "disabled"],
    [registry.replace('id = "runtime-t3"', 'id = "runtime-pi"'), "not in carillon.toml"],
  ] as const) {
    await writeFile(config, changed);
    const idle = await cli(["check", "--config", config], "");
    expect(idle.status, idle.stderr).toBe(0);
    expect(idle.stdout).toBe(`runtime-t3 is ${state}; nothing to check\n`);
  }
  await writeFile(config, registry);
  await writeFile(providerFile, usable);

  for (const [unusable, reason] of [
    [() => writeFile(tokenFile, "\n"), /t3\.token: the T3 token file is empty/u],
    [
      () =>
        writeFile(
          providerFile,
          settings(
            "missing.token",
            agent("assistant", "carillon_claude"),
            agent("code", "claudeAgent"),
          ),
        ),
      /missing\.token/u,
    ],
    [
      () => writeFile(providerFile, 'url = "saved-token\ntitle ='),
      /runtime-t3\.toml: invalid TOML at line /u,
    ],
  ] as const) {
    await unusable();
    const refused = await cli(["check", "--config", config], "");
    expect(refused.status).toBe(1);
    // One short line, quoting nothing from the files.
    expect(refused.stderr).toMatch(new RegExp(`^error: .*${reason.source}.*\\n$`, "u"));
    expect(refused.stderr).not.toContain("saved-token");
  }
});

/** The CLI with `input` on standard input; the fake T3 serves from this process meanwhile. */
async function cli(
  args: readonly string[],
  input: string,
): Promise<{ status: number | null; stdout: string; stderr: string }> {
  const child = spawn(process.execPath, [join(import.meta.dirname, "cli.ts"), ...args]);
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk: Buffer) => (stdout += chunk.toString()));
  child.stderr.on("data", (chunk: Buffer) => (stderr += chunk.toString()));
  child.stdin.end(input);
  await once(child, "close");
  return { status: child.exitCode, stdout, stderr };
}

it("takes the token on standard input, says the settings are saved for a start, and reports a refusal in one line", async () => {
  const fake = await fakeT3();
  const discovered = await cli(
    ["discover", "--url", fake.url, "--refresh", "carillon_claude,carillon_codex"],
    `${fake.token}\n`,
  );
  expect(discovered.status, discovered.stderr).toBe(0);
  expect(fake.refreshes).toEqual([
    { instanceId: "carillon_claude" },
    { instanceId: "carillon_codex" },
  ]);
  const offer: unknown = JSON.parse(discovered.stdout);
  expect(offer).toMatchObject({ projects: [{ id: "project-1" }] });

  const refused = await cli(["discover", "--url", fake.url], "wrong\n");
  expect(refused.status).toBe(1);
  expect(refused.stderr).toMatch(/^error: T3 refused the token: /u);
  expect(refused.stderr).not.toContain("wrong");

  const { workspace, config } = await installation();
  fake.projects = [{ id: "agent", title: "Agent", workspaceRoot: workspace }];
  const saved = await cli(
    [
      "configure",
      "--config",
      config,
      "--url",
      fake.url,
      "--agent",
      "code",
      "--project",
      "agent",
      "--model",
      "claudeAgent/claude-sonnet-5",
    ],
    `${fake.token}\n`,
  );
  expect(saved.status, saved.stderr).toBe(0);
  // The files are startup settings: nothing claims a running runtime took them.
  expect(saved.stdout).toBe(
    `Saved runtime-t3's settings for code: project Agent (${workspace}) on ${fake.url}, with claudeAgent/claude-sonnet-5. It reads them when it starts, so a running Carillon needs \`carillon restart\`.\n`,
  );

  const registry = await readFile(config, "utf8");
  await writeFile(config, registry.replace('id = "runtime-t3"', 'id = "runtime-other"'));
  const configured = await cli(
    [
      "configure",
      "--config",
      config,
      "--url",
      fake.url,
      "--agent",
      "code",
      "--project",
      "p",
      "--model",
      "a/b",
    ],
    `${fake.token}\n`,
  );
  expect(configured.status).toBe(1);
  expect(configured.stderr).toMatch(/runtime-t3 is not in carillon\.toml/u);
});

it("makes a Carillon profile: a T3 instance whose home links to the assistant's resources", async () => {
  const fake = await fakeT3();
  const { root, workspace, config } = await installation();
  const runtime = await loaded(config);
  const endpoint = { url: fake.url, token: fake.token };
  const home = join(root, "profiles", "claude");
  const choice = { harness: "claude", agentId: "assistant", home, key: "sk-test" } as const;
  expect(await profile(runtime, endpoint, choice)).toEqual({
    instanceId: "carillon_claude",
    home,
  });
  // One copy of Carillon's instructions and skills: the resources'.
  expect(await readlink(join(home, "CLAUDE.md"))).toBe(join(root, "agent", "AGENTS.md"));
  expect(await readlink(join(home, "skills"))).toBe(join(root, "agent", "skills"));
  expect((await stat(join(root, "agent", "skills"))).isDirectory()).toBe(true);
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
    agentId: "assistant",
    home: join(root, "profiles", "codex"),
    key: "proxy-token",
    endpoint: "https://proxy.example",
  });
  expect(fake.instances.get(proxied.instanceId)).toMatchObject({
    driver: "codex",
    environment: [
      { name: "OPENAI_BASE_URL", value: "https://proxy.example", sensitive: false },
      { name: "OPENAI_API_KEY", value: "proxy-token", sensitive: true },
    ],
  });
  expect(await readlink(join(root, "profiles", "codex", "AGENTS.md"))).toBe(
    join(root, "agent", "AGENTS.md"),
  );
  fake.instances.delete(proxied.instanceId);

  // Making it again keeps what is there.
  await profile(runtime, endpoint, { harness: "claude", agentId: "assistant", home });
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
  await expect(
    profile(runtime, endpoint, { harness: "codex", agentId: "assistant", home: join(root, "x") }),
  ).rejects.toThrow(/not this profile/u);
  const taken = join(root, "profiles", "taken");
  await mkdir(taken, { recursive: true });
  await writeFile(join(taken, "AGENTS.md"), "the owner's own");
  await expect(
    profile(runtime, endpoint, {
      harness: "codex",
      agentId: "assistant",
      instanceId: "other",
      home: taken,
    }),
  ).rejects.toThrow(/is not a link to/u);
  expect((await lstat(join(taken, "AGENTS.md"))).isFile()).toBe(true);
  // Only an agent with resources has something to share.
  await expect(
    profile(runtime, endpoint, { harness: "claude", agentId: "code", home }),
  ).rejects.toThrow(/has no resources/u);
});
