import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const root = await mkdtemp(join(tmpdir(), "carillon-journey-"));
const restoredRoot = await mkdtemp(join(tmpdir(), "carillon-restored-"));
const piState = await mkdtemp(join(tmpdir(), "pi-carillon-"));
const workspace = await mkdtemp(join(tmpdir(), "carillon-workspace-"));
const backup = join(tmpdir(), `carillon-backup-${crypto.randomUUID()}.tar.gz`);
const binary = resolve("target/debug/carillon");
const runtimeEntry = resolve("packages/runtime-pi/src/index.ts");
const packagedRuntime = process.env.CARILLON_RUNTIME_BINARY;
const basePort = 45_000 + (process.pid % 10_000);
let installation = root;
const services = new Map();

try {
  run(
    "init",
    "--root",
    installation,
    "--workspace",
    workspace,
    "--guild-id",
    "100",
    "--bot-user-id",
    "201",
    "--user-id",
    "200",
    "--route",
    "300:always:channel",
    "--route",
    "301:mention:thread",
    "--route",
    "302:always:thread",
    "--base-port",
    String(basePort),
  );
  await startAll();

  const discovered = JSON.parse(run("model", "--config", configPath()));
  assert.equal(discovered.models.length, 2);
  assert.equal(discovered.selected, null);
  const selected = JSON.parse(run("model", "--config", configPath(), "deterministic/concise"));
  assert.equal(selected.requested.model, "deterministic/concise");
  assert.equal(selected.effective.model, "deterministic/concise");

  const denied = await requestSimulation(event("denied", "999", "200", "300", null, "no"));
  assert.equal(denied.status, 403, "other guilds must fail closed");
  const deniedRoute = await requestSimulation(
    event("denied-route", "100", "200", "399", null, "no"),
  );
  assert.equal(deniedRoute.status, 403, "unconfigured channels must fail closed");
  const ignoredMention = await requestSimulation(
    event("ignored", "100", "200", "301", null, "no activation"),
  );
  assert.equal(ignoredMention.status, 204, "mention routes must ignore unmentioned messages");

  const first = await simulate("event-1", "hello");
  assert.equal(first.receipt.state, "queued");
  assert.match(first.delivery.content, /Local provider response 1/u);
  assert.match(first.delivery.content, /native extension inactive; skill inactive/u);
  assert.deepEqual(first.feedback, ["processing", "delivered"]);

  const duplicate = await simulate("event-1", "hello");
  assert.equal(duplicate.receipt.duplicate, true);
  assert.equal(duplicate.delivery, null);

  const unauthenticatedStatus = await fetch(`http://127.0.0.1:${String(basePort)}/v1/status`);
  assert.equal(
    unauthenticatedStatus.status,
    403,
    "operator diagnostics must require authentication",
  );

  const currentProfile = JSON.parse(
    run(
      "model",
      "--config",
      configPath(),
      "--scope",
      "current-conversation",
      "--audience",
      "discord-100-300",
      "deterministic/steady",
    ),
  );
  assert.equal(currentProfile.effective.model, "deterministic/steady");

  run("enable-demo-resources", "--config", configPath());
  const additions = await simulate("event-2", "use the additions");
  assert.match(additions.delivery.content, /Local provider response 2/u);
  assert.match(additions.delivery.content, /native extension active; skill active/u);

  await restart("discord");
  const afterDiscordRestart = await simulate("event-3", "continue after Discord restart");
  assert.match(afterDiscordRestart.delivery.content, /Local provider response 3/u);

  await restart("runtime");
  const afterRuntimeRestart = await simulate("event-4", "continue after runtime restart");
  assert.match(afterRuntimeRestart.delivery.content, /Local provider response 4/u);

  const firstInspection = JSON.parse(
    run("inspect", "--config", configPath(), first.receipt.turn_id),
  );
  const laterInspection = JSON.parse(
    run("inspect", "--config", configPath(), additions.receipt.turn_id),
  );
  assert.equal(firstInspection.audience_id, laterInspection.audience_id);
  assert.equal(laterInspection.state, "completed");
  assert.equal(laterInspection.delivery_state, "delivered");
  assert.ok(laterInspection.providers.some((provider) => provider.id === "runtime-pi"));
  assert.equal(typeof laterInspection.resources.extensions_sha256, "string");

  const threadParent = await simulateRoute(
    "401",
    "301",
    null,
    "<@201> start a thread while preserving <@999>",
  );
  assert.equal(threadParent.conversation_id, "discord-100-401");
  const threadChild = await simulateRoute("thread-child-1", "401", "301", "continue");
  assert.equal(threadChild.conversation_id, threadParent.conversation_id);

  await restart("discord");
  const persistedThread = await simulateRoute(
    "thread-child-2",
    "401",
    "301",
    "continue after adapter restart",
  );
  assert.equal(persistedThread.conversation_id, threadParent.conversation_id);

  const adopted = await simulateRoute("thread-adopt", "402", "301", "<@201> adopt this thread");
  const adoptedLater = await simulateRoute(
    "thread-adopt-later",
    "402",
    "301",
    "continue adopted thread",
  );
  assert.equal(adopted.conversation_id, adoptedLater.conversation_id);

  const alwaysParent = await simulateRoute("403", "302", null, "always-active parent");
  const alwaysChild = await simulateRoute("always-child", "404", "302", "always-active child");
  assert.notEqual(alwaysParent.conversation_id, alwaysChild.conversation_id);

  const channelChild = await requestSimulation(
    event("channel-child", "100", "200", "405", "300", "reject child"),
  );
  assert.equal(channelChild.status, 403, "channel conversations must not inherit child threads");

  await stop("discord");
  const abandoned = await submitCoreEvent("event-delivery-crash", "claim then crash");
  const claimed = await waitForDeliveryClaim("crashed-discord");
  assert.equal(claimed.turn_id, abandoned.turn_id);
  await start("discord");
  const abandonedInspection = JSON.parse(
    run("inspect", "--config", configPath(), abandoned.turn_id),
  );
  assert.equal(abandonedInspection.delivery_state, "uncertain");

  const recent = JSON.parse(
    run("recent", "--config", configPath(), "--event-id", "event-4", "--limit", "5"),
  );
  assert.equal(recent.length, 1);
  assert.equal(recent[0].turn_id, afterRuntimeRestart.receipt.turn_id);

  const status = JSON.parse(run("status", "--config", configPath()));
  assert.ok(status.providers.every((provider) => provider.status === "ready"));
  assert.equal(status.deliveries.uncertain, 1);
  assert.equal(status.execution_profile.requested.model, "deterministic/concise");

  await stopAll();
  run("backup", "--config", configPath(), "--output", backup);
  await rm(restoredRoot, { recursive: true, force: true });
  run("restore", "--input", backup, "--target", restoredRoot);
  installation = restoredRoot;
  await startAll();
  const restored = await simulate("event-5", "continue after restore");
  assert.match(restored.delivery.content, /Local provider response 6/u);

  process.stdout.write(
    `${JSON.stringify(
      {
        admission: "guild, user, and routes fail closed",
        conversations: "channel and thread continuity",
        duplicate: "deduplicated",
        resources: "Pi extensions and skills loaded natively",
        restarts: ["discord", "runtime-pi"],
        abandoned_delivery_claim: "uncertain",
        diagnostics: "authenticated and queryable",
        backup_restore: "continued",
      },
      null,
      2,
    )}\n`,
  );
} finally {
  await stopAll();
  await rm(root, { recursive: true, force: true });
  await rm(restoredRoot, { recursive: true, force: true });
  await rm(backup, { force: true });
  await rm(piState, { recursive: true, force: true });
  await rm(workspace, { recursive: true, force: true });
}

