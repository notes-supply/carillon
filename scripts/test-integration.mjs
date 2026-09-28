import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { constants } from "node:fs";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  open,
  readdir,
  readFile,
  rename,
  rm,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

// This fixture uses the bundled providers and simulated model responses.
// Counters/activation markers below are test output, not a required reply format.
const root = await mkdtemp(join(tmpdir(), "carillon-integration-"));
const restoredRoot = await mkdtemp(join(tmpdir(), "carillon-restored-"));
const workspace = await mkdtemp(join(tmpdir(), "carillon-workspace-"));
const backup = join(tmpdir(), `carillon-backup-${crypto.randomUUID()}.tar.gz`);
const binary = resolve(process.env.CARILLON_CORE_BINARY ?? "target/debug/carillon");
const commandsModule = resolve(
  process.env.CARILLON_COMMANDS_MODULE ??
    "target/wasm32-unknown-unknown/wasm/carillon_discord_commands.wasm",
);
const runtimeEntry = resolve("packages/runtime-pi/src/cli.ts");
const packagedRuntime = process.env.CARILLON_RUNTIME_BINARY;
const basePort = 45_000 + (process.pid % 10_000);
let installation = root;
let runtimeHost = "127.0.0.1";
let runtimeConfigPath;
const services = new Map();

