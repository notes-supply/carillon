/**
 * The web provider with a real core and deterministic Pi, driven as a
 * browser drives it: sign in, send, read, control, restart. Nothing
 * registers a web provider yet, so its entry in `carillon.toml` is written
 * here, and a stand-in for the user service manager runs the three
 * services, so `carillon restart` stops and starts them as it does an
 * installation's.
 */
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { isolateEnvironment } from "../environment.ts";
import { availableBasePort, PORT_OFFSETS } from "./ports.mjs";

const basePort = await availableBasePort();
const origin = `http://127.0.0.1:${String(basePort + PORT_OFFSETS.web)}`;
const core = `http://127.0.0.1:${String(basePort)}`;
const root = await mkdtemp(join(tmpdir(), "carillon-web-"));
const installation = join(root, "installation");
const workspace = join(root, "workspace");
const manager = join(root, "manager");
const binary = resolve("target/debug/carillon");
isolateEnvironment();
// `carillon restart` reads the units' installation here and stops and
// starts the services through the `systemctl` first on its path.
const env = {
  ...process.env,
  XDG_CONFIG_HOME: join(root, "config"),
  PATH: `${manager}:${process.env.PATH}`,
};

function carillon(...args) {
  const options = { encoding: "utf8", env, stdio: ["ignore", "pipe", "pipe"] };
  return execFileSync(binary, ["--home", installation, ...args], options).trim();
}

function refused(...args) {
  try {
    carillon(...args);
  } catch (error) {
    return String(error.stderr);
  }
  throw new Error(`carillon ${args.join(" ")} succeeded`);
}

function services(...args) {
  execFileSync(join(manager, "systemctl"), args, { env, stdio: ["ignore", "ignore", "inherit"] });
}

async function credential(name) {
  return (await readFile(join(installation, "credentials", `${name}.token`), "utf8")).trim();
}

async function healthy(name) {
  const url = `http://127.0.0.1:${String(basePort + PORT_OFFSETS[name])}/health`;
  await until(`${name} to answer`, async () => (await fetch(url).catch(() => null))?.ok);
}

/** The first truthy value `read` gives within ten seconds. */
async function until(what, read) {
  for (let attempt = 0; attempt < 400; attempt += 1) {
    const value = await read();
    if (value) return value;
    await new Promise((resolveWait) => setTimeout(resolveWait, 25));
  }
  throw new Error(`timed out waiting for ${what}`);
}

/** A request as the page makes one, answered with its status and JSON body. */
async function api(token, path, options = {}) {
  const response = await fetch(`${origin}${path}`, {
    ...options,
    headers: { authorization: `Bearer ${token}`, ...options.headers },
  });
  const text = await response.text();
  return { status: response.status, body: text === "" ? null : JSON.parse(text) };
}

async function send(token, conversation, event, text, { agent, files = [] } = {}) {
  const form = new FormData();
  form.set("text", text);
  for (const [name, bytes, type] of files) form.append("file", new Blob([bytes], { type }), name);
  const query = agent === undefined ? "" : `?agent=${agent}`;
  const path = `/api/conversations/${conversation}/messages/${event}${query}`;
  return await api(token, path, { method: "PUT", body: form });
}

async function turns(token, conversation) {
  const read = await api(token, `/api/conversations/${conversation}/turns`);
  assert.equal(read.status, 200, JSON.stringify(read.body));
  return read.body.turns;
}

/** The conversation's turns once none of them is at work, as a page's poll finds out. */
async function settled(token, conversation) {
  await until(`${conversation} to settle`, async () => {
    const { body } = await api(token, "/api/conversations");
    const listed = body.conversations.find((candidate) => candidate.id === conversation);
    return listed !== undefined && listed.at_work === 0;
  });
  return await turns(token, conversation);
}

/** A request whose `Host` and `Origin` a browser, not this script, would choose. */
function raw(path, headers) {
  return new Promise((resolveStatus, reject) => {
    const sent = request(`${origin}${path}`, { headers, agent: false }, (response) => {
      response.resume();
      resolveStatus(response.statusCode);
    });
    sent.on("error", reject).end();
  });
}