function configPath() {
  return join(installation, "carillon.toml");
}

function credential(name) {
  return join(installation, "credentials", `${name}.token`);
}

async function credentialValue(name) {
  return (await readFile(credential(name), "utf8")).trim();
}

function run(...args) {
  return execFileSync(binary, args, { encoding: "utf8" }).trim();
}

function event(eventId, guildId, userId, channelId, parentChannelId, content) {
  return {
    event_id: eventId,
    guild_id: guildId,
    user_id: userId,
    channel_id: channelId,
    parent_channel_id: parentChannelId,
    content,
  };
}

async function requestSimulation(value) {
  return fetch(`http://127.0.0.1:${String(basePort + 1)}/v1/simulate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(value),
  });
}

async function simulate(eventId, content) {
  return simulateRoute(eventId, "300", null, content);
}

async function simulateRoute(eventId, channelId, parentChannelId, content) {
  const response = await requestSimulation(
    event(eventId, "100", "200", channelId, parentChannelId, content),
  );
  const body = await response.text();
  assert.equal(response.status, 200, body);
  return JSON.parse(body);
}

async function submitCoreEvent(eventId, content) {
  const response = await fetch(`http://127.0.0.1:${String(basePort)}/v1/events`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${await credentialValue("discord-private")}`,
      "x-carillon-provider": "discord-private",
      "content-type": "application/json",
    },
    body: JSON.stringify({
      event_id: eventId,
      agent_id: "default",
      user_id: "200",
      audience_id: "discord-100-300",
      delivery_target_id: "300",
      feedback_target_id: `300:${eventId}`,
      content,
    }),
  });
  const body = await response.text();
  assert.equal(response.status, 200, body);
  return JSON.parse(body);
}