try {
  run("--home", installation, "init", "--workspace", workspace, "--base-port", String(basePort));
  // Channel 305 is reserved for the model picker so its turns do not disturb
  // the response counters of the conversations below.
  assert.match(
    configureDiscord(
      installation,
      "100",
      "201",
      "200",
      "300:always:channel",
      "301:mention:thread",
      "302:always:thread",
      "305:always:channel",
    ),
    /providers\/discord-private\.toml$/u,
    "configure must print the provider-owned file it wrote",
  );
  await testConfigurationRefusals();
  await testConfigurationSymlinkUpdates();
  await testProviderVersions();
  await startAll();
  for (const authorization of [
    null,
    "Bearer wrong",
    `Bearer ${await credentialValue("operator")}`,
  ]) {
    const response = await fetch(`http://127.0.0.1:${String(basePort + 1)}/v1/simulate`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        ...(authorization === null ? {} : { authorization }),
      },
      body: JSON.stringify(event("unauthenticated", "100", "200", "402", "301", "<@201> adopt")),
    });
    assert.equal(
      response.status,
      403,
      "simulation must authenticate before participation or runtime work",
    );
  }
  assert.deepEqual(JSON.parse(run("--home", installation, "recent")), []);
  await assert.rejects(
    readFile(join(installation, "state/components/discord-private/participation.json")),
    { code: "ENOENT" },
  );
  await writeFile(join(installation, "pi", "test-private-state"), "private runtime state fixture");

  const denied = await requestSimulation(event("denied", "999", "200", "300", null, "no"));
  assert.equal(denied.status, 403, "other guilds must fail closed");
  const deniedUser = await requestSimulation(event("denied-user", "100", "999", "300", null, "no"));
  assert.equal(deniedUser.status, 403, "users outside the allowlist must fail closed");
  const deniedDm = await requestSimulation(event("denied-dm", null, "200", "300", null, "no"));
  assert.equal(deniedDm.status, 403, "direct messages must fail closed");
  const deniedRoute = await requestSimulation(
    event("denied-route", "100", "200", "399", null, "no"),
  );
  assert.equal(deniedRoute.status, 403, "unconfigured channels must fail closed");
  for (const eventId of ["denied", "denied-user", "denied-dm", "denied-route"]) {
    const turns = JSON.parse(run("--home", installation, "recent", "--event-id", eventId));
    assert.deepEqual(turns, [], "rejected ingress must not create accepted work");
  }
  const ignoredMention = await requestSimulation(
    event("ignored", "100", "200", "301", null, "no activation"),
  );
  assert.equal(ignoredMention.status, 204, "mention routes must ignore unmentioned messages");

  const first = await simulate("event-1", "hello");
  assert.equal(first.receipt.state, "queued");
  assert.match(first.delivery.content, /Local provider response 1/u);
  assert.match(first.delivery.content, /native extension inactive; skill inactive/u);
  assert.match(first.delivery.content, /model local\/echo\.$/u, "turns run on the runtime default");
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

  run("--home", installation, "enable-demo-resources");
  const additions = await simulate("event-2", "use the additions");
  assert.match(additions.delivery.content, /Local provider response 2/u);
  assert.match(additions.delivery.content, /native extension active; skill active/u);

  await restart("discord");
  const afterDiscordRestart = await simulate("event-3", "continue after Discord restart");
  assert.match(afterDiscordRestart.delivery.content, /Local provider response 3/u);

  await restart("runtime");
  const afterRuntimeRestart = await simulate("event-4", "continue after runtime restart");
  assert.match(afterRuntimeRestart.delivery.content, /Local provider response 4/u);

  const firstInspection = JSON.parse(run("--home", installation, "inspect", first.receipt.turn_id));
  const laterInspection = JSON.parse(
    run("--home", installation, "inspect", additions.receipt.turn_id),
  );
  assert.equal(firstInspection.audience_id, laterInspection.audience_id);
  assert.equal(laterInspection.state, "completed");
  assert.equal(laterInspection.delivery_state, "delivered");
  assert.ok(laterInspection.providers.some((provider) => provider.id === "runtime-pi"));
  assert.match(laterInspection.resource_snapshot_id, /^[a-f0-9]{64}$/u);
  assert.notEqual(firstInspection.resource_snapshot_id, laterInspection.resource_snapshot_id);

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
  const abandonedInspection = JSON.parse(run("--home", installation, "inspect", abandoned.turn_id));
  assert.equal(abandonedInspection.delivery_state, "uncertain");

  const recent = JSON.parse(
    run("--home", installation, "recent", "--event-id", "event-4", "--limit", "5"),
  );
  assert.equal(recent.length, 1);
  assert.equal(recent[0].turn_id, afterRuntimeRestart.receipt.turn_id);

  const status = JSON.parse(run("--home", installation, "status"));
  assert.ok(status.providers.every((provider) => provider.status === "ready"));
  assert.equal(status.deliveries.uncertain, 1);

  await stop("runtime");
  await stop("core");
  // A queued observation is not a shutdown barrier: an outage worker can
  // claim again before SIGTERM. Start this backup fixture with unavailable
  // credentials so no iteration can claim the turn, rather than racing a retry.
  const runtimeCredential = credential("runtime-pi");
  const runtimeToken = await readFile(runtimeCredential);
  await writeFile(runtimeCredential, "");
  await start("core");
  const queued = await submitCoreEvent(
    "event-queued-restore",
    "survive runtime outage and restore",
  );
  try {
    const queuedInspection = await waitForTurn(queued.turn_id, "queued");
    assert.equal(queuedInspection.state, "queued");
    await stop("core");
  } finally {
    await writeFile(runtimeCredential, runtimeToken);
  }
  await stopAll();
  // Restore onto the other supported loopback family. This exercises the real
  // Pi listener and authenticated core/runtime requests, including queued work.
  runtimeHost = "[::1]";
  const priorConfig = await readFile(configPath(), "utf8");
  const ipv6Config = priorConfig.replace(
    `http://127.0.0.1:${String(basePort + 2)}`,
    `http://${runtimeHost}:${String(basePort + 2)}`,
  );
  assert.notEqual(ipv6Config, priorConfig);
  await writeFile(configPath(), ipv6Config);
  run("--home", installation, "backup", "--output", backup);
  await rm(restoredRoot, { recursive: true, force: true });
  run("restore", "--input", backup, "--target", restoredRoot);
  // Prove restoration uses archived runtime state, not the original directory.
  await rm(root, { recursive: true, force: true });
  installation = restoredRoot;
  assert.equal(
    await readFile(join(installation, "pi", "test-private-state"), "utf8"),
    "private runtime state fixture",
    "restore must include private runtime state, not just core and session records",
  );
  // Startup-refusal checks inspect the same queued turn repeatedly. Keep core
  // from claiming it at all: an offline runtime alone still permits transient
  // `running` states and makes shutdown race the worker's requeue.
  const restoredRuntimeCredential = credential("runtime-pi");
  await writeFile(restoredRuntimeCredential, "");
  try {
    // The external workspace is deliberately not in the archive. Pi must refuse
    // startup if it disappears, before binding or touching native session state.
    await start("core");
    await rename(workspace, `${workspace}-saved`);
    const sessions = join(installation, "agents/default/sessions");
    await rename(sessions, `${sessions}-saved`);
    try {
      for (const missing of [true, false]) {
        if (!missing) await writeFile(workspace, "not a directory");
        assertStartupRefused("runtime", missing ? /ENOENT/u : /existing directory/u);
        await assert.rejects(stat(sessions), { code: "ENOENT" });
        // Dispatch is paused, so startup refusals must leave accepted work queued.
        assert.equal(
          JSON.parse(run("--home", installation, "inspect", queued.turn_id)).state,
          "queued",
        );
      }
    } finally {
      await rm(workspace, { force: true });
      await rename(`${workspace}-saved`, workspace);
      await rm(sessions, { recursive: true, force: true });
      await rename(`${sessions}-saved`, sessions);
      await stop("core");
    }
    await testResourceStateSeparation(queued.turn_id);
    await testMissingResourceRoot(queued.turn_id);
    await testRuntimeStartupRefusals(queued.turn_id);
  } finally {
    await stop("core");
    await writeFile(restoredRuntimeCredential, runtimeToken);
  }
  await testPiModelReadiness();
  // Both services must interpret relative paths from the canonical config file,
  // not from the directory holding an operator's convenient symlink.
  runtimeConfigPath = join(workspace, "config-link.toml");
  await symlink(configPath(), runtimeConfigPath);
  await startAll();
  await waitForTurn(queued.turn_id, "completed");
  await assert.rejects(stat(join(workspace, "agents")), { code: "ENOENT" });
  const resumedDelivery = await waitForDeliveryClaim("resumed-discord");
  assert.equal(resumedDelivery.turn_id, queued.turn_id);
  await acknowledgeDelivery(resumedDelivery.id, "resumed-discord");
  const resumed = await waitForTurn(queued.turn_id, "completed", "delivered");
  assert.equal(resumed.delivery_state, "delivered");
  const restored = await simulate("event-5", "continue after restore");
  assert.match(restored.delivery.content, /Local provider response 7/u);
  await testModelPicker();
  await testProviderFailure();
  await testActivityVisibility();

  process.stdout.write(
    `${JSON.stringify(
      {
        admission: "guild, user, and routes fail closed",
        conversations: "channel and thread continuity",
        duplicate: "deduplicated",
        resources: "resource snapshots reach the deterministic runtime",
        restarts: ["discord", "runtime-pi"],
        abandoned_delivery_claim: "uncertain",
        diagnostics: "authenticated and queryable",
        runtime_outage: "queued and resumed",
        backup_restore: "continued",
        model_picker: "selections reach the runtime, persist, and never wait for a running turn",
        provider_failure:
          "explained once without replay, pages and credentials withheld, conversation continues",
        activity: "a slow native command is visible to its interface before the answer",
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
  await rm(workspace, { recursive: true, force: true });
}

function configPath() {
  return join(installation, "carillon.toml");
}

function providerConfigPath() {
  return join(installation, "providers", "discord-private.toml");
}

function configureDiscord(home, guildId, botUserId, userId, ...routes) {
  return run(
    "--home",
    home,
    "discord",
    "configure",
    "--provider",
    "discord-private",
    "--guild-id",
    guildId,
    "--bot-user-id",
    botUserId,
    "--user-id",
    userId,
    ...routes.flatMap((route) => ["--route", route]),
  );
}

function assertStartupRefused(name, expected, extraArgs = [], options = {}) {
  const [program, args] = serviceArguments(name, options);
  const result = spawnSync(program, [...args, ...extraArgs], {
    encoding: "utf8",
    timeout: 10_000,
    env: options.env,
  });
  assert.doesNotMatch(result.stdout, /ready at/u, `${name} must refuse before listening`);
  assert.ifError(result.error);
  assert.equal(result.signal, null);
  assert.notEqual(result.status, 0, `${name} must refuse startup`);
  assert.match(result.stderr, expected);
}

async function testModelPicker() {
  const conversation = "discord-100-305";
  const shown = await interact("305", null, "command", "model");
  assert.match(shown.content, /^\*\*Model settings\*\*\n/u);
  assert.match(
    shown.content,
    /This conversation: Local echo \(`local\/echo`\) \(follows the agent default\)/u,
  );
  assert.match(shown.content, /Agent default: Local echo \(`local\/echo`\) \(runtime default\)/u);
  assert.deepEqual(customIds(shown), [
    providerControl(shown, "conversation"),
    "model:model:conversation",
    "model:scope:conversation",
  ]);
  assert.equal(selectedLabel(shown, providerControl(shown, "conversation")), "local");
  assert.deepEqual(buttons(shown), [
    "model:scope:conversation",
    "model:scope:agent",
    "model:default:conversation",
  ]);
  // Only the browsed provider's models are offered. Identities remain opaque.
  assert.deepEqual(
    options(shown, providerControl(shown, "conversation")).map((option) => option.label),
    ["local", "substitute"],
  );
  assert.deepEqual(
    options(shown, "model:model:conversation").map((option) => option.description),
    ["local/echo", "local/reasoner"],
  );
  assert.ok(
    options(shown, "model:model:conversation").every((option) =>
      /^[0-9a-f]{16}$/u.test(option.value),
    ),
  );
  assert.equal(selectedLabel(shown, "model:model:conversation"), "Local echo");
  assert.equal(control(shown, "model:default:conversation").disabled, true);
  // Outside a conversation only the agent default is offered.
  const outside = await interact("301", null, "command", "model");
  assert.match(outside.content, /Use `\/model` inside a conversation to set a model for it\./u);
  assert.deepEqual(customIds(outside), [
    providerControl(outside, "agent"),
    "model:model:agent",
    "model:default:agent",
  ]);

  // Browsing another provider cannot apply a model or disturb inheritance.
  const provider = providerControl(shown, "conversation");
  const browsed = await interact("305", null, "component", provider, [
    value(shown, provider, "substitute"),
  ]);
  assert.equal(selectedLabel(browsed, providerControl(browsed, "conversation")), "substitute");
  assert.deepEqual(
    options(browsed, "model:model:conversation").map((option) => option.description),
    ["substitute/fixed"],
  );
  assert.match(browsed.content, /This conversation: Local echo/u);
  assert.match(browsed.content, /follows the agent default/u);
  assert.match(browsed.content, /Agent default: Local echo/u);
  const stillActive = await simulateRoute("model-browse", "305", null, "which model");
  assert.match(stillActive.delivery.content, /model local\/echo\.$/u);
  const reopened = await interact("305", null, "command", "model");
  assert.equal(selectedLabel(reopened, providerControl(reopened, "conversation")), "local");

  // Choosing a model shows the settings it will run with, defaults included,
  // and a turn runs with exactly those.
  const chosen = await interact("305", null, "component", "model:model:conversation", [
    value(shown, "model:model:conversation", "local/reasoner"),
  ]);
  assert.match(
    chosen.content,
    /This conversation: Local reasoner \(`local\/reasoner`\), Thinking: medium \(set for this conversation\)/u,
  );
  const thinking = customIds(chosen).find((id) => id.startsWith("model:option:conversation:"));
  assert.ok(thinking, "the reasoner's thinking setting is offered");
  assert.deepEqual(
    options(chosen, thinking).map((option) => option.label),
    ["off", "low", "medium", "high"],
  );
  assert.equal(selectedLabel(chosen, thinking), "medium");
  const defaulted = await simulateRoute("model-0", "305", null, "which model");
  assert.equal(defaulted.conversation_id, conversation);
  assert.match(defaulted.delivery.content, /model local\/reasoner \(thinking=medium\)\.$/u);

  const reasoner =
    /Local reasoner \(`local\/reasoner`\), Thinking: high \(set for this conversation\)/u;
  const tuned = await interact("305", null, "component", thinking, [
    value(chosen, thinking, "high"),
  ]);
  assert.match(tuned.content, reasoner);
  assert.equal(selectedLabel(tuned, thinking), "high");
  assert.equal(control(tuned, "model:default:conversation").disabled, false);
  const overridden = await simulateRoute("model-1", "305", null, "which model");
  assert.match(overridden.delivery.content, /model local\/reasoner \(thinking=high\)\.$/u);
  // A control left over from an earlier catalogue is reported, not misapplied.
  const staleSetting = await interact("305", null, "component", thinking, ["0000000000000000"]);
  assert.match(staleSetting.content, /That setting is no longer available/u);
  assert.match(staleSetting.content, reasoner);
  const staleModel = await interact("305", null, "component", "model:model:conversation", [
    "0000000000000000",
  ]);
  assert.match(staleModel.content, /That model is no longer available/u);
  assert.match(staleModel.content, reasoner);

  // Core must relay the runtime's atomic precondition, not discard it.
  const staleWrite = await fetch(`http://127.0.0.1:${String(basePort)}/v1/model`, {
    method: "PUT",
    headers: {
      authorization: `Bearer ${await credentialValue("discord-private")}`,
      "x-carillon-provider": "discord-private",
      "content-type": "application/json",
    },
    body: JSON.stringify({
      agent_id: "default",
      audience_id: conversation,
      scope: "conversation",
      selection: { provider: "local", model: "echo", options: {} },
      expected: { provider: "local", model: "reasoner", options: { thinking: "medium" } },
    }),
  });
  assert.equal(staleWrite.status, 409, await staleWrite.text());
  const unchanged = await interact("305", null, "command", "model");
  assert.match(unchanged.content, reasoner);

  // The agent default reaches conversations without a choice of their own.
  const agentScope = await interact("305", null, "component", "model:scope:agent");
  assert.match(agentScope.content, /Changing the agent selection:/u);
  assert.deepEqual(customIds(agentScope), [
    providerControl(agentScope, "agent"),
    "model:model:agent",
    "model:scope:conversation",
  ]);
  const agentProvider = providerControl(agentScope, "agent");
  const agentBrowse = await interact("305", null, "component", agentProvider, [
    value(agentScope, agentProvider, "substitute"),
  ]);
  assert.match(agentBrowse.content, reasoner);
  assert.match(agentBrowse.content, /Agent default: Local echo/u);
  const agentSet = await interact("305", null, "component", "model:model:agent", [
    value(agentBrowse, "model:model:agent", "substitute/fixed"),
  ]);
  assert.match(
    agentSet.content,
    /Agent default: Substitute fixed \(`substitute\/fixed`\) \(set for this agent\)/u,
  );
  assert.match(agentSet.content, reasoner);
  const elsewhere = await simulateRoute("model-2", "406", "302", "which model");
  assert.notEqual(elsewhere.conversation_id, conversation);
  assert.match(elsewhere.delivery.content, /model substitute\/fixed\.$/u);
  const kept = await simulateRoute("model-3", "305", null, "which model");
  assert.match(kept.delivery.content, /model local\/reasoner \(thinking=high\)\.$/u);

  // Selections live in the runtime's ledger, not in a process.
  await restart("runtime");
  const persisted = await interact("305", null, "command", "model");
  assert.match(persisted.content, reasoner);
  assert.match(
    persisted.content,
    /Agent default: Substitute fixed \(`substitute\/fixed`\) \(set for this agent\)/u,
  );

  // Each scope returns to its default separately.
  const followsAgent = await interact("305", null, "component", "model:default:conversation");
  assert.match(
    followsAgent.content,
    /This conversation: Substitute fixed \(`substitute\/fixed`\) \(follows the agent default\)/u,
  );
  const runtimeDefault = await interact("305", null, "component", "model:default:agent");
  assert.match(
    runtimeDefault.content,
    /Agent default: Local echo \(`local\/echo`\) \(runtime default\)/u,
  );
  assert.match(
    runtimeDefault.content,
    /This conversation: Local echo \(`local\/echo`\) \(follows the agent default\)/u,
  );
  const reset = await simulateRoute("model-4", "305", null, "which model");
  assert.match(reset.delivery.content, /model local\/echo\.$/u);

  // A change made while a turn runs is confirmed at once; the running turn
  // keeps the selection it started with and the next turn picks up the change.
  const held = await holdTurn("held-turn");
  const during = await interact("305", null, "component", "model:model:agent", [
    value(shown, "model:model:conversation", "local/reasoner"),
  ]);
  assert.match(
    during.content,
    /Agent default: Local reasoner \(`local\/reasoner`\), Thinking: medium \(set for this agent\)/u,
  );
  assert.match(
    (await interact("305", null, "command", "model")).content,
    /Agent default: Local reasoner \(`local\/reasoner`\), Thinking: medium \(set for this agent\)/u,
  );
  const released = await held.release();
  assert.match(released.output, /model local\/echo\.$/u, "the running turn keeps its selection");
  const next = await simulateRoute("model-5", "305", null, "which model");
  assert.match(next.delivery.content, /model local\/reasoner \(thinking=medium\)\.$/u);
  await interact("305", null, "component", "model:default:agent");

  // Interactions are admitted like messages and never reach the module otherwise.
  for (const [changes, message] of [
    [{ guild_id: "999" }, "other guilds must fail closed"],
    [{ guild_id: null }, "direct messages must fail closed"],
    [{ user_id: "999" }, "users outside the allowlist must fail closed"],
    [{ channel_id: "399" }, "unconfigured channels must fail closed"],
  ]) {
    const response = await requestInteraction({
      ...interaction("305", null, "command", "model", []),
      ...changes,
    });
    assert.equal(response.status, 403, message);
  }
  const unauthenticated = await fetch(`http://127.0.0.1:${String(basePort + 1)}/v1/interactions`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${await credentialValue("operator")}`,
    },
    body: JSON.stringify(interaction("305", null, "command", "model", [])),
  });
  assert.equal(unauthenticated.status, 403, "interactions require the provider credential");
  await testCommandModuleOperations();
}

/**
 * The operator replaces the running command module and returns to the
 * release's; every installed module keeps serving through the same Wasm host.
 */
async function testCommandModuleOperations() {
  const commands = (...args) => run("--home", installation, "discord", "commands", ...args);
  assert.equal(commands(), "discord-private serves the release command module: /model.");
  assert.match(commands("check", commandsModule), /is a usable command module .*: \/model\.$/u);
  assert.equal(
    commands("install", commandsModule),
    "discord-private now serves the installed command module: /model.",
  );
  assert.match((await interact("305", null, "command", "model")).content, /Agent default:/u);
  // Repeating an install finishes its save; it serves the same module.
  assert.equal(
    commands("install", commandsModule),
    "discord-private was already serving the installed command module: /model.",
  );
  const broken = join(workspace, "broken-commands.wasm");
  await writeFile(broken, "\0asm broken");
  for (const args of [
    ["check", broken],
    ["install", broken],
  ]) {
    const refused = spawnSync(binary, ["--home", installation, "discord", "commands", ...args], {
      encoding: "utf8",
    });
    assert.notEqual(refused.status, 0, `${args[0]} accepted a broken module`);
    assert.match(refused.stderr, /^error: .*not a usable command module/u);
    if (args[0] === "install") {
      assert.match(refused.stderr, /Still serving the installed command module: \/model\.$/mu);
    }
  }
  await rm(broken);
  assert.equal(commands(), "discord-private serves the installed command module: /model.");
  assert.equal(commands("reset"), "discord-private now serves the release command module: /model.");
  assert.match((await interact("305", null, "command", "model")).content, /Agent default:/u);
}

/**
 * Starts a deterministic turn directly on the runtime and holds it at a
 * resource read: the extension file is a pipe that yields nothing until
 * `release` writes to it.
 */
async function holdTurn(correlationId) {
  const fixture = join(workspace, `resources-${correlationId}`);
  await mkdir(join(fixture, "extensions"), { recursive: true });
  const pipe = join(fixture, "extensions", "carillon-context.js");
  execFileSync("mkfifo", [pipe]);
  const response = fetch(`http://${runtimeHost}:${String(basePort + 2)}/v1/turns`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${await credentialValue("runtime-pi")}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      correlation_id: correlationId,
      interface_id: "discord-private",
      agent_id: "default",
      audience_id: `discord-100-${correlationId}`,
      input: "held",
      resource_root: fixture,
    }),
  });
  response.catch(() => undefined);
  // The pipe accepts a writer once the runtime has opened it to read.
  let writer;
  for (let attempt = 0; writer === undefined && attempt < 400; attempt += 1) {
    try {
      writer = await open(pipe, constants.O_WRONLY | constants.O_NONBLOCK);
    } catch (failure) {
      if (failure.code !== "ENXIO") throw failure;
      await new Promise((resolveWait) => setTimeout(resolveWait, 25));
    }
  }
  assert.ok(writer, "the held turn did not reach its resource read");
  return {
    async release() {
      await writer.write("held");
      await writer.close();
      const settled = await response;
      const body = await settled.text();
      assert.equal(settled.status, 200, body);
      return JSON.parse(body);
    },
  };
}

