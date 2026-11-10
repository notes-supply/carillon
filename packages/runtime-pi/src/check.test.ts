import { fauxAssistantMessage, fauxToolCall } from "@earendil-works/pi-ai";
import { createCodingTools } from "@earendil-works/pi-durable/tools";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { stringify } from "smol-toml";
import { afterEach, expect, it } from "vitest";
import { startFixture, textsOf } from "./fixture.ts";

const homes: string[] = [];
afterEach(async () => {
  await Promise.all(homes.splice(0).map((home) => rm(home, { recursive: true, force: true })));
});

/** An installation whose Pi agents each have a resource folder holding these files. */
async function installation(agents: Record<string, Record<string, string>>): Promise<string> {
  const home = await mkdtemp(join(tmpdir(), "carillon-pi-check-"));
  homes.push(home);
  const hash = "0".repeat(64);
  await writeFile(
    join(home, "carillon.toml"),
    stringify({
      data_dir: "state",
      core: { listen: "127.0.0.1:1", operator_token_hash: hash },
      providers: [
        {
          id: "runtime-pi",
          role: "runtime",
          version: "0.0.0",
          url: "http://127.0.0.1:2",
          token_hash: hash,
          enabled: true,
        },
      ],
      agents: Object.keys(agents).map((id) => ({
        id,
        name: id,
        runtime: "runtime-pi",
        workspace: home,
        resources: `agents/${id}`,
      })),
    }),
  );
  for (const [id, files] of Object.entries(agents)) {
    await mkdir(join(home, "agents", id), { recursive: true });
    for (const [path, content] of Object.entries(files)) {
      await mkdir(dirname(join(home, "agents", id, path)), { recursive: true });
      await writeFile(join(home, "agents", id, path), content);
    }
  }
  return home;
}

/** Runs the check as the agent's bash runs `carillon runtime-pi check-resources`, under Bun as packaged. */
async function check(
  home: string,
  args: readonly string[] = [],
  agent = "assistant",
  cwd = process.cwd(),
) {
  const command = [join(import.meta.dirname, "cli.ts"), "check-resources", ...args];
  const child = spawn("bun", command, {
    cwd,
    env: { ...process.env, CARILLON_HOME: home, CARILLON_AGENT: agent },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk: Buffer) => (stdout += chunk.toString()));
  child.stderr.on("data", (chunk: Buffer) => (stderr += chunk.toString()));
  const started = Date.now();
  await once(child, "close");
  return { status: child.exitCode, stdout, stderr, seconds: (Date.now() - started) / 1000 };
}

const selecting = (module: string) => ({
  "extensions/mine.ts": module,
  "settings.json": JSON.stringify({ extensions: { add: ["extensions/mine.ts"] } }),
});

it("says the agent's own folder loads", async () => {
  const home = await installation({
    assistant: selecting("const word: string = 'hi'; export default { hooks: [] };"),
  });
  expect(await check(home)).toMatchObject({
    status: 0,
    stdout: `${join(home, "agents/assistant")} loads.\n`,
  });
});

it.each([
  {
    name: "a syntax error, at its place",
    files: selecting("export default { hooks: [ };"),
    problem:
      "The agent's extension extensions/mine.ts could not be loaded: extensions/mine.ts:1:27: Unexpected }",
  },
  {
    name: "settings that are not JSON",
    files: { "settings.json": "{" },
    problem: "The agent's settings.json is not valid JSON: JSON Parse error: Expected '}'",
  },
  {
    name: "an import that never settles, within the runtime's deadline",
    files: selecting("await new Promise(() => {}); export default {};"),
    problem:
      "The agent's extension extensions/mine.ts could not be loaded: its module did not finish importing within 5 seconds",
  },
])(
  "fails on $name with the problem the agent's note would carry",
  async ({ files, problem }) => {
    const home = await installation({ assistant: files });
    expect(await check(home)).toMatchObject({
      status: 1,
      stdout: "",
      stderr: `${join(home, "agents/assistant")} does not load:\n\n${problem}\n`,
    });
  },
  20_000,
);

it("fails when a module ends the process at its top level, which would stop the runtime", async () => {
  const home = await installation({
    assistant: selecting("process.exit(0); export default {};"),
  });
  expect(await check(home)).toMatchObject({
    status: 1,
    stdout: "",
    stderr: `${join(home, "agents/assistant")} did not finish loading: something ended the process (exit code 0), such as a module calling process.exit(). In the runtime, that stops every agent.\n`,
  });
});

it("fails within its bound when a module's top level never ends", async () => {
  const home = await installation({ assistant: selecting("for (;;) {} export default {};") });
  const result = await check(home);
  expect(result).toMatchObject({
    status: 1,
    stderr: `${join(home, "agents/assistant")} did not finish loading within 10 seconds: a module's top-level code may never end. In the runtime, that stops every agent.\n`,
  });
  expect(result.seconds).toBeLessThan(15);
}, 30_000);

it("resolves a module's relative paths where the runtime's service does, not where it is run", async () => {
  const file = `carillon-check-${String(process.pid)}.json`;
  const home = await installation({
    assistant: selecting(
      `import { readFileSync } from "node:fs"; readFileSync("./${file}"); export default {};`,
    ),
  });
  await writeFile(join(home, file), "{}");
  expect(await check(home, [], "assistant", home)).toMatchObject({
    status: 1,
    stderr: expect.stringContaining("ENOENT") as unknown,
  });
});

it("says how it is used", async () => {
  const home = await installation({ assistant: {} });
  expect(await check(home, ["--help"])).toMatchObject({
    status: 0,
    stdout: expect.stringContaining("Usage: carillon runtime-pi check-resources") as unknown,
  });
});

it("checks the agent it runs for by default, and another it names", async () => {
  const home = await installation({
    assistant: selecting("export default {};"),
    helper: { "settings.json": JSON.stringify({ busy: "sometimes" }) },
  });
  expect(await check(home)).toMatchObject({ status: 0 });
  expect(await check(home, ["--agent", "helper"])).toMatchObject({
    status: 1,
    stderr: expect.stringContaining("The agent's settings.json is invalid at /busy") as unknown,
  });
  expect(await check(home, [], "helper")).toMatchObject({
    status: 1,
    stderr: expect.stringContaining("The agent's settings.json is invalid at /busy") as unknown,
  });
  expect(await check(home, ["--agent", "nobody"])).toMatchObject({
    status: 1,
    stderr: "The check could not run: agent nobody does not run on runtime-pi\n",
  });
});

it("names the agent in its shell, where the check reads which agent it runs for", async () => {
  const images = {
    prepare: (bytes: Uint8Array, mimeType: string) =>
      Promise.resolve({ data: Buffer.from(bytes).toString("base64"), mimeType }),
  };
  const agent = await startFixture({
    agents: ["assistant"],
    builtIns: [{ ...createCodingTools({ images }), name: "coding" }],
    defaults: ["coding"],
    answers: [
      fauxAssistantMessage(fauxToolCall("bash", { command: "echo agent=$CARILLON_AGENT" }), {
        stopReason: "toolUse",
      }),
      (context) => fauxAssistantMessage(textsOf(context.messages.at(-1)).join("")),
    ],
  });
  try {
    expect(await agent.ask("turn-1", "Who are you?", { agent_id: "assistant" })).toContain(
      "agent=assistant",
    );
  } finally {
    await agent.close();
  }
});
