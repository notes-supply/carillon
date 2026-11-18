import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { chmod, mkdir, mkdtemp, readdir, readFile, rm, writeFile } from "node:fs/promises";
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
const site = resolve("packages/web/dist");
const PASSWORD = "correct horse battery";
const TAILNET = "chat.example-tailnet.ts.net";
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

async function send(token, chat, event, text, { agent = "assistant", files = [] } = {}) {
  const form = new FormData();
  form.set("text", text);
  for (const [name, bytes, type] of files) form.append("file", new Blob([bytes], { type }), name);
  const path = `/api/conversations/${agent}/${chat}/messages/${event}`;
  return await api(token, path, { method: "PUT", body: form });
}

function viewOf(chat) {
  return { conversations: "50", agent_id: "assistant", audience_id: chat, turns: "50" };
}

async function open(token, view = { conversations: "50" }) {
  const closing = new AbortController();
  const response = await fetch(`${origin}/api/stream?${new URLSearchParams(view).toString()}`, {
    headers: { authorization: `Bearer ${token}` },
    signal: AbortSignal.any([closing.signal, AbortSignal.timeout(30_000)]),
  });
  assert.equal(response.status, 200, `the stream of ${JSON.stringify(view)}`);
  assert.equal(response.headers.get("content-type"), "text/event-stream");
  const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
  let unread = "";
  return {
    async next() {
      for (;;) {
        const end = unread.indexOf("\n\n");
        if (end === -1) {
          const { done, value } = await reader.read();
          if (done) return undefined;
          unread += value;
          continue;
        }
        const event = unread.slice(0, end);
        unread = unread.slice(end + 2);
        // A keep-alive is a comment and has no data.
        if (event.startsWith("data: ")) return JSON.parse(event.slice("data: ".length));
      }
    },
    close() {
      closing.abort();
    },
  };
}

async function shown(token, view, seen) {
  const stream = await open(token, view);
  try {
    for (;;) {
      const message = await stream.next();
      assert.equal(message?.kind, "snapshot", JSON.stringify(message));
      const found = seen(message.snapshot);
      if (found) return found;
    }
  } finally {
    stream.close();
  }
}

async function snapshot(token, view) {
  return await shown(token, view, (first) => first);
}

async function turns(token, chat) {
  const { open: opened } = await snapshot(token, viewOf(chat));
  assert.equal(opened.kind, "recorded", JSON.stringify(opened));
  return opened.turns;
}

async function settled(token, chat) {
  return await shown(
    token,
    viewOf(chat),
    ({ open: opened }) =>
      opened.kind === "recorded" && opened.conversation.at_work === 0 && opened.turns,
  );
}

/** A request whose `Host` and `Origin` a browser, not this script, would choose. */
function raw(path, headers) {
  return new Promise((resolveStatus, reject) => {
    const sent = request(`${origin}${path}`, { headers, agent: false }, (response) => {
      resolveStatus(response.statusCode);
      response.destroy();
    });
    sent.on("error", reject).end();
  });
}