async function testResourceStateSeparation(turnId) {
  const original = await readFile(configPath(), "utf8");
  const state = join(installation, "state");
  const resources = join(installation, "agents/default");
  const alias = join(installation, "state-alias");
  const database = await readFile(join(state, "carillon.sqlite3"));
  for (const subtree of ["extensions", "skills"]) {
    const nested = join(resources, subtree, "core-state");
    await mkdir(join(resources, subtree), { recursive: true });
    await rename(state, nested);
    await symlink(nested, alias);
    try {
      for (const path of [
        `agents/default/${subtree}/core-state`,
        `agents/default/${subtree}/../${subtree}/core-state`,
        nested,
        "state-alias",
      ]) {
        await writeFile(
          configPath(),
          original.replace('data_dir = "state"', `data_dir = "${path}"`),
        );
        for (const service of ["core", "discord"])
          assertStartupRefused(service, /data_dir must not be inside selected agent resources/u);
        assert.deepEqual(await readFile(join(nested, "carillon.sqlite3")), database);
        await assert.rejects(stat(state), { code: "ENOENT" });
      }
    } finally {
      await writeFile(configPath(), original);
      await rm(alias);
      await rename(nested, state);
    }
  }
  // Non-selected siblings remain valid: the resource root need not be isolated
  // from every kind of state (Pi itself keeps sessions beneath that root).
  const sibling = join(resources, "core-state");
  await rename(state, sibling);
  try {
    await writeFile(
      configPath(),
      original.replace('data_dir = "state"', 'data_dir = "agents/default/core-state"'),
    );
    await start("core");
    assert.equal(JSON.parse(run("--home", installation, "inspect", turnId)).state, "queued");
  } finally {
    await stop("core");
    await writeFile(configPath(), original);
    await rename(sibling, state);
  }
}