async function install() {
  await mkdir(workspace, { recursive: true });
  const created = ["--interface", "discord", "--workspace", workspace, "--base-port", basePort];
  execFileSync(binary, ["--home", installation, "init", ...created.map(String)], { env });
  carillon("provider", "discord-private", "disable");
  carillon("provider", "runtime-pi", "enable");
  const token = randomUUID();
  const hash = createHash("sha256").update(token).digest("hex");
  const version = execFileSync(binary, ["--version"], { encoding: "utf8" }).trim().split(" ")[1];
  await writeFile(join(installation, "credentials/web.token"), `${token}\n`, { mode: 0o600 });
  // Captures here must not depend on the host's free space.
  await writeFile(
    join(installation, "carillon.toml"),
    `
[core.attachments]
free_space_mib = 0

[[providers]]
id = "web"
role = "interface"
program = "web"
version = "${version}"
url = "${origin}"
token_hash = "${hash}"
enabled = true
max_upload_mib = 1024
delivery = "retained"
`,
    { flag: "a" },
  );
  // The units name their installation, which `carillon restart` checks.
  const units = join(env.XDG_CONFIG_HOME, "systemd/user");
  await mkdir(units, { recursive: true });
  await writeFile(
    join(units, "carillon-core.service"),
    `[Service]\nEnvironment="CARILLON_HOME=${installation}"\n`,
  );
  const config = join(installation, "carillon.toml");
  const logs = join(installation, "logs");
  const tokenFile = (name) => join(installation, "credentials", `${name}.token`);
  await mkdir(join(manager, "pids"), { recursive: true });
  await mkdir(logs, { recursive: true });
  await writeFile(
    join(manager, "systemctl"),
    `#!/bin/sh
set -eu
pids='${manager}/pids'
start() {
  for name; do
    case $name in
      core) '${binary}' core --config '${config}' >>'${logs}/core.log' 2>&1 & ;;
      runtime) '${process.execPath}' '${resolve("packages/runtime-pi/src/cli.ts")}' \\
        --config '${config}' --provider runtime-pi --token-file '${tokenFile("runtime-pi")}' \\
        --deterministic --state-dir '${join(installation, "pi")}' >>'${logs}/runtime-pi.log' 2>&1 & ;;
      web) '${resolve("target/debug/carillon-web")}' serve --config '${config}' --provider web \\
        --token-file '${tokenFile("web")}' --site '${resolve("crates/carillon-web/site")}' \\
        >>'${logs}/web.log' 2>&1 & ;;
    esac
    echo "$!" >"$pids/$name"
  done
}
stop() {
  for name; do
    [ -f "$pids/$name" ] || continue
    pid=$(cat "$pids/$name")
    kill "$pid" 2>/dev/null || true
    while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done
    rm -f "$pids/$name"
  done
}
case "$*" in
  '--user show --property=ActiveState --value carillon.target')
    if [ -n "$(ls "$pids")" ]; then echo active; else echo inactive; fi ;;
  '--user show --property=ConsistsOf --value carillon.target')
    echo 'carillon-core.service carillon-runtime-pi.service carillon-web.service' ;;
  '--user start carillon.target') start runtime core web ;;
  '--user stop '*) stop web core runtime ;;
  'start '*) shift; start "$@" ;;
  'stop '*) shift; stop "$@" ;;
  *) exit 2 ;;
esac
`,
  );
  await chmod(join(manager, "systemctl"), 0o755);
}

/** Signs a browser in with the link `carillon web open` prints, which works once. */
async function signIn() {
  const link = carillon("web", "open", "--print");
  assert.match(link, new RegExp(`^${origin}/#sign-in=\\d+\\.[0-9a-f]{32}\\.[0-9a-f]{64}$`, "u"));
  const code = link.split("#sign-in=")[1];
  const trade = () =>
    fetch(`${origin}/api/sign-in`, {
      method: "POST",
      headers: { "content-type": "application/json", origin },
      body: JSON.stringify({ code }),
    });
  const traded = await trade();
  assert.equal(traded.status, 200);
  assert.equal((await trade()).status, 401, "a sign-in code worked twice");
  return (await traded.json()).token;
}

async function testGate(token) {
  const page = await fetch(origin);
  assert.equal(page.status, 200);
  assert.match(await page.text(), /<script type="module" src="app\.js">/u);
  assert.match(page.headers.get("content-security-policy"), /default-src 'none'/u);
  assert.equal((await fetch(`${origin}/app.js`)).status, 200);
  assert.equal(await raw("/", { host: "rebound.example" }), 421);
  assert.equal(await raw("/api/session", { origin: "http://evil.example" }), 403);
  assert.equal((await api("wrong", "/api/session")).status, 401);
  const session = await api(token, "/api/session");
  assert.deepEqual(session.body.agents, [{ id: "assistant", name: "Carillon" }]);
  assert.equal(session.body.custom_site, false);
}

