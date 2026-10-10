import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadInstallation, type Installation } from "@carillon/runtime/installation";
import { afterEach, expect, it } from "vitest";
import { startFakeT3, type FakeT3 } from "./fake.ts";
import { loadSettings } from "./settings.ts";
import { configure, discover } from "./setup.ts";
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

/** An installation registering runtime-t3, with its agent working in `workspace/`. */
async function installation(): Promise<{ root: string; workspace: string; config: string }> {
  const root = await mkdtemp(join(tmpdir(), "carillon-t3-setup-"));
  cleanups.push(() => rm(root, { recursive: true }));
  const workspace = join(root, "workspace");
  await mkdir(workspace);
  await mkdir(join(root, "credentials"));
  await mkdir(join(root, "providers"));
  const config = join(root, "carillon.toml");
  await writeFile(
    config,
    [
      'data_dir = "state"',
      "[core]",
      `operator_token_hash = "${HASH}"`,
      "[agent]",
      'resource_root = "agent"',
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
  ]);
  await expect(discover({ url: fake.url, token: "wrong" })).rejects.toSatisfy(
    (failure) => failure instanceof T3Failure && failure.kind === "refused",
  );
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
  for (const [choice, refusal] of [
    [{ projectId: "missing", instanceId: "claudeAgent", model: "claude-sonnet-5" }, /no project/u],
    [{ projectId: "elsewhere", instanceId: "claudeAgent", model: "claude-sonnet-5" }, /rooted/u],
    [{ projectId: "agent", instanceId: "cursor", model: "auto" }, /no usable model/u],
  ] as const) {
    await expect(configure(runtime, endpoint, choice)).rejects.toThrow(refusal);
    expect(await readdir(join(root, "credentials"))).toEqual([]);
    expect(await readdir(join(root, "providers"))).toEqual([]);
  }

  const project = await configure(runtime, endpoint, {
    projectId: "agent",
    instanceId: "claudeAgent",
    model: "claude-sonnet-5",
  });
  expect(project.title).toBe("Agent");
  const settings = await loadSettings(runtime.providerFile, root);
  expect(settings).toMatchObject({
    url: fake.url,
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
  const sonnet = { projectId: "agent", instanceId: "claudeAgent", model: "claude-sonnet-5" };
  // Settings the operator tuned as documented.
  await writeFile(
    runtime.providerFile,
    [
      'url = "http://127.0.0.1:1"',
      'token_file = "credentials/t3.token"',
      'project_id = "agent"',
      'title = "Notes"',
      "[model]",
      'instance_id = "claudeAgent"',
      'model = "claude-sonnet-5"',
      "[model.options]",
      'effort = "high"',
      "thinking = true",
    ].join("\n"),
  );

  // Another server with a new token, the same model: everything tuned stays.
  const moved = await fakeT3("rotated-token");
  moved.projects = [{ id: "agent", title: "Agent", workspaceRoot: workspace }];
  await configure(runtime, { url: moved.url, token: moved.token }, sonnet);
  let settings = await loadSettings(runtime.providerFile, runtime.root);
  expect(settings).toMatchObject({
    url: moved.url,
    title: "Notes",
    selection: {
      instanceId: "claudeAgent",
      model: "claude-sonnet-5",
      options: [
        { id: "effort", value: "high" },
        { id: "thinking", value: true },
      ],
    },
  });
  expect((await readFile(settings.tokenFile, "utf8")).trim()).toBe("rotated-token");

  // Another model starts from its own defaults; the title stays.
  await configure(
    runtime,
    { url: moved.url, token: moved.token },
    { ...sonnet, model: "claude-haiku-4-5" },
  );
  settings = await loadSettings(runtime.providerFile, runtime.root);
  expect(settings.title).toBe("Notes");
  expect(settings.selection).toEqual({
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
    'project_id = "agent"',
    "[model]",
    'instance_id = "claudeAgent"',
    'model = "claude-sonnet-5"',
  ];
  for (const malformed of [
    // Not TOML, the line before holding something that must not be repeated.
    'note = "secret-in-file"\ntitle =\n',
    // TOML, but not the runtime's settings.
    [...valid, 'title = ""'].join("\n"),
    [...valid, "[model.options]", "effort = 3"].join("\n"),
  ]) {
    await writeFile(runtime.providerFile, malformed);
    await expect(
      configure(
        runtime,
        { url: fake.url, token: fake.token },
        {
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
  const discovered = await cli(["discover", "--url", fake.url], `${fake.token}\n`);
  expect(discovered.status, discovered.stderr).toBe(0);
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
    `Saved runtime-t3's settings: project Agent (${workspace}) on ${fake.url}, with claudeAgent/claude-sonnet-5. It reads them when it starts, so a running Carillon needs \`carillon restart\`.\n`,
  );

  const registry = await readFile(config, "utf8");
  await writeFile(config, registry.replace('id = "runtime-t3"', 'id = "runtime-other"'));
  const configured = await cli(
    ["configure", "--config", config, "--url", fake.url, "--project", "p", "--model", "a/b"],
    `${fake.token}\n`,
  );
  expect(configured.status).toBe(1);
  expect(configured.stderr).toMatch(/runtime-t3 is not in carillon\.toml/u);
});