async function testPiModelReadiness() {
  const privateState = join(installation, "pi");
  const sessions = join(installation, "agents/default/sessions");
  await rename(privateState, `${privateState}-saved`);
  await rename(sessions, `${sessions}-saved`);
  await mkdir(privateState);
  // Pi's own offline switch keeps catalog refresh local, without substituting
  // ModelRuntime or borrowing the installing user's model credentials.
  const options = { nativePi: true, env: { ...process.env, PI_OFFLINE: "1" } };
  const settingsPath = join(privateState, "settings.json");
  const modelsPath = join(privateState, "models.json");
  const provider = {
    baseUrl: "http://127.0.0.1:1/v1",
    api: "openai-completions",
    models: [{ id: "readiness-model" }],
  };
  try {
    for (const [settings, configured, message] of [
      [{}, provider, /no Pi model is selected/u],
      [
        { defaultProvider: "readiness-fixture", defaultModel: "missing-model" },
        { ...provider, apiKey: "test-only" },
        /selected Pi model readiness-fixture\/missing-model is not available/u,
      ],
      [
        { defaultProvider: "readiness-fixture", defaultModel: "readiness-model" },
        provider,
        /selected Pi model readiness-fixture\/readiness-model is not available/u,
      ],
    ]) {
      await writeFile(settingsPath, JSON.stringify(settings));
      await writeFile(
        modelsPath,
        JSON.stringify({ providers: { "readiness-fixture": configured } }),
      );
      assertStartupRefused("runtime", message, [], options);
      assert.deepEqual(await readdir(sessions), [], "setup failure must not create a native turn");
    }
    // Core stays stopped throughout this runtime-only readiness check, so no
    // queued work can reach the intentionally non-listening model endpoint.
    await writeFile(
      modelsPath,
      JSON.stringify({
        providers: { "readiness-fixture": { ...provider, apiKey: "test-only" } },
      }),
    );
    await start("runtime", options);
    const response = await fetch(`http://${runtimeHost}:${String(basePort + 2)}/health`);
    assert.equal(response.status, 200);
    assert.equal((await response.json()).provider, "pi");
    assert.deepEqual(await readdir(sessions), []);
    await stop("runtime");
  } finally {
    await stop("runtime");
    await rm(privateState, { recursive: true, force: true });
    await rm(sessions, { recursive: true, force: true });
    await rename(`${privateState}-saved`, privateState);
    await rename(`${sessions}-saved`, sessions);
  }
}

