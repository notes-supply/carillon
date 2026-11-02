// Packaged executables: Pi's command line and the durable runtime, each with
// real QuickJS and tools, against controlled local inference.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { startFakeCore } from "../packages/runtime/src/testing.ts";

const release = resolve(
  process.argv[2] ?? assert.fail("usage: node scripts/test-codemode.mjs RELEASE_DIRECTORY"),
);
const root = await mkdtemp(join(tmpdir(), "carillon-packaged-codemode-"));
const token = "test-only-component-token";
const hash = createHash("sha256").update(token).digest("hex");
const observations = [];
let scriptPending = true;
const provider = createServer(async (request, response) => {
  let raw = "";
  for await (const part of request) raw += part;
  const body = JSON.parse(raw);
  observations.push(body);
  const code = `await tools.write({path: "result.txt", content: "before"});
await tools.edit({path: "result.txt", edits: [{oldText: "before", newText: "after"}]});
text(await Promise.allSettled([tools.read({path: "result.txt"}), tools.bash({command: "printf packaged-bash"})]));
store("packaged", "native-worker");`;
  const delta = scriptPending
    ? {
        role: "assistant",
        tool_calls: [
          {
            index: 0,
            id: "packaged-code-call",
            type: "function",
            function: { name: "codemode", arguments: JSON.stringify({ code }) },
          },
        ],
      }
    : { role: "assistant", content: "packaged code mode checked" };
  const finish = scriptPending ? "tool_calls" : "stop";
  scriptPending = !scriptPending;
  const chunk = (delta, finish_reason) =>
    `data: ${JSON.stringify({ id: "fixture-response", object: "chat.completion.chunk", created: 1, model: "code-model", choices: [{ index: 0, delta, finish_reason }] })}\n\n`;
  response.writeHead(200, { "content-type": "text/event-stream" });
  response.end(`${chunk(delta, null)}${chunk({}, finish)}data: [DONE]\n\n`);
});
provider.listen(0, "127.0.0.1");
await once(provider, "listening");
const endpoint = `http://127.0.0.1:${provider.address().port}/v1`;
let active;
/** The outcome core received for a turn, once the runtime posted it. */
async function settled(core, turn, logs) {
  const path = `/v1/runtime/turns/${turn}/outcome`;
  for (const deadline = Date.now() + 20_000; Date.now() < deadline;) {
    const received = core.received.find((call) => call.path === path);
    if (received !== undefined) return received.body;
    await new Promise((done) => setTimeout(done, 50));
  }
  throw new Error(`no outcome of ${turn}: ${logs}`);
}
async function stopChild() {
  if (active && active.exitCode === null) {
    const exited = once(active, "exit");
    active.kill("SIGTERM");
    await exited;
  }
  active = undefined;
}
try {
  for (const kind of ["cli", "runtime"]) {
    const work = join(root, kind);
    const state = join(work, "pi");
    const resources = join(work, "resources");
    await mkdir(state, { recursive: true });
    await mkdir(resources);
    // Pi's own command line runs Pi's default model; the runtime runs its agent's.
    const settings = {
      ...(kind === "cli" ? { defaultProvider: "code-fixture", defaultModel: "code-model" } : {}),
      defaultTools: ["+codemode"],
      codemode: { mode: "only" },
    };
    await writeFile(join(state, "settings.json"), JSON.stringify(settings));
    // The runtime's scripts call its coding tools.
    await writeFile(
      join(resources, "settings.json"),
      JSON.stringify({ extensions: ["coding", "codemode"] }),
    );
    await writeFile(
      join(state, "models.json"),
      JSON.stringify({
        providers: {
          "code-fixture": {
            baseUrl: endpoint,
            api: "openai-completions",
            apiKey: "test-only",
            models: [{ id: "code-model" }],
          },
        },
      }),
    );
    const firstRequest = observations.length;
    let logs = "";
    const env = { ...process.env, PI_OFFLINE: "1", PI_CODING_AGENT_DIR: state };
    if (kind === "cli") {
      active = spawn(
        join(release, "bin/carillon-runtime-pi"),
        ["pi", "--offline", "--no-session", "--mode", "json", "-p", "Run the verification."],
        { cwd: work, env, stdio: ["ignore", "pipe", "pipe"] },
      );
      const timeout = setTimeout(() => active?.kill("SIGKILL"), 30_000);
      active.stdout.on("data", (part) => {
        logs += part;
      });
      active.stderr.on("data", (part) => {
        logs += part;
      });
      const [code] = await once(active, "exit");
      clearTimeout(timeout);
      assert.equal(code, 0, logs);
      active = undefined;
    } else {
      const probe = createServer();
      probe.listen(0, "127.0.0.1");
      await once(probe, "listening");
      const url = `http://127.0.0.1:${probe.address().port}`;
      await new Promise((done) => probe.close(done));
      const core = await startFakeCore();
      const config = join(work, "carillon.toml");
      const tokenFile = join(work, "runtime.token");
      await writeFile(tokenFile, token, { mode: 0o600 });
      await writeFile(
        config,
        `data_dir = "state"\n[core]\nlisten = "${core.url.replace("http://", "")}"\noperator_token_hash = "${hash}"\n[[agents]]\nid = "default"\nname = "Fixture"\nruntime = "runtime-pi"\nworkspace = ${JSON.stringify(work)}\nresources = "resources"\n[[providers]]\nid = "runtime-pi"\nrole = "runtime"\nversion = "0.0.0"\nurl = "${url}"\ntoken_hash = "${hash}"\nenabled = true\n`,
      );
      const runtime = [
        "--config",
        config,
        "--provider",
        "runtime-pi",
        "--token-file",
        tokenFile,
        "--state-dir",
        state,
      ];
      execFileSync(
        join(release, "bin/carillon-runtime-pi"),
        [...runtime, "--configure", "--agent", "default", "--model", "code-fixture/code-model"],
        { cwd: work, env },
      );
      active = spawn(join(release, "bin/carillon-runtime-pi"), runtime, { cwd: work, env });
      active.stdout.on("data", (part) => {
        logs += part;
      });
      active.stderr.on("data", (part) => {
        logs += part;
      });
      const deadline = Date.now() + 20_000;
      while (true) {
        if (active.exitCode !== null) throw new Error(logs);
        if (Date.now() > deadline) throw new Error(`Runtime startup deadline: ${logs}`);
        try {
          if ((await fetch(`${url}/health`)).ok) break;
        } catch {}
        await new Promise((done) => setTimeout(done, 50));
      }
      const result = await fetch(`${url}/v1/inbox`, {
        method: "POST",
        headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
        body: JSON.stringify({
          correlation_id: "packaged-code-check",
          interface_id: "fixture",
          agent_id: "default",
          audience_id: "code-check",
          channel_id: "code-check",
          input: "Run the verification.",
          resource_root: resources,
          attachment_root: join(work, "attachments"),
        }),
        signal: AbortSignal.timeout(20_000),
      });
      assert.equal(result.status, 202, `${await result.clone().text()}\n${logs}`);
      const outcome = await settled(core, "packaged-code-check", logs);
      assert.equal(outcome.output, "packaged code mode checked", logs);
      await stopChild();
      await core.close();
    }
    assert.equal(await readFile(join(work, "result.txt"), "utf8"), "after", logs);
    const sent = observations.slice(firstRequest);
    assert.equal(sent.length, 2, logs);
    const tools = sent[0].tools.map((tool) => tool.function.name);
    if (kind === "cli") {
      assert.deepEqual(tools, ["codemode"]);
      // Pi finds its documentation beside the executable, as it tells the model.
      const system = sent[0].messages.find((message) => message.role === "system")?.content ?? "";
      const docs = { readme: join(release, "bin/README.md"), docs: join(release, "bin/docs") };
      assert.match(system, new RegExp(`Main documentation: ${RegExp.escape(docs.readme)}$`, "mu"));
      assert.match(system, new RegExp(`Additional docs: ${RegExp.escape(docs.docs)}$`, "mu"));
      assert.ok((await stat(docs.readme)).isFile() && (await stat(docs.docs)).isDirectory());
    } else assert.ok(tools.includes("codemode"), tools.join(", "));
    const nested = sent[1].messages
      .filter((message) => message.role === "tool")
      .map((message) => message.content)
      .join("\n");
    assert.match(nested, /Script completed/u, logs);
    assert.match(nested, /packaged-bash/u, logs);
    assert.match(nested, /after/u, logs);
    console.log(
      `${kind}: packaged codemode worker, write/edit/read/bash and model-visible results passed (local inference fixture)`,
    );
  }
} finally {
  await stopChild();
  provider.closeAllConnections();
  await new Promise((done) => provider.close(done));
  await rm(root, { recursive: true, force: true });
}
