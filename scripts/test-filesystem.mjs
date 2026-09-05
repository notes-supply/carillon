import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { chmod, mkdir, mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

// Linux syscall fault injection: real binaries, SQLite, files and HTTP. No
// application fault switches, mocked storage, or service-manager interaction.
const [mode, artifact] = process.argv.slice(2);
assert.ok(mode === "init" || mode === "resources", "expected init or resources");
const binary = resolve(mode === "init" ? join(artifact, "bin/carillon") : artifact);
const release = mode === "init" ? resolve(artifact) : null;
execFileSync("strace", ["--version"], { stdio: "ignore" });
const scratch = await mkdtemp(join(tmpdir(), "carillon-filesystem-"));
const port = 40_000 + (process.pid % 10_000);
const options = [
  "--guild-id",
  "100",
  "--bot-user-id",
  "201",
  "--user-id",
  "200",
  "--route",
  "300:always:channel",
  "--base-port",
  String(port),
];
const traceOptions = ["-f", "-qq", "-yy", "-s", "4096", "-e", "trace=mkdir,rename,fsync,fdatasync"];
const run = (...args) => execFileSync(binary, args, { encoding: "utf8", timeout: 30_000 });

try {
  if (mode === "init") await testInitialization();
  else await testResources();
} finally {
  await rm(scratch, { recursive: true, force: true });
}

async function testInitialization() {
  const baseline = join(scratch, "baseline");
  const trace = join(scratch, "init.trace");
  execFileSync("strace", [
    ...traceOptions,
    "-o",
    trace,
    binary,
    "init",
    "--root",
    baseline,
    "--workspace",
    scratch,
    ...options,
  ]);
  const lines = (await readFile(trace, "utf8")).split("\n");
  const renames = lines.filter((line) => / rename\(/u.test(line));
  const publication = renames.findIndex((line) => line.includes(`, "${baseline}")`)) + 1;
  assert.ok(publication > 2, "trace must include staged writes and final publication");
  const publicationLine = lines.indexOf(renames[publication - 1]);
  const syncs = lines.filter((line) => / fsync\(/u.test(line));
  const configSync = syncs.findIndex((line) => line.includes("/carillon.toml>")) + 1;
  const parentSync =
    lines.slice(0, publicationLine).filter((line) => / fsync\(/u.test(line)).length + 1;
  assert.ok(configSync > 0, "staged configuration must be synced");
  for (const required of [
    "carillon.toml",
    "credentials/operator.token",
    "state/carillon.sqlite3",
    "agents/default",
  ]) {
    assert.ok(
      lines
        .slice(0, publicationLine)
        .some(
          (line) =>
            line.includes(`${required}>`) && line.includes("fsync(") && line.endsWith("= 0"),
        ),
      `initial state must be synced before publication: ${required}`,
    );
  }
  const failures = [
    ["early-directories", "mkdir:error=EIO:signal=SIGKILL:when=4", false],
    ["before-config", "rename:error=EIO:signal=SIGKILL:when=1", false],
    ["after-config", "rename:error=EIO:signal=SIGTERM:when=2", false],
    ["sync", `fsync:error=EIO:when=${configSync}`, false],
    ["publish-error", `rename:error=EIO:when=${publication}`, false],
    ["publish-kill-before", `rename:error=EIO:signal=SIGKILL:when=${publication}`, false],
    ["publish-term-after", `fsync:error=EIO:signal=SIGTERM:when=${parentSync}`, true],
    ["publish-kill-after", `fsync:error=EIO:signal=SIGKILL:when=${parentSync}`, true],
  ];
  for (const empty of [false, true]) {
    for (const [name, injection, published] of failures) {
      const home = join(scratch, `${empty ? "empty" : "absent"}-${name}`);
      const root = join(home, ".carillon");
      await mkdir(home);
      if (empty) await mkdir(root);
      const oldInode = empty ? (await stat(root)).ino : null;
      const log = join(home, "trace");
      const result = spawnSync(
        "strace",
        [
          ...traceOptions,
          "-o",
          log,
          "-e",
          `inject=${injection}`,
          binary,
          "init",
          "--root",
          root,
          "--workspace",
          home,
          ...options,
        ],
        { encoding: "utf8", timeout: 30_000 },
      );
      assert.ifError(result.error);
      assert.notEqual(result.status, 0, `injection must fail: ${name}`);
      const observed = await readFile(log, "utf8");
      const signal = injection.match(/signal=(SIG\w+)/u)?.[1];
      if (signal) {
        assert.ok(
          observed.includes(`killed by ${signal}`),
          "injection must terminate the real initializer",
        );
        const syscall = injection.split(":")[0];
        const ordinal = Number(injection.match(/when=(\d+)/u)[1]);
        assert.equal(
          observed.split("\n").filter((line) => line.includes(` ${syscall}(`)).length,
          ordinal,
          "initializer must stop at the requested boundary",
        );
      } else {
        assert.match(observed, /INJECTED/u, "failure must hit a real syscall");
      }
      let token;
      if (published) {
        run(
          "backup",
          "--config",
          join(root, "carillon.toml"),
          "--output",
          join(home, "validated.tar.gz"),
        );
        token = await readFile(join(root, "credentials/operator.token"));
      } else if (empty) {
        assert.deepEqual(await readdir(root), [], "partial initialization must not be visible");
        assert.equal(
          (await stat(root)).ino,
          oldInode,
          "failure must preserve the original empty target",
        );
      } else {
        await assert.rejects(stat(root), { code: "ENOENT" });
      }
      // Exercise the actual installer's choice of fresh/existing state, not just init again.
      execFileSync(join(release, "install.sh"), ["--workspace", home, ...options], {
        env: {
          ...process.env,
          HOME: home,
          XDG_CONFIG_HOME: join(home, ".config"),
          CARILLON_HOME: root,
          CARILLON_SKIP_SYSTEMD_RELOAD: "1",
        },
        stdio: "pipe",
        timeout: 60_000,
      });
      run(
        "backup",
        "--config",
        join(root, "carillon.toml"),
        "--output",
        join(home, "retry.tar.gz"),
      );
      if (published)
        assert.deepEqual(
          await readFile(join(root, "credentials/operator.token")),
          token,
          "retry must retain committed credentials",
        );
      console.log(`initialization interruption passed: ${empty ? "empty" : "absent"} / ${name}`);
      await rm(home, { recursive: true, force: true });
    }
  }
}

async function testResources() {
  // Discover syscall ordinals from the real publication, not a source-shape assertion.
  const baseline = await resourceAttempt("baseline", null);
  const worker = baseline
    .find((line) => line.includes("fsync(") && line.includes("/resource-snapshots/"))
    ?.split(" ")[0];
  assert.ok(worker, "snapshot contents must be synced before acceptance");
  const syncs = baseline.filter((line) => line.startsWith(`${worker} `) && / fsync\(/u.test(line));
  const startupSyncs = baseline.filter(
    (line) => !line.startsWith(`${worker} `) && / fsync\(/u.test(line),
  ).length;
  const points = [
    ["store", (line) => line.includes("/resource-snapshots>")],
    ["store-parent", (line) => line.includes("/state>")],
  ];
  const rename = baseline.findIndex((line) => / rename\(/u.test(line));
  assert.ok(rename > 0, "snapshot rename must be traced");
  const beforeRename = baseline.slice(0, rename);
  for (const relative of [
    "AGENTS.md",
    "SYSTEM.md",
    "settings.json",
    "skills/example/scripts/helper",
    "skills/example/scripts",
    "skills/example",
    "skills",
  ]) {
    assert.ok(
      beforeRename.some(
        (line) =>
          line.includes("fsync(") &&
          line.includes(`/resource-snapshots/`) &&
          line.includes(`/${relative}>`) &&
          line.endsWith("= 0"),
      ),
      `resource must be synced before rename: ${relative}`,
    );
  }
  assert.ok(
    beforeRename.some((line) => / fsync\(.*resource-snapshots\/\.[^/>]+>\)\s+= 0/u.test(line)),
    "snapshot directory must be synced before rename",
  );
  for (const [name, predicate] of points) {
    const index = syncs.findIndex(predicate);
    assert.ok(index >= 0, `missing durable snapshot boundary: ${name}`);
    assert.ok(index + 1 > startupSyncs, "injection must not hit the startup thread");
    assert.ok(baseline.indexOf(syncs[index]) > rename, `sync must follow publication: ${name}`);
    await resourceAttempt(name, { ordinal: index + 1, predicate });
    console.log(`snapshot durability failure passed: ${name}`);
  }
}

async function resourceAttempt(name, fault) {
  const root = join(scratch, name);
  run("init", "--root", root, "--workspace", scratch, ...options);
  const helper = join(root, "agents/default/skills/example/scripts/helper");
  await mkdir(dirname(helper), { recursive: true });

  await writeFile(helper, "#!/bin/sh\nprintf durable-helper");
  await chmod(helper, 0o750);
  const config = join(root, "carillon.toml");
  const log = join(scratch, `${name}.trace`);
  const token = (await readFile(join(root, "credentials/discord-private.token"), "utf8")).trim();
  const args = [...traceOptions, "-o", log];
  if (fault) args.push("-e", `inject=fsync:error=EIO:when=${fault.ordinal}`);
  args.push(binary, "core", "--config", config);
  const server = spawn("strace", args, {
    env: { ...process.env, TOKIO_WORKER_THREADS: "1" },
    detached: true,
    stdio: "ignore",
  });
  const exited = once(server, "exit");
  const base = `http://127.0.0.1:${port}`;
  try {
    const deadline = Date.now() + 10_000;
    while (true) {
      assert.equal(server.exitCode, null, "core exited before listening");
      try {
        const response = await fetch(`${base}/health`, { signal: AbortSignal.timeout(500) });
        if (response.status === 200) break;
      } catch {
        /* Listener is not ready yet. */
      }
      assert.ok(Date.now() < deadline, "core did not become ready");
      await delay(25);
    }
    const accept = () =>
      fetch(`${base}/v1/events`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${token}`,
          "x-carillon-provider": "discord-private",
          "content-type": "application/json",
        },
        body: JSON.stringify({
          event_id: "durability",
          agent_id: "default",
          user_id: "200",
          audience_id: "test",
          delivery_target_id: "300",
          feedback_target_id: "300:durability",
          content: "keep queued while runtime is absent",
        }),
        signal: AbortSignal.timeout(5_000),
      });
    if (fault) {
      const failed = await accept();
      assert.equal(failed.status, 500, await failed.text());
      const injected = (await readFile(log, "utf8"))
        .split("\n")
        .filter((line) => line.includes("INJECTED"));
      assert.equal(injected.length, 1);
      assert.ok(fault.predicate(injected[0]), `wrong injection boundary: ${injected[0]}`);
      assert.deepEqual(
        JSON.parse(run("recent", "--config", config)),
        [],
        "failed snapshot publication must not accept durable work",
      );
    }
    const response = await accept();
    assert.equal(response.status, 200, await response.clone().text());
    const receipt = await response.json();
    assert.equal(receipt.state, "queued");
    const turns = JSON.parse(run("recent", "--config", config));
    assert.equal(turns.length, 1);
    const inspection = JSON.parse(run("inspect", "--config", config, receipt.turn_id));
    const snapshot = join(root, "state/resource-snapshots", inspection.resources.id);
    assert.equal(
      execFileSync(join(snapshot, "skills/example/scripts/helper"), { encoding: "utf8" }),
      "durable-helper",
    );
    const duplicate = await accept();
    assert.equal(duplicate.status, 200);
    assert.equal((await duplicate.json()).turn_id, receipt.turn_id);
    return (await readFile(log, "utf8")).split("\n");
  } finally {
    try {
      process.kill(-server.pid, "SIGTERM");
    } catch (error) {
      if (error.code !== "ESRCH") throw error;
    }
    const forceStop = setTimeout(() => {
      try {
        process.kill(-server.pid, "SIGKILL");
      } catch (error) {
        if (error.code !== "ESRCH") throw error;
      }
    }, 5_000);
    try {
      await exited;
    } finally {
      clearTimeout(forceStop);
    }
  }
}