/**
 * Native Pi against a local model endpoint that fails twice, then answers: a
 * provider's explanation reaches the conversation, a raw page does not, and
 * neither failure is replayed or blocks the next message.
 */
async function testProviderFailure() {
  const privateState = join(installation, "pi");
  const sessions = join(installation, "agents/default/sessions");
  const key = `sk-proj-${"Ab1".repeat(20)}`;
  const failures = [
    {
      type: "application/json",
      body: JSON.stringify({
        error: {
          message: `The usage limit has been reached for @everyone and <@200> (request ${key}). See https://example.invalid/limits`,
          type: "usage_limit",
        },
      }),
    },
    {
      type: "text/html",
      body: `<html><body>Gateway refused ${key}</body></html>`,
    },
    {
      type: "application/json",
      body: JSON.stringify({ error: { message: "Authentication failed: password=short-secret" } }),
    },
  ];
  let requests = 0;
  const provider = createServer((request, response) => {
    request.resume();
    const failure = failures[requests];
    requests += 1;
    if (failure !== undefined) {
      response.writeHead(403, { "content-type": failure.type });
      response.end(failure.body);
      return;
    }
    const chunk = (delta, finish) =>
      `data: ${JSON.stringify({
        id: "recovered",
        object: "chat.completion.chunk",
        created: 0,
        model: "failure-model",
        choices: [{ index: 0, delta, finish_reason: finish }],
      })}\n\n`;
    response.writeHead(200, { "content-type": "text/event-stream" });
    response.end(
      `${chunk({ role: "assistant", content: "Recovered after the provider failures." }, null)}${chunk({}, "stop")}data: [DONE]\n\n`,
    );
  });
  provider.listen(0, "127.0.0.1");
  await once(provider, "listening");
  await stop("runtime");
  await rename(privateState, `${privateState}-saved`);
  await rename(sessions, `${sessions}-saved`);
  await mkdir(privateState);
  const log = [];
  try {
    await writeFile(
      join(privateState, "settings.json"),
      JSON.stringify({ defaultProvider: "failure-fixture", defaultModel: "failure-model" }),
    );
    await writeFile(
      join(privateState, "models.json"),
      JSON.stringify({
        providers: {
          "failure-fixture": {
            baseUrl: `http://127.0.0.1:${String(provider.address().port)}/v1`,
            api: "openai-completions",
            apiKey: "test-only",
            models: [{ id: "failure-model" }],
          },
        },
      }),
    );
    await start("runtime", { nativePi: true, env: { ...process.env, PI_OFFLINE: "1" }, log });

    const explained = await simulate("provider-failure-explained", "use the provider");
    assert.equal(explained.delivery.turn_state, "interrupted");
    const [reason, next] = explained.delivery.content.split("\n");
    assert.match(
      reason,
      /^That turn was interrupted: .*The usage limit has been reached for @everyone and <@200> \(request \[redacted\]\)\. See \[link\]$/u,
    );
    assert.equal(
      next,
      "Send another message to continue in this conversation. Actions already taken may have had effects.",
    );
    const inspection = JSON.parse(
      run("--home", installation, "inspect", explained.receipt.turn_id),
    );
    assert.equal(inspection.state, "interrupted");
    assert.equal(
      inspection.error,
      `runtime reported failure: ${reason.split(": ").slice(1).join(": ")}`,
    );

    // A raw page, and an explanation carrying a credential, show the generic notice.
    const withheld = [];
    for (const attempt of ["page", "credential"]) {
      const { delivery } = await simulate(`provider-failure-${attempt}`, `try the ${attempt}`);
      assert.equal(delivery.turn_state, "interrupted");
      assert.equal(
        delivery.content,
        "That turn was interrupted. Send another message to continue in this conversation. Actions already taken may have had effects.",
      );
      withheld.push(delivery.content);
    }

    const recovered = await simulate("provider-recovered", "continue");
    assert.equal(recovered.delivery.turn_state, "completed");
    assert.equal(recovered.delivery.content, "Recovered after the provider failures.");
    assert.equal(requests, 4, "failed turns must not be retried or replayed");
    const diagnostics = log.join("");
    assert.match(diagnostics, /POST \/v1\/turns failed: .*The usage limit has been reached/u);
    assert.match(diagnostics, /POST \/v1\/turns failed: .*Gateway refused \[redacted\]/u);
    assert.match(diagnostics, /POST \/v1\/turns failed: .*password=\[redacted\]/u);
    for (const text of [explained.delivery.content, ...withheld, diagnostics])
      for (const secret of [key, "short-secret"])
        assert.ok(!text.includes(secret), "credentials must not be displayed or logged");
  } finally {
    await stop("runtime");
    provider.close();
    await rm(privateState, { recursive: true, force: true });
    await rm(sessions, { recursive: true, force: true });
    await rename(`${privateState}-saved`, privateState);
    await rename(`${sessions}-saved`, sessions);
  }
}

/**
 * Native Pi runs a real, deliberately slow `bash` call. While it runs, core's
 * interface-authenticated activity view shows the command; afterwards the
 * turn is gone from the view and the answer is delivered as before.
 */