async function waitForDeliveryClaim(instance) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    const response = await fetch(`http://127.0.0.1:${String(basePort)}/v1/deliveries`, {
      headers: {
        authorization: `Bearer ${await credentialValue("discord-private")}`,
        "x-carillon-provider": "discord-private",
        "x-carillon-instance": instance,
      },
    });
    const body = await response.text();
    assert.equal(response.status, 200, body);
    const delivery = JSON.parse(body);
    if (delivery !== null) return delivery;
    await new Promise((resolveWait) => setTimeout(resolveWait, 25));
  }
  throw new Error("delivery did not become ready");
}

function serviceArguments(name) {
  if (name === "core") return [binary, ["core", "--config", configPath()]];
  if (name === "runtime") {
    const args = [
      "--config",
      configPath(),
      "--token-file",
      credential("runtime-pi"),
      "--provider",
      "runtime-pi",
      "--deterministic",
      "--state-dir",
      piState,
    ];
    return packagedRuntime === undefined
      ? [process.execPath, [runtimeEntry, ...args]]
      : [resolve(packagedRuntime), args];
  }
  return [
    binary,
    [
      "discord",
      "--config",
      configPath(),
      "--token-file",
      credential("discord-private"),
      "--deterministic",
    ],
  ];
}

function servicePort(name) {
  return basePort + { core: 0, discord: 1, runtime: 2 }[name];
}

async function start(name) {
  const [program, args] = serviceArguments(name);
  const child = spawn(program, args, { stdio: "ignore" });
  services.set(name, child);
  await waitForHealth(servicePort(name));
}

async function startAll() {
  for (const name of ["runtime", "core", "discord"]) await start(name);
}

async function restart(name) {
  await stop(name);
  await start(name);
}

async function stop(name) {
  const child = services.get(name);
  if (child === undefined) return;
  services.delete(name);
  if (child.exitCode === null) {
    child.kill("SIGTERM");
    await Promise.race([
      new Promise((resolveClose) => child.once("close", resolveClose)),
      new Promise((resolveTimeout) => setTimeout(resolveTimeout, 2_000)),
    ]);
    if (child.exitCode === null) child.kill("SIGKILL");
  }
}

async function stopAll() {
  for (const name of [...services.keys()].reverse()) await stop(name);
}

async function waitForHealth(port) {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    try {
      const response = await fetch(`http://127.0.0.1:${String(port)}/health`);
      if (response.ok) return;
    } catch {}
    await new Promise((resolveWait) => setTimeout(resolveWait, 50));
  }
  throw new Error(`service on port ${String(port)} did not become ready`);
}
