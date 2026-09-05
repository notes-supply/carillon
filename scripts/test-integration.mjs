import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rename,
  rm,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

// This fixture uses the bundled providers and simulated model responses.
// Counters/activation markers below are test output, not a required reply format.
const root = await mkdtemp(join(tmpdir(), "carillon-integration-"));
const restoredRoot = await mkdtemp(join(tmpdir(), "carillon-restored-"));
const workspace = await mkdtemp(join(tmpdir(), "carillon-workspace-"));
const backup = join(tmpdir(), `carillon-backup-${crypto.randomUUID()}.tar.gz`);
const binary = resolve(process.env.CARILLON_CORE_BINARY ?? "target/debug/carillon");
const runtimeEntry = resolve("packages/runtime-pi/src/index.ts");
const packagedRuntime = process.env.CARILLON_RUNTIME_BINARY;
const basePort = 45_000 + (process.pid % 10_000);
let installation = root;
let runtimeHost = "127.0.0.1";
let runtimeConfigPath;
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
  assert.deepEqual(JSON.parse(run("recent", "--config", configPath())), []);
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
    const turns = JSON.parse(run("recent", "--config", configPath(), "--event-id", eventId));
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

  await stop("runtime");
  const queued = await submitCoreEvent(
    "event-queued-restore",
    "survive runtime outage and restore",
  );
  const queuedInspection = await waitForTurn(queued.turn_id, "queued");
  assert.equal(queuedInspection.state, "queued");

  await stop("core");
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
  run("backup", "--config", configPath(), "--output", backup);
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
      const inspection = JSON.parse(run("inspect", "--config", configPath(), queued.turn_id));
      assert.equal(inspection.state, "queued");
    }
  } finally {
    await rm(workspace, { force: true });
    await rename(`${workspace}-saved`, workspace);
    await rm(sessions, { recursive: true, force: true });
    await rename(`${sessions}-saved`, sessions);
    await stop("core");
  }
  await testPiModelReadiness(queued.turn_id);
  await testMissingResourceRoot(queued.turn_id);
  await testRuntimeStartupRefusals(queued.turn_id);
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

async function testPiModelReadiness(turnId) {
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
  await start("core");
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
      assert.equal(JSON.parse(run("inspect", "--config", configPath(), turnId)).state, "queued");
      const status = JSON.parse(run("status", "--config", configPath()));
      assert.equal(
        status.providers.find((entry) => entry.id === "runtime-pi").status,
        "unavailable",
      );
    }
    // Repair readiness without a model API call. Stop core first so this fixture
    // never dispatches work to the intentionally non-listening model endpoint.
    await stop("core");
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
    await start("core");
    assert.equal(JSON.parse(run("inspect", "--config", configPath(), turnId)).state, "queued");
  } finally {
    await stop("runtime");
    await stop("core");
    await rm(privateState, { recursive: true, force: true });
    await rm(sessions, { recursive: true, force: true });
    await rename(`${privateState}-saved`, privateState);
    await rename(`${sessions}-saved`, sessions);
  }
}

async function testMissingResourceRoot(turnId) {
  const resources = join(installation, "agents/default");
  const privateState = join(installation, "pi");
  await rename(resources, `${resources}-saved`);
  await rename(privateState, `${privateState}-saved`);
  await start("core");
  const before = JSON.parse(run("recent", "--config", configPath()));
  try {
    for (const missing of [true, false]) {
      if (!missing) await writeFile(resources, "not a directory");
      assertStartupRefused("runtime", missing ? /ENOENT/u : /existing directory/u);
      if (missing) await assert.rejects(stat(resources), { code: "ENOENT" });
      else assert.equal(await readFile(resources, "utf8"), "not a directory");
      await assert.rejects(stat(privateState), { code: "ENOENT" });
      assert.equal(JSON.parse(run("inspect", "--config", configPath(), turnId)).state, "queued");
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
      assert.deepEqual(JSON.parse(run("recent", "--config", configPath())), before);
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
      assert.equal(JSON.parse(run("inspect", "--config", configPath(), turnId)).state, "queued");
    }
    run("provider", "--config", configPath(), "runtime-pi", "disable");
    assertStartupRefused("runtime", /runtime provider runtime-pi is disabled/u);
    await assert.rejects(stat(sessions), { code: "ENOENT" });
    await assert.rejects(stat(privateState), { code: "ENOENT" });
    assert.equal(JSON.parse(run("inspect", "--config", configPath(), turnId)).state, "queued");
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
    const status = JSON.parse(run("status", "--config", configPath()));
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
    JSON.parse(run("status", "--config", configPath())).providers.every(
      (provider) => provider.status === "ready",
    ),
  );
  await stopAll();
}

async function testConfigurationSymlinkUpdates() {
  const original = await readFile(configPath(), "utf8");
  const previousMode = (await stat(configPath())).mode & 0o777;
  const link = join(workspace, "admin-config.toml");
  await symlink(configPath(), link);
  const linkInode = (await lstat(link)).ino;
  await chmod(configPath(), 0o640);
  const assertUpdated = async () => {
    assert.equal((await lstat(link)).ino, linkInode, "CLI must retain the configuration symlink");
    assert.ok((await lstat(link)).isSymbolicLink());
    assert.equal((await stat(configPath())).mode & 0o777, 0o640);
    const canonical = await readFile(configPath(), "utf8");
    assert.equal(await readFile(link, "utf8"), canonical);
    assert.match(
      canonical,
      /^data_dir = "state"$/mu,
      "writing must retain installation-relative paths",
    );
  };
  try {
    for (const enabled of [false, true]) {
      run("provider", "--config", link, "runtime-pi", enabled ? "enable" : "disable");
      await assertUpdated();
      await start("core");
      const status = JSON.parse(run("status", "--config", configPath()));
      assert.equal(
        status.providers.find((provider) => provider.id === "runtime-pi").status,
        enabled ? "unavailable" : "disabled",
      );
      await stop("core");
    }
    run(
      "configure-discord",
      "--config",
      link,
      "--guild-id",
      "101",
      "--bot-user-id",
      "202",
      "--user-id",
      "203",
      "--route",
      "304:mention:thread",
    );
    await assertUpdated();
    const beforeRefusal = await readFile(configPath(), "utf8");
    assert.throws(
      () =>
        run(
          "configure-discord",
          "--config",
          link,
          "--guild-id",
          "invalid",
          "--bot-user-id",
          "202",
          "--user-id",
          "203",
          "--route",
          "304:mention:thread",
        ),
      /snowflake/u,
    );
    await assertUpdated();
    assert.equal(await readFile(configPath(), "utf8"), beforeRefusal);
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
    assert.deepEqual(JSON.parse(run("recent", "--config", configPath())), []);
  } finally {
    await stopAll();
    await rm(link, { force: true });
    await writeFile(configPath(), original);
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
    assert.deepEqual(JSON.parse(run("recent", "--config", configPath())), []);
    assert.throws(
      () => run("provider", "--config", configPath(), "discord-private", "enable"),
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
    inspection = JSON.parse(run("inspect", "--config", configPath(), turnId));
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

function serviceArguments(name, { nativePi = false } = {}) {
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

async function start(name, options = {}) {
  const [program, args] = serviceArguments(name, options);
  const child = spawn(program, args, { stdio: "ignore", env: options.env });
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