async function testActivityVisibility() {
  const privateState = join(installation, "pi");
  const sessions = join(installation, "agents/default/sessions");
  const marker = "activity-release";
  const command = `until [ -e ${marker} ]; do sleep 0.1; done; echo released`;
  let requests = 0;
  const provider = createServer((request, response) => {
    request.resume();
    requests += 1;
    const chunk = (delta, finish) =>
      `data: ${JSON.stringify({
        id: `activity-${String(requests)}`,
        object: "chat.completion.chunk",
        created: 0,
        model: "activity-model",
        choices: [{ index: 0, delta, finish_reason: finish }],
      })}\n\n`;
    response.writeHead(200, { "content-type": "text/event-stream" });
    response.end(
      requests === 1
        ? `${chunk(
            {
              role: "assistant",
              content: "Waiting for the release marker.",
              tool_calls: [
                {
                  index: 0,
                  id: "call_slow",
                  type: "function",
                  function: { name: "bash", arguments: JSON.stringify({ command }) },
                },
              ],
            },
            null,
          )}${chunk({}, "tool_calls")}data: [DONE]\n\n`
        : `${chunk({ role: "assistant", content: "The marker appeared." }, null)}${chunk({}, "stop")}data: [DONE]\n\n`,
    );
  });
  provider.listen(0, "127.0.0.1");
  await once(provider, "listening");
  await stop("runtime");
  await rename(privateState, `${privateState}-saved`);
  await rename(sessions, `${sessions}-saved`);
  await mkdir(privateState);
  try {
    await writeFile(
      join(privateState, "settings.json"),
      JSON.stringify({ defaultProvider: "activity-fixture", defaultModel: "activity-model" }),
    );
    await writeFile(
      join(privateState, "models.json"),
      JSON.stringify({
        providers: {
          "activity-fixture": {
            baseUrl: `http://127.0.0.1:${String(provider.address().port)}/v1`,
            api: "openai-completions",
            apiKey: "test-only",
            models: [{ id: "activity-model" }],
          },
        },
      }),
    );
    await start("runtime", { nativePi: true, env: { ...process.env, PI_OFFLINE: "1" } });
    const answered = simulate("activity-slow", "wait for the marker");
    answered.catch(() => undefined);
    let running;
    for (let attempt = 0; running === undefined && attempt < 200; attempt += 1) {
      const turns = await activeTurns(`Bearer ${await credentialValue("discord-private")}`);
      assert.equal(turns.status, 200);
      running = turns.body.find((turn) => turn.activity?.running.length > 0);
      if (running === undefined) await new Promise((resolveWait) => setTimeout(resolveWait, 25));
    }
    assert.ok(running, "the running command never became visible");
    assert.equal(running.state, "running");
    assert.equal(running.target_id, "300");
    assert.deepEqual(
      running.activity.running.map(({ tool, detail }) => ({ tool, detail })),
      [{ tool: "bash", detail: command }],
    );
    assert.equal(running.activity.headline, "Waiting for the release marker.");
    assert.equal(running.activity.phase, "tools");
    for (const authorization of [null, `Bearer ${await credentialValue("operator")}`])
      assert.equal((await activeTurns(authorization)).status, 403);

    await writeFile(join(workspace, marker), "");
    const { receipt, delivery } = await answered;
    assert.equal(receipt.turn_id, running.turn_id);
    assert.equal(delivery.turn_state, "completed");
    assert.match(delivery.content, /The marker appeared\.$/u);
    const after = await activeTurns(`Bearer ${await credentialValue("discord-private")}`);
    assert.deepEqual(after.body, []);
    assert.equal(requests, 2);
  } finally {
    await stop("runtime");
    provider.close();
    await rm(join(workspace, marker), { force: true });
    await rm(privateState, { recursive: true, force: true });
    await rm(sessions, { recursive: true, force: true });
    await rename(`${privateState}-saved`, privateState);
    await rename(`${sessions}-saved`, sessions);
  }
}

async function activeTurns(authorization) {
  const response = await fetch(`http://127.0.0.1:${String(basePort)}/v1/activity`, {
    headers: {
      "x-carillon-provider": "discord-private",
      ...(authorization === null ? {} : { authorization }),
    },
  });
  return { status: response.status, body: await response.json() };
}

