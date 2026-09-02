import { createHash } from "node:crypto";
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
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
  await configureOptionalProviderFixtures();
  await startAll();
  const discoveredProfiles = JSON.parse(run("model", "--config", configPath()));
  assert.equal(discoveredProfiles.models.length, 2);
  assert.equal(discoveredProfiles.selected, null, "an unset model must remain unset");
  await stop("runtime");
  await writeFile(
    join(piState, "settings.json"),
    JSON.stringify({ defaultProvider: "missing-provider", defaultModel: "missing-model" }),
  );
  await start("runtime");
  const unavailableProfiles = JSON.parse(run("model", "--config", configPath()));
  assert.equal(unavailableProfiles.selected.requested.model, "missing-provider/missing-model");
  assert.equal(
    unavailableProfiles.selected.effective,
    null,
    "an unavailable requested model must not fabricate an effective model",
  );
  const appliedProfile = JSON.parse(
    run("model", "--config", configPath(), "deterministic/concise"),
  );
  assert.equal(appliedProfile.requested.model, "deterministic/concise");
  assert.equal(appliedProfile.effective.model, "deterministic/concise");

  const denied = await fetch(`http://127.0.0.1:${String(basePort + 1)}/v1/simulate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(event("denied", "999", "200", "300", null, "should not run")),
  });
  assert.equal(denied.status, 403, "other guilds must fail closed");

  const deniedRoute = await fetch(`http://127.0.0.1:${String(basePort + 1)}/v1/simulate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(event("denied-route", "100", "200", "399", null, "should not run")),
  });
  assert.equal(deniedRoute.status, 403, "unconfigured channels must fail closed");

  const ignoredMention = await fetch(`http://127.0.0.1:${String(basePort + 1)}/v1/simulate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(event("ignored", "100", "200", "301", null, "no activation")),
  });
  assert.equal(ignoredMention.status, 204, "mention routes must ignore unmentioned messages");

  const first = await simulate("event-1", "hello");
  assert.match(first.delivery.content, /Local provider response 1/u);
  assert.match(first.delivery.content, /native extension inactive; skill inactive/u);
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

  const duplicate = await simulate("event-1", "hello");
  assert.equal(duplicate.receipt.duplicate, true, "redelivery must not repeat runtime work");
  assert.equal(duplicate.delivery, null);

  const unauthenticatedStatus = await fetch(`http://127.0.0.1:${String(basePort)}/v1/status`);
  assert.equal(
    unauthenticatedStatus.status,
    403,
    "operator diagnostics must require authentication",
  );

  run("provider", "--config", configPath(), "guild-command", "enable");
  run("provider", "--config", configPath(), "echo-capability", "enable");
  run("enable-demo-resources", "--config", configPath());

  const command = await simulate("event-command-1", "!carillon status");
  assert.match(
    command.delivery.content,
    /Carillon 0\.0\.0 \(status\); audience discord-100-300; operator 200/u,
  );

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

  const channelFirst = JSON.parse(run("inspect", "--config", configPath(), first.receipt.turn_id));
  const channelLater = JSON.parse(
    run("inspect", "--config", configPath(), additions.receipt.turn_id),
  );
  assert.equal(
    channelFirst.audience_id,
    channelLater.audience_id,
    "channel routes must share one continuation",
  );
  assert.equal(
    channelLater.native_evidence.execution_profile.requested.model,
    "deterministic/steady",
  );
  assert.equal(
    channelLater.native_evidence.execution_profile.effective.model,
    "deterministic/steady",
  );

  const threadParent = await simulateRoute(
    "401",
    "301",
    null,
    "<@201> start a thread while preserving <@999>",
  );
  assert.equal(threadParent.conversation_id, "discord-100-401");
  assert.deepEqual(threadParent.feedback, ["processing", "delivered"]);
  const threadChild = await simulateRoute("thread-child-1", "401", "301", "continue");
  assert.equal(threadChild.conversation_id, threadParent.conversation_id);

  await restart("discord");
  const persistedThread = await simulateRoute(
    "thread-child-2",
    "401",
    "301",
    "continue after adapter restart",
  );
  assert.equal(
    persistedThread.conversation_id,
    threadParent.conversation_id,
    "Discord-owned thread participation must survive restart",
  );

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

  const channelChild = await fetch(`http://127.0.0.1:${String(basePort + 1)}/v1/simulate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(event("channel-child", "100", "200", "405", "300", "reject child")),
  });
  assert.equal(channelChild.status, 403, "channel conversations must not inherit child threads");

  run("provider", "--config", configPath(), "runtime-pi", "disable");
  const failed = await simulateRoute("failed-before-delivery", "300", null, "cannot run");
  assert.equal(failed.receipt.state, "interrupted");
  assert.deepEqual(failed.feedback, ["processing", "failed"]);
  run("provider", "--config", configPath(), "runtime-pi", "enable");

  await stop("discord");
  const abandoned = await submitCoreEvent("event-delivery-crash", "claim then crash");
  const claimed = await fetch(`http://127.0.0.1:${String(basePort)}/v1/deliveries`, {
    headers: {
      authorization: `Bearer ${await credentialValue("discord-private")}`,
      "x-carillon-provider": "discord-private",
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

  const runtimeEnvironmentResponse = await fetch(
    `http://127.0.0.1:${String(basePort + 2)}/v1/environment`,
    { headers: { authorization: `Bearer ${await credentialValue("runtime-pi")}` } },
  );
  const runtimeEnvironmentBody = await runtimeEnvironmentResponse.text();
  assert.equal(runtimeEnvironmentResponse.status, 200, runtimeEnvironmentBody);
  const runtimeEnvironment = JSON.parse(runtimeEnvironmentBody);
  const queuedCorrelation = crypto.randomUUID();
  await writeFile(
    join(
      installation,
      "state",
      "components",
      "runtime-pi",
      "evidence",
      `${queuedCorrelation}.json`,
    ),
    `${JSON.stringify({
      correlation_id: queuedCorrelation,
      state: "processing",
      native: { runtime_instance: runtimeEnvironment.runtime_instance, phase: "queued" },
    })}\n`,
  );
  const queuedEvidence = await fetch(
    `http://127.0.0.1:${String(basePort + 2)}/v1/evidence/${queuedCorrelation}`,
    { headers: { authorization: `Bearer ${await credentialValue("runtime-pi")}` } },
  );
  const queuedBody = await queuedEvidence.text();
  assert.equal(queuedEvidence.status, 200, queuedBody);
  assert.equal(
    JSON.parse(queuedBody).state,
    "interrupted",
    "inactive queued work owned by this runtime must terminalize",
  );

  const inspected = JSON.parse(
    run("inspect", "--config", configPath(), afterCapabilityRestart.receipt.turn_id),
  );
  assert.equal(inspected.state, "completed");
  assert.equal(inspected.delivery_state, "delivered");
  assert.ok(inspected.providers.some((provider) => provider.id === "echo-capability"));
  assert.equal(inspected.capability_activity.length, 1);
  assert.equal(typeof inspected.resources.extensions_sha256, "string");

  const recent = JSON.parse(
    run("recent", "--config", configPath(), "--event-id", "event-5", "--limit", "5"),
  );
  assert.equal(recent.length, 1);
  assert.equal(recent[0].turn_id, afterCapabilityRestart.receipt.turn_id);

  const status = JSON.parse(run("status", "--config", configPath()));
  assert.ok(status.providers.every((provider) => provider.status === "ready"));
  assert.equal(status.turns.interrupted, 1);
  assert.equal(status.deliveries.uncertain, 1);
  assert.equal(status.execution_profile.requested.model, "deterministic/concise");
  assert.equal(status.execution_profile.effective.model, "deterministic/concise");

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
        admission: "guild, user, and immutable routes fail closed",
        activation: ["mention", "always"],
        conversations: ["thread adoption persisted", "channel continuity"],
        feedback: ["processing", "delivered", "failed", "uncertain"],
        duplicate: "deduplicated",
        command_extension: "active",
        pi_resources: "active",
        capability: "active",
        restarts: ["discord", "runtime-pi", "interface-extension", "capability"],
        abandoned_delivery_claim: "uncertain",
        native_runtime_reconciliation: "completed from session evidence",
        queued_runtime_reconciliation: "inactive work interrupted",
        diagnostics: "authenticated and discoverable by event",
        inspected_turn: afterCapabilityRestart.receipt.turn_id,
        backup_restore: "continued safely",
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

async function configureOptionalProviderFixtures() {
  const extensionToken = "journey-guild-command-token";
  const capabilityToken = "journey-echo-capability-token";
  await writeFile(credential("guild-command"), `${extensionToken}\n`, { mode: 0o600 });
  await writeFile(credential("echo-capability"), `${capabilityToken}\n`, { mode: 0o600 });
  await mkdir(join(installation, "state/components/echo-capability"), {
    recursive: true,
    mode: 0o700,
  });

  const config = await readFile(configPath(), "utf8");
  const releaseMatch = /^release = "([^"]+)"$/mu.exec(config);
  assert.notEqual(releaseMatch, null, "generated configuration must declare its release");
  const emptyBindings = "interface_extensions = []";
  assert.equal(
    config.split(emptyBindings).length,
    2,
    "fresh configuration must have exactly one empty extension binding list",
  );
  const providerFixture = `
[[providers]]
id = "guild-command"
role = "interface-extension"
version = "${releaseMatch[1]}"
url = "http://127.0.0.1:${String(basePort + 3)}"
token_hash = "${createHash("sha256").update(extensionToken).digest("hex")}"
enabled = false

[[providers]]
id = "echo-capability"
role = "capability"
version = "${releaseMatch[1]}"
url = "http://127.0.0.1:${String(basePort + 4)}"
token_hash = "${createHash("sha256").update(capabilityToken).digest("hex")}"
enabled = false

[[agent.interface_extensions]]
interface_provider = "discord-private"
extension_provider = "guild-command"
`;
  await writeFile(
    configPath(),
    `${config.replace(emptyBindings, 'capability_provider = "echo-capability"')}${providerFixture}`,
    { mode: 0o600 },
  );
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

async function simulate(eventId, content) {
  return simulateRoute(eventId, "300", null, content);
}

async function simulateRoute(eventId, channelId, parentChannelId, content) {
  const response = await fetch(`http://127.0.0.1:${String(basePort + 1)}/v1/simulate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(event(eventId, "100", "200", channelId, parentChannelId, content)),
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
  if (name === "extension")
    return [
      binary,
      [
        "interface-extension",
        "--config",
        configPath(),
        "--token-file",
        credential("guild-command"),
        "guild-command",
      ],
    ];
  if (name === "capability")
    return [
      binary,
      [
        "capability",
        "--config",
        configPath(),
        "--token-file",
        credential("echo-capability"),
        "echo-capability",
      ],
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
