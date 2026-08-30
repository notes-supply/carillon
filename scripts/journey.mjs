import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const root = await mkdtemp(join(tmpdir(), "carillon-journey-"));
const restoredRoot = await mkdtemp(join(tmpdir(), "carillon-restored-"));
const piState = await mkdtemp(join(tmpdir(), "pi-carillon-"));
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
    "--guild-id",
    "100",
    "--user-id",
    "200",
    "--base-port",
    String(basePort),
  );
  await startAll();
  const discoveredModels = JSON.parse(run("model", "--config", configPath()));
  assert.equal(discoveredModels.models.length, 2);
  assert.equal(discoveredModels.selected, null, "an unset model must remain unset");
  await stop("runtime");
  await writeFile(
    join(piState, "settings.json"),
    JSON.stringify({ defaultProvider: "missing-provider", defaultModel: "missing-model" }),
  );
  await start("runtime");
  const unavailableModels = JSON.parse(run("model", "--config", configPath()));
  assert.equal(typeof unavailableModels.selected, "string");
  assert.equal(
    unavailableModels.models.some((model) => model.key === unavailableModels.selected),
    false,
    "an unavailable selection must remain selected",
  );
  const selectedModels = JSON.parse(
    run("model", "--config", configPath(), "deterministic/concise"),
  );
  assert.equal(
    selectedModels.models.find((model) => model.key === selectedModels.selected).id,
    "concise",
  );

  const denied = await fetch(`http://127.0.0.1:${String(basePort + 1)}/v1/simulate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(event("denied", "999", "200", "should not run")),
  });
  assert.equal(denied.status, 403, "other guilds must fail closed");

  const first = await simulate("event-1", "hello");
  assert.match(first.delivery.content, /Local provider response 1/u);
  assert.match(first.delivery.content, /native extension inactive; skill inactive/u);

  const duplicate = await simulate("event-1", "hello");
  assert.equal(duplicate.receipt.duplicate, true, "redelivery must not repeat runtime work");
  assert.equal(duplicate.delivery, null);

  const unauthenticatedStatus = await fetch(`http://127.0.0.1:${String(basePort)}/v1/status`);
  assert.equal(
    unauthenticatedStatus.status,
    403,
    "operator diagnostics must require authentication",
  );

  run("component", "--config", configPath(), "guild-command", "enable");
  run("component", "--config", configPath(), "echo-capability", "enable");
  run("enable-demo-resources", "--config", configPath());

  const command = await simulate("event-command-1", "!carillon status");
  assert.match(command.delivery.content, /Carillon 0\.0\.0 \(status\); guild 100; operator 200/u);

  const additions = await simulate("event-2", "use the additions and capability");
  assert.match(additions.delivery.content, /Local provider response 2/u);
  assert.match(additions.delivery.content, /native extension active; skill active/u);
  assert.match(additions.delivery.content, /external echo/u);

  await restart("discord");
  const afterDiscordRestart = await simulate("event-3", "continue after Discord restart");
  assert.match(afterDiscordRestart.delivery.content, /Local provider response 3/u);

  await restart("runtime");
  const afterRuntimeRestart = await simulate("event-4", "continue after runtime restart");
  assert.match(afterRuntimeRestart.delivery.content, /Local provider response 4/u);

  await restart("extension");
  const afterExtensionRestart = await simulate("event-command-2", "!carillon restarted");
  assert.match(afterExtensionRestart.delivery.content, /\(restarted\)/u);

  await restart("capability");
  const afterCapabilityRestart = await simulate("event-5", "use capability after its restart");
  assert.match(afterCapabilityRestart.delivery.content, /Local provider response 5/u);
  assert.match(afterCapabilityRestart.delivery.content, /external echo/u);

  await stop("discord");
  const abandoned = await submitCoreEvent("event-delivery-crash", "claim then crash");
  const claimed = await fetch(`http://127.0.0.1:${String(basePort)}/v1/deliveries`, {
    headers: {
      authorization: `Bearer ${await credentialValue("discord-private")}`,
      "x-carillon-instance": "crashed-discord",
    },
  });
  assert.equal(claimed.status, 200, await claimed.text());
  await start("discord");
  const abandonedInspection = JSON.parse(
    run("inspect", "--config", configPath(), abandoned.turn_id),
  );
  assert.equal(
    abandonedInspection.delivery_state,
    "uncertain",
    "a replacement Discord process must reconcile an abandoned claim",
  );

  const nativeCorrelation = crypto.randomUUID();
  const nativeSession = join(
    installation,
    "agents",
    "default",
    "sessions",
    `${nativeCorrelation}.json`,
  );
  await writeFile(
    nativeSession,
    `${JSON.stringify({
      turns: [{ correlation_id: nativeCorrelation, input: "recover", output: "native result" }],
    })}\n`,
  );
  await writeFile(
    join(
      installation,
      "state",
      "components",
      "runtime-pi",
      "evidence",
      `${nativeCorrelation}.json`,
    ),
    `${JSON.stringify({
      correlation_id: nativeCorrelation,
      state: "processing",
      engine: "deterministic",
      continuation_ref: nativeSession,
      native: { runtime_instance: "crashed-runtime" },
    })}\n`,
  );
  const reconciledEvidence = await fetch(
    `http://127.0.0.1:${String(basePort + 2)}/v1/evidence/${nativeCorrelation}`,
    { headers: { authorization: `Bearer ${await credentialValue("runtime-pi")}` } },
  );
  const reconciledBody = await reconciledEvidence.text();
  assert.equal(reconciledEvidence.status, 200, reconciledBody);
  assert.equal(JSON.parse(reconciledBody).result.output, "native result");

  const inspected = JSON.parse(
    run("inspect", "--config", configPath(), afterCapabilityRestart.receipt.turn_id),
  );
  assert.equal(inspected.state, "completed");
  assert.equal(inspected.delivery_state, "delivered");
  assert.ok(inspected.components.some((component) => component.id === "echo-capability"));
  assert.equal(inspected.capability_activity.length, 1);
  assert.equal(typeof inspected.resources.extensions_sha256, "string");

  const recent = JSON.parse(
    run("recent", "--config", configPath(), "--event-id", "event-5", "--limit", "5"),
  );
  assert.equal(recent.length, 1);
  assert.equal(recent[0].turn_id, afterCapabilityRestart.receipt.turn_id);

  const status = JSON.parse(run("status", "--config", configPath()));
  assert.ok(status.components.every((component) => component.status === "ready"));
  assert.equal(status.turns.interrupted, 0);
  assert.equal(status.deliveries.uncertain, 1);

  await stopAll();
  run("backup", "--config", configPath(), "--output", backup);
  await rm(restoredRoot, { recursive: true, force: true });
  run("restore", "--input", backup, "--target", restoredRoot);
  installation = restoredRoot;
  await startAll();

  const restored = await simulate("event-6", "continue after restore with capability");
  assert.match(restored.delivery.content, /Local provider response 7/u);
  assert.match(restored.delivery.content, /external echo/u);

  process.stdout.write(
    `${JSON.stringify(
      {
        admission: "fail-closed",
        duplicate: "deduplicated",
        command_extension: "active",
        pi_resources: "active",
        capability: "active",
        restarts: ["discord", "runtime-pi", "interface-extension", "capability"],
        abandoned_delivery_claim: "uncertain",
        native_runtime_reconciliation: "completed from session evidence",
        diagnostics: "authenticated and discoverable by event",
        inspected_turn: afterCapabilityRestart.receipt.turn_id,
        backup_restore: "continued at turn 7",
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

function event(eventId, guildId, userId, content) {
  return { event_id: eventId, guild_id: guildId, user_id: userId, channel_id: "300", content };
}

async function simulate(eventId, content) {
  const response = await fetch(`http://127.0.0.1:${String(basePort + 1)}/v1/simulate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(event(eventId, "100", "200", content)),
  });
  const body = await response.text();
  assert.equal(response.status, 200, body);
  return JSON.parse(body);
}