async function testMissingResourceRoot(turnId) {
  const resources = join(installation, "agents/default");
  const privateState = join(installation, "pi");
  await rename(resources, `${resources}-saved`);
  await rename(privateState, `${privateState}-saved`);
  await start("core");
  const before = JSON.parse(run("--home", installation, "recent"));
  try {
    for (const missing of [true, false]) {
      if (!missing) await writeFile(resources, "not a directory");
      assertStartupRefused("runtime", missing ? /ENOENT/u : /existing directory/u);
      if (missing) await assert.rejects(stat(resources), { code: "ENOENT" });
      else assert.equal(await readFile(resources, "utf8"), "not a directory");
      await assert.rejects(stat(privateState), { code: "ENOENT" });
      assert.equal(JSON.parse(run("--home", installation, "inspect", turnId)).state, "queued");
      const response = await fetch(`http://127.0.0.1:${String(basePort)}/v1/events`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${await credentialValue("discord-private")}`,
          "x-carillon-provider": "discord-private",
          "content-type": "application/json",
        },
        body: JSON.stringify({
          event_id: "missing-resources",
          agent_id: "default",
          user_id: "200",
          audience_id: "new-conversation",
          delivery_target_id: "300",
          feedback_target_id: "300:missing-resources",
          content: "must not be accepted without the configured root",
        }),
      });
      assert.equal(response.status, 500, await response.text());
      assert.deepEqual(JSON.parse(run("--home", installation, "recent")), before);
    }
  } finally {
    await stop("core");
    await rm(resources, { recursive: true, force: true });
    await rm(privateState, { recursive: true, force: true });
    await rename(`${resources}-saved`, resources);
    await rename(`${privateState}-saved`, privateState);
  }
}

async function testRuntimeStartupRefusals(turnId) {
  const original = await readFile(configPath(), "utf8");
  const sessions = join(installation, "agents/default/sessions");
  const privateState = join(installation, "pi");
  const alternateToken = join(workspace, "alternate-runtime.token");
  await rename(sessions, `${sessions}-saved`);
  await rename(privateState, `${privateState}-saved`);
  await start("core");
  try {
    for (const value of ["wrong-token", "   ", null]) {
      if (value === null) await rm(alternateToken);
      else await writeFile(alternateToken, value);
      assertStartupRefused("runtime", value === null ? /ENOENT/u : /credential does not match/u, [
        "--token-file",
        alternateToken,
      ]);
      await assert.rejects(stat(sessions), { code: "ENOENT" });
      await assert.rejects(stat(privateState), { code: "ENOENT" });
      assert.equal(JSON.parse(run("--home", installation, "inspect", turnId)).state, "queued");
    }
    run("--home", installation, "provider", "runtime-pi", "disable");
    assertStartupRefused("runtime", /runtime provider runtime-pi is disabled/u);
    await assert.rejects(stat(sessions), { code: "ENOENT" });
    await assert.rejects(stat(privateState), { code: "ENOENT" });
    assert.equal(JSON.parse(run("--home", installation, "inspect", turnId)).state, "queued");
  } finally {
    await stop("core");
    await writeFile(configPath(), original);
    await rm(alternateToken, { force: true });
    await rm(sessions, { recursive: true, force: true });
    await rm(privateState, { recursive: true, force: true });
    await rename(`${sessions}-saved`, sessions);
    await rename(`${privateState}-saved`, privateState);
  }
}

async function testProviderVersions() {
  const original = await readFile(configPath(), "utf8");
  const coreVersion = run("--version").replace(/^carillon /u, "");
  const runtimeVersion = JSON.parse(
    await readFile("packages/runtime-pi/package.json", "utf8"),
  ).version;
  const expected = [
    ["core", coreVersion],
    ["discord-private", coreVersion],
    ["runtime-pi", runtimeVersion],
  ];
  try {
    // A preserved expectation from a different release must not become the
    // running provider's identity. This runs against source and packaged services.
    const stale = original.replaceAll(/version = "[^"]+"/gu, 'version = "stale-expectation"');
    assert.notEqual(stale, original);
    await writeFile(configPath(), stale);
    await startAll();
    for (const [offset, [id, version]] of expected.entries()) {
      const response = await fetch(`http://127.0.0.1:${String(basePort + offset)}/health`);
      assert.equal(response.status, 200);
      const health = await response.json();
      assert.equal(health.id, id);
      assert.equal(health.version, version, `${id} must report its build-owned version`);
    }
    const status = JSON.parse(run("--home", installation, "status"));
    for (const [id, version] of expected) {
      const provider = status.providers.find((report) => report.id === id);
      assert.equal(provider.version, version);
      assert.equal(provider.status, id === "core" ? "ready" : "version_mismatch");
    }
  } finally {
    await stopAll();
    await writeFile(configPath(), original);
  }
  // Repair the operator expectation and restart, without changing binaries.
  await startAll();
  assert.ok(
    JSON.parse(run("--home", installation, "status")).providers.every(
      (provider) => provider.status === "ready",
    ),
  );
  await stopAll();
}

async function testConfigurationSymlinkUpdates() {
  const original = await readFile(configPath(), "utf8");
  const originalDiscord = await readFile(providerConfigPath(), "utf8");
  const previousMode = (await stat(configPath())).mode & 0o777;
  // An operator's convenient link to the installation is an ordinary --home.
  const link = join(workspace, "admin-home");
  await symlink(installation, link);
  const linkInode = (await lstat(link)).ino;
  await chmod(configPath(), 0o640);
  const assertUpdated = async () => {
    assert.equal((await lstat(link)).ino, linkInode, "CLI must retain the installation symlink");
    assert.ok((await lstat(link)).isSymbolicLink());
    assert.equal((await stat(configPath())).mode & 0o777, 0o640);
    const canonical = await readFile(configPath(), "utf8");
    assert.equal(await readFile(join(link, "carillon.toml"), "utf8"), canonical);
    assert.match(
      canonical,
      /^data_dir = "state"$/mu,
      "writing must retain installation-relative paths",
    );
  };
  try {
    for (const enabled of [false, true]) {
      run("--home", link, "provider", "runtime-pi", enabled ? "enable" : "disable");
      await assertUpdated();
      await start("core");
      const status = JSON.parse(run("--home", installation, "status"));
      assert.equal(
        status.providers.find((provider) => provider.id === "runtime-pi").status,
        enabled ? "unavailable" : "disabled",
      );
      await stop("core");
    }
    // Provider settings live in the provider's own file; the registry is untouched.
    configureDiscord(link, "101", "202", "203", "304:mention:thread");
    await assertUpdated();
    assert.equal(await readFile(configPath(), "utf8"), original);
    const configured = await readFile(providerConfigPath(), "utf8");
    assert.match(configured, /^guild_id = "101"$/mu);
    assert.equal((await stat(providerConfigPath())).mode & 0o777, 0o600);
    assert.throws(
      () => configureDiscord(link, "invalid", "202", "203", "304:mention:thread"),
      /snowflake/u,
    );
    await assertUpdated();
    assert.equal(await readFile(providerConfigPath(), "utf8"), configured);
    // Readiness through the canonical path must observe what the symlink command wrote.
    await startAll();
    const denied = await requestSimulation(
      event("symlink-old-guild", "100", "200", "300", null, "no"),
    );
    assert.equal(denied.status, 403);
    const ignored = await requestSimulation(
      event("symlink-new-route", "101", "203", "304", null, "no mention"),
    );
    assert.equal(ignored.status, 204);
    assert.deepEqual(JSON.parse(run("--home", installation, "recent")), []);
  } finally {
    await stopAll();
    await rm(link, { force: true });
    await writeFile(configPath(), original);
    await writeFile(providerConfigPath(), originalDiscord);
    await chmod(configPath(), previousMode);
  }
}

async function testConfigurationRefusals() {
  const original = await readFile(configPath(), "utf8");
  const runtimeUrl = `http://127.0.0.1:${String(basePort + 2)}`;
  const interfaceUrl = `http://127.0.0.1:${String(basePort + 1)}`;
  const coreAddress = `127.0.0.1:${String(basePort)}`;
  const collision = /listen address conflicts/u;
  const database = join(installation, "state/carillon.sqlite3");
  const previousDatabase = await readFile(database);
  try {
    // The provider serves nothing without its own settings or its command module.
    await rename(providerConfigPath(), `${providerConfigPath()}-saved`);
    try {
      assertStartupRefused("discord", /read Discord configuration/u);
    } finally {
      await rename(`${providerConfigPath()}-saved`, providerConfigPath());
    }
    // Invalid inline presentation fails before the provider starts any work.
    const discordSettings = await readFile(providerConfigPath(), "utf8");
    try {
      const invalidMarker = discordSettings.replace(/^working = .*$/mu, 'working = "@everyone"');
      assert.notEqual(invalidMarker, discordSettings);
      await writeFile(providerConfigPath(), invalidMarker);
      assertStartupRefused("discord", /activity_markers\.working/u);
      assert.deepEqual(await readFile(database), previousDatabase);
    } finally {
      await writeFile(providerConfigPath(), discordSettings);
    }
    const brokenModule = join(workspace, "broken.wasm");
    await writeFile(brokenModule, "\0asm broken");
    assertStartupRefused("discord", /read release command module/u, [], {
      commands: join(workspace, "missing.wasm"),
    });
    assertStartupRefused("discord", /release command module .* is unusable/u, [], {
      commands: brokenModule,
    });
    // An unusable installed module refuses startup, naming the file to remove,
    // even though the release's module is usable.
    const installed = join(installation, "state/components/discord-private/command-module.wasm");
    await mkdir(join(installed, ".."), { recursive: true });
    await rename(brokenModule, installed);
    assertStartupRefused(
      "discord",
      /installed command module .*command-module\.wasm is unusable; remove it/u,
    );
    await rm(installed);
    for (const id of ["runtime-pi", "discord-private"]) {
      // Matching hashes must not turn a provider credential into operator identity;
      // differing hashes must not leave an impossible-to-authenticate provider.
      for (const sameHash of [false, true]) {
        let text = original.replaceAll(`"${id}"`, '"operator"');
        if (sameHash) {
          const operatorHash = /^operator_token_hash = "([a-f0-9]+)"/mu.exec(original)?.[1];
          assert.ok(operatorHash);
          text = text.replaceAll(/^token_hash = "[a-f0-9]+"/gmu, `token_hash = "${operatorHash}"`);
        }
        await writeFile(configPath(), text);
        for (const name of ["core", "discord", "runtime"])
          assertStartupRefused(name, /provider ID operator is reserved/u);
        assert.deepEqual(await readFile(database), previousDatabase);
      }
    }
    for (const id of ["runtime-pi", "discord-private"]) {
      await writeFile(configPath(), original.replaceAll(`"${id}"`, '"discord-bot"'));
      for (const name of ["core", "discord", "runtime"])
        assertStartupRefused(name, /provider ID discord-bot is reserved/u);
      assert.deepEqual(await readFile(database), previousDatabase);
    }
    for (const field of ["operator_token_hash", "token_hash"]) {
      for (const hash of [
        "",
        " ",
        "a".repeat(63),
        "a".repeat(65),
        "A".repeat(64),
        "g".repeat(64),
        "é".repeat(32),
      ]) {
        const text = original.replace(
          new RegExp(`^${field} = "[^"]+"`, "mu"),
          `${field} = "${hash}"`,
        );
        assert.notEqual(text, original);
        await writeFile(configPath(), text);
        for (const name of ["core", "discord"]) {
          assertStartupRefused(
            name,
            /credential hash must be 64 lowercase hexadecimal characters/u,
          );
        }
        assert.deepEqual(
          await readFile(database),
          previousDatabase,
          "invalid hash must fail before recovery",
        );
      }
    }
    for (const [text, message] of [
      [original.replace(runtimeUrl, interfaceUrl), collision],
      [original.replace(runtimeUrl, `http://${coreAddress}`), collision],
      [original.replace(interfaceUrl, `http://${coreAddress}`), collision],
      [
        original
          .replace(runtimeUrl, interfaceUrl)
          .replace('role = "interface"', 'role = "runtime"'),
        collision,
      ],
      [original.replace(coreAddress, `0.0.0.0:${String(basePort + 2)}`), collision],
      [original.replace(coreAddress, `[::]:${String(basePort + 2)}`), collision],
      [
        original
          .replace(interfaceUrl, `http://[::1]:${String(basePort + 1)}`)
          .replace(runtimeUrl, `http://[0:0:0:0:0:0:0:1]:${String(basePort + 1)}`),
        collision,
      ],
      [
        original.replace('id = "default"', 'id = ""').replace('agent = "default"', 'agent = ""'),
        /agent identity must not be empty/u,
      ],
      [original.replace('id = "default"', 'id = "  "'), /agent identity must not be empty/u],
    ]) {
      assert.notEqual(text, original);
      await writeFile(configPath(), text);
      for (const name of ["core", "discord"]) assertStartupRefused(name, message);
      assert.deepEqual(await readFile(database), previousDatabase);
    }
    // Disabled entries reserve no listener; enable must reject before writing.
    const disabled = original
      .replace(runtimeUrl, interfaceUrl)
      .replace("enabled = true", "enabled = false");
    await writeFile(configPath(), disabled);
    await start("core");
    assert.deepEqual(JSON.parse(run("--home", installation, "recent")), []);
    assert.throws(
      () => run("--home", installation, "provider", "discord-private", "enable"),
      collision,
    );
    assert.equal(await readFile(configPath(), "utf8"), disabled);
  } finally {
    await stop("core");
    await writeFile(configPath(), original);
  }
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
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${await credentialValue("discord-private")}`,
    },
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

function interaction(channelId, parentChannelId, kind, name, values) {
  return {
    guild_id: "100",
    user_id: "200",
    channel_id: channelId,
    parent_channel_id: parentChannelId,
    kind,
    name,
    values,
    options: {},
  };
}

async function requestInteraction(source) {
  return fetch(`http://127.0.0.1:${String(basePort + 1)}/v1/interactions`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${await credentialValue("discord-private")}`,
    },
    body: JSON.stringify(source),
  });
}

