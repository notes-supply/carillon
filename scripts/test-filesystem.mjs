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
assert.ok(
  ["init", "resources", "restore", "participation", "config"].includes(mode),
  "unknown filesystem test mode",
);
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
  if (mode === "init") {
    await testInitialization();
    await testRestore();
  } else if (mode === "restore") await testRestore();
  else if (mode === "participation") await testParticipation();
  else if (mode === "config") await testConfiguration();
  else {
    await testConfiguration();
    await testResources();
    await testParticipation();
  }
} finally {
  await rm(scratch, { recursive: true, force: true });
}

async function testConfiguration() {
  const root = join(scratch, "configuration");
  run("init", "--root", root, "--workspace", scratch, ...options);
  const config = join(root, "carillon.toml");
  await chmod(config, 0o640);
  const original = await readFile(config);
  const token = await readFile(join(root, "credentials/operator.token"));
  for (const args of [
    ["provider", "--config", config, "runtime-pi", "disable"],
    [
      "configure-discord",
      "--config",
      config,
      "--guild-id",
      "100",
      "--bot-user-id",
      "201",
      "--user-id",
      "999",
      "--route",
      "301:mention:thread",
    ],
  ]) {
    await writeFile(config, original);
    const trace = join(scratch, `${args[0]}.trace`);
    execFileSync("strace", [...traceOptions, "-o", trace, binary, ...args]);
    const updated = await readFile(config);
    assert.notDeepEqual(updated, original);
    const lines = (await readFile(trace, "utf8")).split("\n");
    const publication = lines.findIndex((line) => line.includes(`, "${config}") = 0`));
    const temporary = /rename\("([^"]+)",/u.exec(lines[publication])?.[1];
    assert.ok(temporary);
    const fileSync = lines.findIndex(
      (line) => line.includes(`fsync(`) && line.includes(`<${temporary}>`) && line.endsWith("= 0"),
    );
    const parentSync = lines.findIndex(
      (line) => line.includes(`fsync(`) && line.includes(`<${root}>`) && line.endsWith("= 0"),
    );
    assert.ok(
      fileSync >= 0 && fileSync < publication && publication < parentSync,
      "configuration must sync file, rename, then sync parent",
    );
    for (const [boundary, published] of [
      [fileSync, false],
      [parentSync, true],
    ]) {
      const pid = lines[boundary].trim().split(/\s+/u)[0];
      const ordinal = lines
        .slice(0, boundary + 1)
        .filter((line) => line.trim().startsWith(`${pid} `) && line.includes(" fsync(")).length;
      assert.ok(ordinal > 0);
      await writeFile(config, original);
      const injected = join(scratch, `${args[0]}-${ordinal}.trace`);
      const result = spawnSync(
        "strace",
        [
          ...traceOptions,
          "-o",
          injected,
          "-e",
          `inject=fsync:error=EIO:when=${ordinal}`,
          binary,
          ...args,
        ],
        { encoding: "utf8", timeout: 30_000 },
      );
      assert.ifError(result.error);
      assert.notEqual(result.status, 0);
      const failures = (await readFile(injected, "utf8"))
        .split("\n")
        .filter((line) => line.includes("INJECTED"));
      assert.equal(failures.length, 1);
      const syncedPath = /fsync\(\d+<([^>]+)>/u.exec(failures[0])?.[1];
      assert.ok(syncedPath);
      assert.equal(published ? syncedPath : dirname(syncedPath), root);
      assert.deepEqual(await readFile(config), published ? updated : original);
      run(...args);
      assert.deepEqual(
        await readFile(config),
        updated,
        "retry must preserve the intended change after either failure boundary",
      );
      assert.equal((await stat(config)).mode & 0o777, 0o640);
      assert.deepEqual(await readFile(join(root, "credentials/operator.token")), token);
      console.log(`configuration durability passed: ${args[0]} / ${published ? "parent" : "file"}`);
    }
  }
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

async function testRestore() {
  const source = join(scratch, "restore-source");
  run("init", "--root", source, "--workspace", scratch, ...options);
  const token = await readFile(join(source, "credentials/operator.token"));
  const archive = join(scratch, "restore-source.tar.gz");
  run("backup", "--config", join(source, "carillon.toml"), "--output", archive);
  await rm(source, { recursive: true });
  const baseline = join(scratch, "restore-baseline");
  const trace = join(scratch, "restore.trace");
  const restoreArgs = (target) => ["restore", "--input", archive, "--target", target];
  execFileSync("strace", [...traceOptions, "-o", trace, binary, ...restoreArgs(baseline)]);
  const lines = (await readFile(trace, "utf8")).split("\n");
  const publication = lines.findIndex((line) => line.includes(`, "${baseline}")`));
  assert.ok(publication >= 0, "restore must publish with rename");
  const staging = lines[publication].match(/rename\("([^"]+)"/u)[1];
  // Check every restored file and directory, not only a convenient fixture file.
  for (const name of ["", ...(await readdir(baseline, { recursive: true }))]) {
    assert.ok(
      lines
        .slice(0, publication)
        .some(
          (line) =>
            line.includes(`fsync(`) &&
            line.includes(`<${join(staging, name)}>`) &&
            line.endsWith("= 0"),
        ),
      `restored tree missing pre-publication sync: ${name}`,
    );
  }
  assert.ok(
    lines
      .slice(publication + 1)
      .some(
        (line) => line.includes(`fsync(`) && line.includes(`<${scratch}>`) && line.endsWith("= 0"),
      ),
    "restore parent must be synced",
  );
  const configSync =
    lines
      .filter((line) => line.includes(" fsync("))
      .findIndex((line) => line.includes("/carillon.toml>")) + 1;
  const parentSync =
    lines.slice(0, publication).filter((line) => line.includes(" fsync(")).length + 1;
  const rename = lines.slice(0, publication + 1).filter((line) => line.includes(" rename(")).length;
  const cases = [
    ["tree-sync", `fsync:error=EIO:when=${configSync}`, false],
    ["rename", `rename:error=EIO:when=${rename}`, false],
    ["kill-before", `rename:error=EIO:signal=SIGKILL:when=${rename}`, false],
    ["parent-sync", `fsync:error=EIO:when=${parentSync}`, true],
    ["term-after", `fsync:error=EIO:signal=SIGTERM:when=${parentSync}`, true],
    ["kill-after", `fsync:error=EIO:signal=SIGKILL:when=${parentSync}`, true],
  ];
  for (const empty of [false, true]) {
    for (const [name, injection, published] of cases) {
      const target = join(scratch, `restore-${empty}-${name}`);
      if (empty) await mkdir(target);
      const inode = empty ? (await stat(target)).ino : null;
      const log = `${target}.trace`;
      const result = spawnSync(
        "strace",
        [...traceOptions, "-o", log, "-e", `inject=${injection}`, binary, ...restoreArgs(target)],
        { encoding: "utf8", timeout: 30_000 },
      );
      assert.ifError(result.error);
      assert.notEqual(result.status, 0, "injection must fail the restore");
      const observed = await readFile(log, "utf8");
      const signal = injection.match(/signal=(SIG\w+)/u)?.[1];
      if (signal) {
        assert.ok(observed.includes(`killed by ${signal}`));
        assert.equal(
          observed.split("\n").filter((line) => line.includes(` ${injection.split(":")[0]}(`))
            .length,
          Number(injection.match(/when=(\d+)/u)[1]),
        );
      } else assert.match(observed, /INJECTED/u);
      if (!published) {
        if (empty) {
          assert.deepEqual(await readdir(target), []);
          assert.equal((await stat(target)).ino, inode);
        } else await assert.rejects(stat(target), { code: "ENOENT" });
        run(...restoreArgs(target));
      }
      // Post-rename failure is a committed publication, not a rollback: validate
      // the selected tree rather than overwriting a nonempty restore destination.
      assert.deepEqual(await readFile(join(target, "credentials/operator.token")), token);
      run("backup", "--config", join(target, "carillon.toml"), "--output", `${target}.tar.gz`);
      console.log(`restore durability failure passed: ${empty ? "empty" : "absent"} / ${name}`);
    }
  }
}

async function testParticipation() {
  const messages = execFileSync(
    "cargo",
    ["test", "--locked", "-p", "carillon-core", "--lib", "--no-run", "--message-format=json"],
    { encoding: "utf8", timeout: 120_000 },
  );
  const artifacts = messages
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line))
    .filter(
      (message) =>
        message.reason === "compiler-artifact" && message.executable && message.profile.test,
    );
  assert.equal(artifacts.length, 1, "one library test binary must be built");
  const args = [
    artifacts[0].executable,
    "--exact",
    "discord::tests::participation_persistence_retries_without_bypassing_activation",
    "--nocapture",
    "--test-threads=1",
  ];
  const trace = join(scratch, "participation.trace");
  const output = execFileSync("strace", [...traceOptions, "-o", trace, ...args], {
    encoding: "utf8",
    timeout: 30_000,
  });
  assert.match(output, /1 passed; 0 failed/u, "must execute the participation regression");
  const lines = (await readFile(trace, "utf8")).split("\n");
  const renames = lines.filter(
    (line) => line.includes(" rename(") && line.includes("/participation.json.tmp"),
  );
  assert.equal(renames.length, 2, "adoption and native creation must each persist");
  let previousPublication = -1;
  for (const [index, rename] of renames.entries()) {
    const publication = lines.findIndex(
      (line, position) => position > previousPublication && line === rename,
    );
    previousPublication = publication;
    const [temporary, target] = rename.match(/rename\("([^"]+)", "([^"]+)"/u).slice(1);
    const before = lines.findLastIndex(
      (line, position) =>
        position < publication &&
        line.includes(`fsync(`) &&
        line.includes(`<${temporary}>`) &&
        line.endsWith("= 0"),
    );
    const after = lines.findIndex(
      (line, position) =>
        position > publication &&
        line.includes(`fsync(`) &&
        line.includes(`<${dirname(target)}>`) &&
        line.endsWith("= 0"),
    );
    assert.ok(
      before >= 0 && after > publication,
      "participation requires file sync before rename and parent sync after",
    );
    for (const [boundary, position] of [
      ["file", before],
      ["parent", after],
    ]) {
      const tid = lines[position].trim().split(/\s+/u)[0];
      const ordinal = lines
        .slice(0, position + 1)
        .filter((line) => line.trim().startsWith(`${tid} `) && line.includes(" fsync(")).length;
      const log = join(scratch, `participation-${index}-${boundary}.trace`);
      const result = spawnSync(
        "strace",
        [...traceOptions, "-o", log, "-e", `inject=fsync:error=EIO:when=${ordinal}`, ...args],
        { encoding: "utf8", timeout: 30_000 },
      );
      assert.ifError(result.error);
      assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
      assert.match(result.stdout, /1 passed; 0 failed/u);
      assert.ok(
        result.stderr.includes(`${index === 0 ? "adoption" : "creation"} persistence failed:`),
        "failed publication must surface before retry",
      );
      const injected = (await readFile(log, "utf8"))
        .split("\n")
        .filter((line) => line.includes("INJECTED"));
      assert.equal(injected.length, 1, "only the intended test-thread syscall may fail");
      assert.ok(
        boundary === "file"
          ? injected[0].includes("/participation.json.tmp>")
          : !injected[0].includes(".json"),
        `wrong participation boundary: ${injected[0]}`,
      );
      console.log(
        `participation durability failure passed: ${index === 0 ? "adoption" : "creation"} / ${boundary}`,
      );
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
    const snapshot = join(root, "state/resource-snapshots", inspection.resource_snapshot_id);
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
