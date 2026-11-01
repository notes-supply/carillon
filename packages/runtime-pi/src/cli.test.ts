import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { stringify } from "smol-toml";
import { afterEach, expect, it } from "vitest";
import { TurnActivitySchema, TurnOutcomeSchema } from "@carillon/protocol";
import { startFakeCore, type FakeCore } from "@carillon/runtime/testing";
import { Parse } from "typebox/value";

const token = "pi-cli-test-token";
const hash = createHash("sha256").update(token).digest("hex");
const cleanups: (() => Promise<unknown>)[] = [];
afterEach(async () => {
  for (const cleanup of cleanups.splice(0).toReversed()) await cleanup();
});

function start(args: readonly string[]) {
  const child = spawn(process.execPath, [join(import.meta.dirname, "cli.ts"), ...args], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  cleanups.push(async () => {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGKILL");
      await once(child, "close");
    }
  });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk: Buffer) => (stdout += chunk.toString()));
  child.stderr.on("data", (chunk: Buffer) => (stderr += chunk.toString()));
  const ready = new Promise<void>((resolve, reject) => {
    child.stdout.on("data", () => {
      if (stdout.includes("runtime-pi ready at")) resolve();
    });
    child.once("close", () => reject(new Error(`runtime exited: ${stderr}`)));
  });
  ready.catch(() => undefined);
  return { child, ready, output: () => ({ stdout, stderr }) };
}

async function run(args: readonly string[]) {
  const started = start(args);
  started.ready.then(
    () => started.child.kill("SIGTERM"),
    () => undefined,
  );
  await once(started.child, "close");
  return { status: started.child.exitCode, ...started.output() };
}

async function freePort(): Promise<number> {
  const listener = createServer();
  listener.listen(0, "127.0.0.1");
  await once(listener, "listening");
  const address = listener.address();
  if (address === null || typeof address === "string") throw new Error("no TCP address");
  await new Promise<void>((done) => listener.close(() => done()));
  return address.port;
}

/** An installation of one Pi runtime whose agents' resources are these directories. */
async function installation(resources: Record<string, string>, core = "127.0.0.1:1") {
  const home = await mkdtemp(join(tmpdir(), "carillon-pi-cli-"));
  cleanups.push(() => rm(home, { recursive: true, force: true }));
  const port = await freePort();
  const config = join(home, "carillon.toml");
  const credential = join(home, "runtime.token");
  await writeFile(credential, token);
  await writeFile(
    config,
    stringify({
      data_dir: "state",
      core: { listen: core, operator_token_hash: hash },
      providers: [
        {
          id: "runtime-pi",
          role: "runtime",
          version: "0.0.0",
          url: `http://127.0.0.1:${String(port)}`,
          token_hash: hash,
          enabled: true,
        },
      ],
      agents: Object.entries(resources).map(([id, directory]) => ({
        id,
        name: id,
        runtime: "runtime-pi",
        workspace: home,
        resources: directory,
      })),
    }),
  );
  const args = [
    "--config",
    config,
    "--provider",
    "runtime-pi",
    "--state-dir",
    join(home, "pi"),
    "--token-file",
    credential,
    "--deterministic",
  ];
  return { home, args, url: `http://127.0.0.1:${String(port)}` };
}

it.each([
  { mode: "check", root: "missing" },
  { mode: "check", root: "file" },
  { mode: "startup", root: "missing" },
  { mode: "startup", root: "file" },
])("refuses a $root resource root during $mode for every Pi agent", async ({ mode, root }) => {
  const { home, args } = await installation({ ready: "resources", broken: "broken-resources" });
  await mkdir(join(home, "resources"));
  if (root === "file") await writeFile(join(home, "broken-resources"), "not a directory");
  const result = await run([...args, ...(mode === "check" ? ["--check"] : [])]);
  expect(result.status, result.stderr).toBe(1);
  expect(result.stderr).toContain("agent broken: resources must be an existing directory");
  if (mode === "check") return;
  if (root === "file") await rm(join(home, "broken-resources"));
  await mkdir(join(home, "broken-resources"));
  const recovered = await run(args);
  expect(recovered.status, recovered.stderr).toBe(0);
  expect(recovered.stdout).toContain("runtime-pi ready at");
});

it("refuses to start with an mcp.json it cannot use", async () => {
  const { home, args } = await installation({ default: "resources" });
  await mkdir(join(home, "resources"));
  await mkdir(join(home, "pi"));
  await writeFile(join(home, "pi", "mcp.json"), JSON.stringify({ mcpServers: { notes: {} } }), {
    mode: 0o600,
  });
  const result = await run(args);
  expect(result.status, result.stderr).toBe(1);
  expect(result.stderr).toContain(`${join(home, "pi", "mcp.json")}: server notes must be`);
});