async function submitCoreEvent(eventId, content) {
  const response = await fetch(`http://127.0.0.1:${String(basePort)}/v1/events`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${await credentialValue("discord-private")}`,
      "content-type": "application/json",
    },
    body: JSON.stringify(event(eventId, "100", "200", content)),
  });
  const body = await response.text();
  assert.equal(response.status, 200, body);
  return JSON.parse(body);
}

function serviceArguments(name) {
  if (name === "core") return [binary, ["core", "--config", configPath()]];
  if (name === "runtime") {
    const args = [
      "--config",
      configPath(),
      "--token-file",
      credential("runtime-pi"),
      "--deterministic",
      "--state-dir",
      piState,
    ];
    return packagedRuntime === undefined
      ? [process.execPath, [runtimeEntry, ...args]]
      : [resolve(packagedRuntime), args];
  }
  if (name === "extension")
    return [
      binary,
      [
        "interface-extension",
        "--config",
        configPath(),
        "--token-file",
        credential("guild-command"),
      ],
    ];
  if (name === "capability")
    return [
      binary,
      ["capability", "--config", configPath(), "--token-file", credential("echo-capability")],
    ];
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
  return basePort + { core: 0, discord: 1, runtime: 2, extension: 3, capability: 4 }[name];
}

async function start(name) {
  const [program, args] = serviceArguments(name);
  const child = spawn(program, args, { stdio: "ignore" });
  services.set(name, child);
  await waitForHealth(servicePort(name));
}

async function startAll() {
  for (const name of ["runtime", "extension", "capability", "core", "discord"]) await start(name);
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