/** A message with a file, answered with a file: both read back by their links. */
async function testFiles(token) {
  const chat = randomUUID();
  const event = `m-${randomUUID()}`;
  const chart = Buffer.concat([Buffer.from("89504e470d0a1a0a", "hex"), Buffer.from("a chart")]);
  await writeFile(join(workspace, "chart.png"), chart);
  const message = `attach ${join(workspace, "chart.png")}`;
  const files = [["notes.txt", "notes", "text/plain"]];
  const unnamed = await send(token, chat, event, message, { files });
  assert.equal(unnamed.status, 404, "a conversation started without an agent");
  const sent = await send(token, chat, event, message, { agent: "assistant", files });
  assert.equal(sent.status, 200, JSON.stringify(sent.body));
  assert.equal(sent.body.duplicate, false);
  const [turn, ...others] = await settled(token, chat);
  assert.deepEqual(others, []);
  assert.equal(turn.id, sent.body.turn_id);
  assert.equal(turn.from.text, message);
  assert.equal(turn.state, "answered");
  assert.match(turn.html, /^<p>Local provider response 1: attach /u);
  assert.match(turn.html, /attachment notes\.txt 5 bytes/u);
  const [note] = turn.from.files;
  assert.deepEqual([note.name, note.size, note.type], ["notes.txt", 5, "application/octet-stream"]);
  const [answered] = turn.files;
  assert.deepEqual([answered.name, answered.size, answered.type], ["chart.png", 15, "image/png"]);
  // A link carries its own authority, as an `<img>` needs.
  const picture = await fetch(`${origin}${answered.url}`);
  assert.equal(picture.headers.get("content-type"), "image/png");
  assert.match(picture.headers.get("content-disposition"), /^inline;/u);
  assert.deepEqual(Buffer.from(await picture.arrayBuffer()), chart);
  const download = await fetch(`${origin}${note.url}`);
  assert.match(download.headers.get("content-disposition"), /^attachment;/u);
  assert.equal(await download.text(), "notes");

  const again = await send(token, chat, event, message, { agent: "assistant", files });
  assert.deepEqual(again.body, { turn_id: turn.id, duplicate: true });
  assert.equal((await turns(token, chat)).length, 1);
  const other = await send(token, chat, `m-${randomUUID()}`, "hello", { agent: "another" });
  assert.equal(other.status, 409, "a conversation changed its agent");
  const [listed] = (await api(token, "/api/conversations")).body.conversations;
  assert.deepEqual([listed.id, listed.agent_id, listed.title], [chat, "assistant", message]);
  return { chat, link: answered.url };
}

/** A message that waits behind a running answer offers controls, and one of them is used. */
async function testControls(token) {
  const chat = randomUUID();
  const marker = join(workspace, "release");
  const [held, waiting] = [`m-${randomUUID()}`, `m-${randomUUID()}`];
  const first = await send(token, chat, held, `hold ${marker}`, { agent: "assistant" });
  const live = async (...ids) => (await api(token, `/api/turns?ids=${ids.join(",")}`)).body.turns;
  await until("the first turn to run", async () => {
    const [turn] = await live(first.body.turn_id);
    return turn?.state === "running";
  });
  const second = await send(token, chat, waiting, "also this");
  assert.equal(second.status, 200, JSON.stringify(second.body));
  const offered = await until("the waiting turn to offer controls", async () => {
    const turn = (await live(first.body.turn_id, second.body.turn_id)).find(
      (candidate) => candidate.id === second.body.turn_id,
    );
    return turn?.state === "queued" && turn.activity?.controls;
  });
  assert.deepEqual(offered, ["interrupt", "stop"]);
  const stop = { method: "POST", headers: { "content-type": "application/json" } };
  const control = (event) =>
    api(token, `/api/messages/${event}/control`, { ...stop, body: '{"action":"stop"}' });
  assert.equal((await control(waiting)).status, 204);
  await until(
    "the stopped turn to end",
    async () => (await turns(token, chat))[1]?.state === "stopped",
  );
  assert.equal((await turns(token, chat))[0].state, "running");
  await writeFile(marker, "");
  const [answered, stopped] = await settled(token, chat);
  assert.equal(answered.state, "answered");
  assert.equal(stopped.state, "stopped");
  assert.equal((await live(first.body.turn_id))[0].state, "ended");
  const late = await control(waiting);
  assert.ok(late.status >= 400 && typeof late.body.error === "string", "a spent control was taken");
  const commands = await api(token, `/api/conversations/${chat}/commands`);
  assert.equal(commands.status, 200, JSON.stringify(commands.body));
  assert.ok(Array.isArray(commands.body.commands));
}

/**
 * A turn the runtime opens appears in the conversation unasked. The
 * deterministic model has no schedule to run, so the turn is opened and
 * answered through core's runtime API with Pi's own credential.
 */