it("passes its check once Pi offers a model, though Pi's settings choose none", async () => {
  const { home, args } = await installation({ default: "resources" });
  await mkdir(join(home, "resources"));
  await mkdir(join(home, "pi"));
  await writeFile(join(home, "pi", "settings.json"), JSON.stringify({}));
  const live = args.filter((arg) => arg !== "--deterministic");
  const unready = await run([...live, "--check"]);
  expect(unready.status, unready.stderr).toBe(1);
  expect(unready.stderr).toContain("Pi is not ready: Pi offers no model");
  await writeFile(
    join(home, "pi", "models.json"),
    JSON.stringify({
      providers: {
        local: {
          baseUrl: "http://127.0.0.1:1/v1",
          api: "openai-completions",
          apiKey: "local",
          models: [{ id: "local-model" }],
        },
      },
    }),
  );
  const ready = await run([...live, "--check"]);
  expect(ready.status, ready.stderr).toBe(0);
  expect(ready.stdout).toBe("Pi is signed in and offers a model.\n");
});

it("sets an agent's model, which it then lists with the models Pi offers", async () => {
  const { home, args } = await installation({ default: "resources", other: "resources" });
  await mkdir(join(home, "resources"));
  const models = async (agent: string) => {
    const listed = await run([...args, "--models", "--agent", agent]);
    expect(listed.status, listed.stderr).toBe(0);
    return JSON.parse(listed.stdout) as unknown;
  };
  expect(await models("default")).toEqual({
    models: [
      { provider: "local", model: "echo", name: "Local echo" },
      { provider: "local", model: "reasoner", name: "Local reasoner" },
      { provider: "substitute", model: "fixed", name: "Substitute fixed" },
    ],
    value: null,
  });
  const configured = await run([
    ...args,
    "--configure",
    "--agent",
    "default",
    "--model",
    "local/reasoner",
  ]);
  expect(configured.status, configured.stderr).toBe(0);
  expect(configured.stdout).toBe("default runs Local reasoner (local/reasoner).\n");
  expect(await models("default")).toMatchObject({ value: "local/reasoner" });
  expect(await models("other")).toMatchObject({ value: null });
  const refused = await run([
    ...args,
    "--configure",
    "--agent",
    "default",
    "--model",
    "local/gone",
  ]);
  expect(refused.status).toBe(1);
  expect(refused.stderr).toContain("local/gone is not a model offered here.");
  expect(await models("default")).toMatchObject({ value: "local/reasoner" });
}, 30_000);

async function configure(args: readonly string[]) {
  const configured = await run([
    ...args,
    "--configure",
    "--agent",
    "default",
    "--model",
    "local/echo",
  ]);
  expect(configured.status, configured.stderr).toBe(0);
}

async function inbox(url: string, home: string, input: string) {
  const response = await fetch(`${url}/v1/inbox`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({
      correlation_id: "turn-1",
      interface_id: "discord-private",
      agent_id: "default",
      audience_id: "conversation",
      channel_id: "300",
      input,
      resource_root: join(home, "resources"),
      attachment_root: join(home, "attachments"),
    }),
  });
  expect(response.status).toBe(202);
}

/** Waits until the held turn's model request runs. */
async function answering(url: string) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    const activity = await fetch(`${url}/v1/turns/turn-1/activity?interface_id=discord-private`, {
      headers: { authorization: `Bearer ${token}` },
    });
    if (
      activity.status === 200 &&
      Parse(TurnActivitySchema, await activity.json()).phase === "model"
    )
      return;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error("the turn never reached its model request");
}

async function outcomes(core: FakeCore) {
  const path = "/v1/runtime/turns/turn-1/outcome";
  for (
    let attempt = 0;
    attempt < 400 && !core.received.some((call) => call.path === path);
    attempt += 1
  )
    await new Promise((resolve) => setTimeout(resolve, 25));
  await new Promise((resolve) => setTimeout(resolve, 100));
  return core.received.filter((call) => call.path === path);
}

it("gives up a message whose answer keeps killing the runtime, and fails its turn", async () => {
  const core: FakeCore = await startFakeCore();
  cleanups.push(() => core.close());
  const { home, args, url } = await installation(
    { default: "resources" },
    core.url.replace("http://", ""),
  );
  await mkdir(join(home, "resources"));
  await configure(args);
  for (let run = 0; run < 5; run += 1) {
    const started = start(args);
    await started.ready;
    if (run === 0) await inbox(url, home, `hold ${join(home, "never")}`);
    await answering(url);
    started.child.kill("SIGKILL");
    await once(started.child, "close");
  }
  await start(args).ready;
  const [outcome, ...more] = await outcomes(core);
  expect(more).toEqual([]);
  expect(outcome?.body).toMatchObject({
    outcome: "failed",
    explanation: "This message stopped the agent's runtime several times, so it was given up.",
  });
}, 30_000);

it("finishes a turn cut off by SIGKILL once it runs again, and settles it once", async () => {
  const core: FakeCore = await startFakeCore();
  cleanups.push(() => core.close());
  const { home, args, url } = await installation(
    { default: "resources" },
    core.url.replace("http://", ""),
  );
  await mkdir(join(home, "resources"));
  await configure(args);
  const marker = join(home, "release");
  const first = start(args);
  await first.ready;
  await inbox(url, home, `hold ${marker}`);
  await answering(url);
  first.child.kill("SIGKILL");
  await once(first.child, "close");
  await start(args).ready;
  await writeFile(marker, "");
  const [outcome, ...more] = await outcomes(core);
  expect(more).toEqual([]);
  const settled = Parse(TurnOutcomeSchema, outcome?.body);
  expect(settled.outcome === "answered" && settled.output).toMatch(
    /^Local provider response 1: hold /u,
  );
}, 30_000);