/** Runs a slash command or component interaction and returns the reply. */
async function interact(channelId, parentChannelId, kind, name, values = []) {
  const response = await requestInteraction(
    interaction(channelId, parentChannelId, kind, name, values),
  );
  const body = await response.text();
  assert.equal(response.status, 200, body);
  return JSON.parse(body);
}

/** The first control of each action row. */
function customIds(reply) {
  return reply.components.map((row) => row.components[0].custom_id);
}

/** The buttons of the last action row. */
function buttons(reply) {
  return reply.components.at(-1).components.map((component) => component.custom_id);
}

function control(reply, customId) {
  const found = reply.components
    .flatMap((row) => row.components)
    .find((component) => component.custom_id === customId);
  assert.ok(found, `${customId} is present`);
  return found;
}

function options(reply, customId) {
  return control(reply, customId).options;
}

function providerControl(reply, scope) {
  const id = customIds(reply).find((id) => id.startsWith(`model:provider:${scope}:`));
  assert.ok(id, "provider menu is present");
  return id;
}

function selectedLabel(reply, customId) {
  return options(reply, customId).find((option) => option.default)?.label;
}

/** The select value standing for the option labelled or described `text`. */
function value(reply, customId, text) {
  const option = options(reply, customId).find(
    (candidate) => candidate.label === text || candidate.description === text,
  );
  assert.ok(option, `${customId} offers ${text}`);
  return option.value;
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

async function acknowledgeDelivery(id, instance) {
  const response = await fetch(
    `http://127.0.0.1:${String(basePort)}/v1/deliveries/${String(id)}/ack`,
    {
      method: "POST",
      headers: {
        authorization: `Bearer ${await credentialValue("discord-private")}`,
        "x-carillon-provider": "discord-private",
        "x-carillon-instance": instance,
        "content-type": "application/json",
      },
      body: JSON.stringify({ outcome: "delivered", native_ref: "local:resumed" }),
    },
  );
  const body = await response.text();
  assert.equal(response.status, 204, body);
}

async function waitForTurn(turnId, state, deliveryState = null) {
  let inspection;
  for (let attempt = 0; attempt < 200; attempt += 1) {
    inspection = JSON.parse(run("--home", installation, "inspect", turnId));
    if (
      inspection.state === state &&
      (deliveryState === null || inspection.delivery_state === deliveryState)
    ) {
      return inspection;
    }
    await new Promise((resolveWait) => setTimeout(resolveWait, 25));
  }
  throw new Error(
    `turn ${turnId} did not reach ${state}/${String(deliveryState)}; last inspection: ${JSON.stringify(inspection)}`,
  );
}

function serviceArguments(name, { nativePi = false, commands = commandsModule } = {}) {
  if (name === "core") return [binary, ["core", "--config", configPath()]];
  if (name === "runtime") {
    const args = [
      "--config",
      runtimeConfigPath ?? configPath(),
      "--token-file",
      credential("runtime-pi"),
      "--provider",
      "runtime-pi",
      ...(nativePi ? [] : ["--deterministic"]),
      "--state-dir",
      join(installation, "pi"),
    ];
    return packagedRuntime === undefined
      ? [process.execPath, [runtimeEntry, ...args]]
      : [resolve(packagedRuntime), args];
  }
  return [
    binary,
    [
      "discord",
      "serve",
      "--config",
      configPath(),
      "--provider",
      "discord-private",
      "--token-file",
      credential("discord-private"),
      "--commands",
      commands,
      "--deterministic",
    ],
  ];
}

function servicePort(name) {
  return basePort + { core: 0, discord: 1, runtime: 2 }[name];
}

async function start(name, options = {}) {
  const [program, args] = serviceArguments(name, options);
  const child = spawn(program, args, {
    stdio: ["ignore", "ignore", options.log === undefined ? "ignore" : "pipe"],
    env: options.env,
  });
  child.stderr?.setEncoding("utf8").on("data", (text) => options.log.push(text));
  services.set(name, child);
  await waitForHealth(servicePort(name), name === "runtime" ? runtimeHost : "127.0.0.1");
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

async function waitForHealth(port, host) {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    try {
      const response = await fetch(`http://${host}:${String(port)}/health`);
      if (response.ok) return;
    } catch {}
    await new Promise((resolveWait) => setTimeout(resolveWait, 50));
  }
  throw new Error(`service on port ${String(port)} did not become ready`);
}