async function testOpenedTurn(token, chat) {
  const runtime = (path, body) =>
    credential("runtime-pi").then((secret) =>
      fetch(`${core}${path}`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${secret}`,
          "x-carillon-provider": "runtime-pi",
          "content-type": "application/json",
        },
        body: JSON.stringify(body),
      }),
    );
  const before = (await api(token, "/api/conversations")).body.conversations.find(
    (listed) => listed.id === chat,
  );
  const opened = await runtime("/v1/runtime/turns", {
    interface_id: "web",
    agent_id: "assistant",
    audience_id: chat,
    key: "integration-schedule",
    label: "Scheduled run: morning summary",
  });
  assert.equal(opened.status, 200, await opened.clone().text());
  const { turn_id: turnId } = await opened.json();
  const working = (await api(token, "/api/conversations")).body.conversations[0];
  assert.deepEqual([working.id, working.at_work], [chat, 1]);
  assert.ok(working.activity > before.activity, "an opened turn changed nothing a poll compares");
  const outcome = await runtime(`/v1/runtime/turns/${turnId}/outcome`, {
    outcome: "answered",
    output: "Good *morning*.",
  });
  assert.ok(outcome.ok, await outcome.text());
  const turn = (await settled(token, chat)).at(-1);
  assert.deepEqual(turn.from, { kind: "runtime", label: "Scheduled run: morning summary" });
  assert.equal(turn.state, "answered");
  assert.equal(turn.html, "<p>Good <em>morning</em>.</p>\n");
}

try {
  await install();
  services("start", "runtime", "core", "web");
  for (const name of ["runtime", "core", "web"]) await healthy(name);
  assert.match(carillon("settings", "--agent", "assistant", "model=local/echo"), /Local echo/u);

  const token = await signIn();
  await testGate(token);
  const { chat, link } = await testFiles(token);
  await testControls(token);
  await testOpenedTurn(token, chat);

  for (const listing of [carillon("settings"), carillon("settings", "--interface", "web")])
    assert.match(listing, /^web\n {2}no channels with settings of their own$/mu);
  assert.match(refused("settings", "--interface", "web", chat), /refused with 404/u);

  // The provider keeps nothing but its key: a restart loses no conversation
  // and signs no browser out.
  const before = await turns(token, chat);
  services("stop", "web");
  services("start", "web");
  await healthy("web");
  assert.deepEqual(await turns(token, chat), before);
  assert.equal((await fetch(`${origin}${link}`)).status, 200);

  // Answers reach a web chat by being recorded, so a drain waits for no
  // page and no provider.
  const status = JSON.parse(carillon("status"));
  assert.deepEqual([status.deliveries.pending, status.deliveries.uncertain], [0, 0]);
  const restarted = carillon("restart");
  assert.match(restarted, /^Core stopped with its runtime workers paused\.$/mu);
  for (const name of ["runtime", "core", "web"]) await healthy(name);
  assert.deepEqual(await turns(token, chat), before);
  const after = await send(token, chat, `m-${randomUUID()}`, "after the restart");
  assert.equal(after.status, 200, JSON.stringify(after.body));
  assert.equal((await settled(token, chat)).at(-1).state, "answered");

  // While core is away the page is told to try again, and a message sent
  // then is core's once it is back.
  services("stop", "core");
  const away = await api(token, "/api/conversations");
  assert.deepEqual(
    [away.status, away.body],
    [503, { error: "Carillon is not answering right now. Try again in a moment." }],
  );
  const across = send(token, chat, `m-${randomUUID()}`, "while core was away");
  await new Promise((resolveWait) => setTimeout(resolveWait, 500));
  services("start", "core");
  assert.equal((await across).status, 200, "a message sent while core was away was lost");
  assert.equal((await settled(token, chat)).at(-1).from.text, "while core was away");

  // Signing out replaces the key file, which the provider reads again for
  // the very next request.
  assert.match(carillon("web", "sign-out"), /Every browser is signed out/u);
  assert.equal((await api(token, "/api/session")).status, 401);
  assert.equal((await fetch(`${origin}${link}`)).status, 403);
  // It needs no provider: one that starts afterwards knows no earlier browser.
  const later = await signIn();
  services("stop", "web");
  assert.match(refused("web", "open", "--print"), /web chat is not running at /u);
  assert.match(carillon("web", "sign-out"), /Every browser is signed out/u);
  services("start", "web");
  await healthy("web");
  assert.equal((await api(later, "/api/session")).status, 401);
  assert.equal((await api(await signIn(), "/api/session")).status, 200);
  process.stdout.write(
    "web passed: sign-in, files both ways, duplicate and agent refusals, controls, an opened turn, provider restart, drained restart, a send across a core stop, sign-out with the provider running and stopped\n",
  );
} catch (error) {
  for (const log of ["core", "runtime-pi", "web"])
    process.stderr.write(
      await readFile(join(installation, `logs/${log}.log`), "utf8").catch(() => ""),
    );
  throw error;
} finally {
  try {
    services("stop", "web", "core", "runtime");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}