async function install() {
  await mkdir(workspace, { recursive: true });
  const created = ["--interface", "web", "--workspace", workspace, "--base-port", basePort];
  execFileSync(binary, ["--home", installation, "init", ...created.map(String)], { env });
  // Captures here must not depend on the host's free space.
  await writeFile(
    join(installation, "carillon.toml"),
    "\n[core.attachments]\nfree_space_mib = 0\n",
    { flag: "a" },
  );
  // The units name their installation, which `carillon restart` checks.
  const units = join(env.XDG_CONFIG_HOME, "systemd/user");
  await mkdir(units, { recursive: true });
  await writeFile(
    join(units, "carillon-core.service"),
    `[Service]\nEnvironment="CARILLON_HOME=${installation}"\n`,
  );
  await writeFile(join(installation, "providers/web.toml"), `addresses = ["https://${TAILNET}"]\n`);
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
        --token-file '${tokenFile("web")}' --site '${site}' \\
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
    waited=0
    while kill -0 "$pid" 2>/dev/null; do
      waited=$((waited + 1))
      [ "$waited" -lt 600 ] || { echo "$name did not stop within 30 seconds" >&2; exit 1; }
      sleep 0.05
    done
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

function choosePassword(password) {
  const options = { encoding: "utf8", env, input: `${password}\n` };
  const chosen = execFileSync(binary, ["--home", installation, "web", "password"], options);
  assert.match(chosen, /^The web chat's password is set\./u);
}

async function guess(password) {
  const response = await fetch(`${origin}/api/sign-in`, {
    method: "POST",
    headers: { "content-type": "application/json", origin },
    body: JSON.stringify({ password }),
  });
  return { status: response.status, body: await response.json() };
}

async function signIn(password = PASSWORD) {
  const answer = await guess(password);
  assert.equal(answer.status, 200, JSON.stringify(answer.body));
  return answer.body.token;
}

async function testGate(token) {
  const page = await fetch(origin);
  assert.equal(page.status, 200);
  assert.match(await page.text(), /<script type="module" crossorigin src="\.\/index-[\w-]+\.js">/u);
  const policy = page.headers.get("content-security-policy");
  assert.match(policy, /default-src 'none'/u);
  assert.match(policy, /font-src 'self'/u);
  const files = (await readdir(site)).filter((file) => !file.endsWith(".txt"));
  assert.ok(files.includes("style.css") && files.includes("literata.woff2"));
  for (const file of files) {
    assert.equal((await fetch(`${origin}/${file}`)).status, 200, `the page's ${file}`);
  }
  assert.equal(await raw("/", { host: "rebound.example" }), 421);
  const stream = "/api/stream?conversations=50";
  const named = { host: TAILNET, origin: `https://${TAILNET}`, authorization: `Bearer ${token}` };
  assert.equal(await raw(stream, named), 200);
  assert.equal(await raw(stream, { origin: "http://evil.example" }), 403);
  assert.equal((await api("wrong", stream)).status, 401);
  const refused = await fetch(`${origin}/api/stream?conversations=0`, {
    headers: { authorization: `Bearer ${token}` },
  });
  assert.equal(refused.status, 400);
  assert.match(await refused.text(), /a window holds 1 to 500 items/u);
  assert.deepEqual(await snapshot(token, { conversations: "50" }), {
    agents: [{ id: "assistant", name: "Carillon" }],
    limits: { files: 10, file_bytes: 1024 * 1024 * 1024, message_bytes: 2048 * 1024 * 1024 },
    custom_site: false,
    conversations: [],
    more: false,
    open: null,
  });
  const chat = randomUUID();
  const unrecorded = { kind: "unrecorded", key: { agent_id: "assistant", audience_id: chat } };
  assert.deepEqual((await snapshot(token, viewOf(chat))).open, unrecorded);
}

/** A message with a file, answered with a file: both read back by their links. */
async function testFiles(token) {
  const chat = randomUUID();
  const event = `m-${randomUUID()}`;
  const chart = Buffer.concat([Buffer.from("89504e470d0a1a0a", "hex"), Buffer.from("a chart")]);
  await writeFile(join(workspace, "chart.png"), chart);
  const message = `attach ${join(workspace, "chart.png")}`;
  const files = [["notes.txt", "notes", "text/plain"]];
  const unknown = await send(token, chat, event, message, { agent: "another", files });
  assert.deepEqual(
    [unknown.status, unknown.body],
    [422, { error: "Carillon has no agent another." }],
  );
  const sent = await send(token, chat, event, message, { files });
  assert.equal(sent.status, 200, JSON.stringify(sent.body));
  assert.equal(sent.body.duplicate, false);
  const [turn, ...others] = await settled(token, chat);
  assert.deepEqual(others, []);
  assert.equal(turn.turn_id, sent.body.turn_id);
  const { source, progress } = turn;
  assert.deepEqual(
    [source.kind, source.event_id, source.text, source.command],
    ["message", event, message, false],
  );
  assert.deepEqual([progress.state, progress.ending.kind], ["ended", "answered"]);
  assert.match(progress.ending.html, /^<p>Local provider response 1: attach /u);
  assert.match(progress.ending.html, /attachment notes\.txt 5 bytes/u);
  const [note] = source.files;
  assert.deepEqual([note.name, note.size, note.type], ["notes.txt", 5, "application/octet-stream"]);
  const [answered] = progress.ending.files;
  assert.deepEqual([answered.name, answered.size, answered.type], ["chart.png", 15, "image/png"]);
  // A link carries its own authority, as an `<img>` needs.
  const picture = await fetch(`${origin}${answered.url}`);
  assert.equal(picture.headers.get("content-type"), "image/png");
  assert.match(picture.headers.get("content-disposition"), /^inline;/u);
  assert.deepEqual(Buffer.from(await picture.arrayBuffer()), chart);
  const download = await fetch(`${origin}${note.url}`);
  assert.match(download.headers.get("content-disposition"), /^attachment;/u);
  assert.equal(await download.text(), "notes");

  const again = await send(token, chat, event, message, { files });
  assert.deepEqual(again.body, { turn_id: turn.turn_id, duplicate: true });
  assert.equal((await turns(token, chat)).length, 1);
  const { conversations } = await snapshot(token, { conversations: "50" });
  assert.deepEqual(
    conversations.map((listed) => [listed.key, listed.title, listed.at_work]),
    [[{ agent_id: "assistant", audience_id: chat }, message, 0]],
  );
  return { chat, link: answered.url };
}

/** A message that waits behind a running answer offers controls, and one of them is used. */
async function testControls(token) {
  const chat = randomUUID();
  const marker = join(workspace, "release");
  const [held, waiting] = [`m-${randomUUID()}`, `m-${randomUUID()}`];
  const first = await send(token, chat, held, `hold ${marker}`);
  const progressOf = ({ open: opened }, turnId) =>
    opened.kind === "recorded" && opened.turns.find((turn) => turn.turn_id === turnId)?.progress;
  await shown(
    token,
    viewOf(chat),
    (seen) => progressOf(seen, first.body.turn_id)?.state === "running",
  );
  const second = await send(token, chat, waiting, "also this");
  assert.equal(second.status, 200, JSON.stringify(second.body));
  const offered = await shown(token, viewOf(chat), (seen) => {
    const progress = progressOf(seen, second.body.turn_id);
    return progress?.state === "running" && progress.controls.length > 0 && progress;
  });
  assert.deepEqual(offered.controls, ["interrupt", "stop"]);
  assert.equal(offered.activity.phase, "queued");
  assert.equal("controls" in offered.activity, false, "an activity listed controls");
  const stop = { method: "POST", headers: { "content-type": "application/json" } };
  const control = (event) =>
    api(token, `/api/messages/${event}/control`, { ...stop, body: '{"action":"stop"}' });
  assert.equal((await control(waiting)).status, 204);
  const stopping = await shown(
    token,
    viewOf(chat),
    (seen) => progressOf(seen, second.body.turn_id)?.ending?.kind === "stopped" && seen,
  );
  assert.equal(progressOf(stopping, first.body.turn_id).state, "running");
  await writeFile(marker, "");
  const [answered, stopped] = await settled(token, chat);
  assert.equal(answered.progress.ending.kind, "answered");
  assert.equal(stopped.progress.ending.kind, "stopped");
  const late = await control(waiting);
  assert.ok(late.status >= 400 && typeof late.body.error === "string", "a spent control was taken");
  const commands = await api(token, `/api/conversations/assistant/${chat}/commands`);
  assert.equal(commands.status, 200, JSON.stringify(commands.body));
  assert.ok(Array.isArray(commands.body.commands));
  const elsewhere = await api(token, `/api/conversations/another/${chat}/commands`);
  assert.equal(elsewhere.status, 422, JSON.stringify(elsewhere.body));
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
  const listed = async () =>
    (await snapshot(token, { conversations: "50" })).conversations.find(
      (candidate) => candidate.key.audience_id === chat,
    );
  const before = await listed();
  const opened = await runtime("/v1/runtime/turns", {
    interface_id: "web",
    agent_id: "assistant",
    audience_id: chat,
    key: "integration-schedule",
    label: "Scheduled run: morning summary",
  });
  assert.equal(opened.status, 200, await opened.clone().text());
  const { turn_id: turnId } = await opened.json();
  const working = await listed();
  assert.equal(working.at_work, 1);
  assert.ok(working.activity > before.activity, "an opened turn left its conversation's activity");
  const outcome = await runtime(`/v1/runtime/turns/${turnId}/outcome`, {
    outcome: "answered",
    output: "Good *morning*.",
  });
  assert.ok(outcome.ok, await outcome.text());
  const turn = (await settled(token, chat)).at(-1);
  assert.equal(turn.turn_id, turnId);
  assert.deepEqual(turn.source, { kind: "opened", label: "Scheduled run: morning summary" });
  assert.deepEqual(
    [turn.progress.ending.kind, turn.progress.ending.html],
    ["answered", "<p>Good <em>morning</em>.</p>\n"],
  );
}

/** A pasted log over 1 MiB is answered, and so is the next message its runtime is handed. */
async function testLargeMessage(token) {
  const [pasted, next, end] = [randomUUID(), randomUUID(), randomUUID()];
  let log = "";
  for (let line = 0; log.length < 1_100_000; line += 1)
    log += `2026-11-18T09:10:00Z INFO worker ${String(line)} finished a request\n`;
  log += `end of log ${end}\n`;
  for (const [chat, text] of [
    [pasted, log],
    [next, "after the log"],
  ]) {
    const sent = await send(token, chat, `m-${randomUUID()}`, text);
    assert.equal(sent.status, 200, JSON.stringify(sent.body));
  }
  assert.equal((await settled(token, next))[0].progress.ending.kind, "answered");
  const [answered] = await settled(token, pasted);
  assert.equal(answered.progress.ending.kind, "answered");
  assert.ok(answered.progress.ending.html.includes(end), "the pasted log arrived whole");
}

try {
  await install();
  services("start", "runtime", "core", "web");
  for (const name of ["runtime", "core", "web"]) await healthy(name);
  assert.match(carillon("settings", "--agent", "assistant", "model=local/echo"), /Local echo/u);

  assert.match((await guess(PASSWORD)).body.error, /no password yet/u);
  choosePassword(PASSWORD);
  assert.deepEqual(await guess("not the password at all"), {
    status: 401,
    body: { error: "That is not the web chat's password." },
  });
  const token = await signIn();
  await testGate(token);
  const { chat, link } = await testFiles(token);
  await testControls(token);
  await testOpenedTurn(token, chat);
  await testLargeMessage(token);

  for (const listing of [carillon("settings"), carillon("settings", "--interface", "web")])
    assert.match(listing, /^web\n {2}no channels with settings of their own$/mu);
  assert.match(refused("settings", "--interface", "web", chat), /refused with 404/u);

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
  assert.equal((await settled(token, chat)).at(-1).progress.ending.kind, "answered");

  const across = await open(token, viewOf(chat));
  assert.equal((await across.next()).kind, "snapshot");
  services("stop", "core");
  assert.deepEqual(
    [await across.next(), await across.next()],
    [{ kind: "unavailable" }, undefined],
  );
  const away = await open(token);
  assert.deepEqual([await away.next(), await away.next()], [{ kind: "unavailable" }, undefined]);
  const waited = send(token, chat, `m-${randomUUID()}`, "while core was away");
  await new Promise((resolveWait) => setTimeout(resolveWait, 500));
  services("start", "core");
  assert.equal((await waited).status, 200, "a message sent while core was away was lost");
  assert.equal((await settled(token, chat)).at(-1).source.text, "while core was away");

  const signedIn = await open(token);
  assert.equal((await signedIn.next()).kind, "snapshot");
  choosePassword("a second good password");
  assert.deepEqual(
    [await signedIn.next(), await signedIn.next()],
    [{ kind: "signed_out" }, undefined],
  );
  const stream = "/api/stream?conversations=50";
  assert.equal((await api(token, stream)).status, 401);
  assert.equal((await fetch(`${origin}${link}`)).status, 403);
  assert.equal((await guess(PASSWORD)).status, 401, "the old password still signed in");
  const later = await signIn("a second good password");
  services("stop", "web");
  choosePassword(PASSWORD);
  services("start", "web");
  await healthy("web");
  assert.equal((await api(later, stream)).status, 401);
  assert.equal((await snapshot(await signIn(), { conversations: "50" })).conversations.length, 4);
  process.stdout.write(
    "web passed: password sign-in, an address the owner named, a snapshot stream of each view, files both ways, a duplicate and an unknown agent, controls, an opened turn, a pasted log over 1 MiB, provider restart, drained restart, streams and a send across a core stop, a new password with a stream open and with the provider stopped\n",
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
