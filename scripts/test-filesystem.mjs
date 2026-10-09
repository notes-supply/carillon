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
  ["init", "resources", "restore", "participation", "config", "module", "attachments"].includes(
    mode,
  ),
  "unknown filesystem test mode",
);
const binary = resolve(mode === "init" ? join(artifact, "bin/carillon") : artifact);
const release = mode === "init" ? resolve(artifact) : null;
execFileSync("strace", ["--version"], { stdio: "ignore" });
const scratch = await mkdtemp(join(tmpdir(), "carillon-filesystem-"));
const port = 40_000 + (process.pid % 10_000);
// `init` takes only installation-wide settings; the installer additionally
// writes the Discord provider's own admission file.
const initOptions = ["--base-port", String(port)];
const installOptions = [
  "--guild-id",
  "100",
  "--bot-user-id",
  "201",
  "--user-id",
  "200",
  "--route",
  "300:always:channel",
  ...initOptions,
];
const discordOptions = [
  "--provider",
  "discord-private",
  "--guild-id",
  "100",
  "--bot-user-id",
  "201",
];
const traceOptions = ["-f", "-qq", "-yy", "-s", "4096", "-e", "trace=mkdir,rename,fsync,fdatasync"];
const run = (...args) => execFileSync(binary, args, { encoding: "utf8", timeout: 30_000 });
/** Files staged per publication attempt: enough that the publication
 * thread's injected call comes later than any other thread's same call, since
 * strace applies `when=N` to each thread's own count. */
const STAGED_FILES = 32;

try {
  if (mode === "init") {
    await testInitialization();
    await testRestore();
  } else if (mode === "restore") await testRestore();
  else if (mode === "participation") await testParticipation();
  else if (mode === "config") await testConfiguration();
  else if (mode === "attachments") await testAttachments();
  else if (mode === "module") {
    await testModule();
    await testModuleDirectory();
  } else {
    await testConfiguration();
    await testResources();
    await testAttachments();
    await testParticipation();
    await testModule();
    await testModuleDirectory();
  }
} finally {
  await rm(scratch, { recursive: true, force: true });
}

async function testConfiguration() {
  const root = join(scratch, "configuration");
  run("--home", root, "init", "--workspace", scratch, ...initOptions);
  const config = join(root, "carillon.toml");
  const discord = join(root, "providers/discord-private.toml");
  run(
    "--home",
    root,
    "discord",
    "configure",
    ...discordOptions,
    "--user-id",
    "200",
    "--route",
    "300:always:channel",
  );
  await chmod(config, 0o640);
  const token = await readFile(join(root, "credentials/operator.token"));
  // The registry keeps its operator-chosen mode; provider files are always owner-only.
  for (const [args, target, mode] of [
    [["provider", "runtime-pi", "disable"], config, 0o640],
    [
      [
        "discord",
        "configure",
        ...discordOptions,
        "--user-id",
        "999",
        "--route",
        "301:mention:thread",
      ],
      discord,
      0o600,
    ],
  ]) {
    const original = await readFile(target);
    const trace = join(scratch, `${args[0]}.trace`);
    execFileSync("strace", [...traceOptions, "-o", trace, binary, "--home", root, ...args]);
    const updated = await readFile(target);
    assert.notDeepEqual(updated, original);
    const lines = (await readFile(trace, "utf8")).split("\n");
    const publication = lines.findIndex((line) => line.includes(`, "${target}") = 0`));
    const temporary = /rename\("([^"]+)",/u.exec(lines[publication])?.[1];
    assert.ok(temporary);
    const fileSync = lines.findIndex(
      (line) => line.includes(`fsync(`) && line.includes(`<${temporary}>`) && line.endsWith("= 0"),
    );
    const parentSync = lines.findIndex(
      (line) =>
        line.includes(`fsync(`) && line.includes(`<${dirname(target)}>`) && line.endsWith("= 0"),
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
      await writeFile(target, original);
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
          "--home",
          root,
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
      assert.equal(published ? syncedPath : dirname(syncedPath), dirname(target));
      assert.deepEqual(await readFile(target), published ? updated : original);
      run("--home", root, ...args);
      assert.deepEqual(
        await readFile(target),
        updated,
        "retry must preserve the intended change after either failure boundary",
      );
      assert.equal((await stat(target)).mode & 0o777, mode);
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
    "--home",
    baseline,
    "init",
    "--workspace",
    scratch,
    ...initOptions,
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
          "--home",
          root,
          "init",
          "--workspace",
          home,
          ...initOptions,
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
        run("--home", root, "backup", "--output", join(home, "validated.tar.gz"));
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
      execFileSync(join(release, "install.sh"), ["--workspace", home, ...installOptions], {
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
      run("--home", root, "backup", "--output", join(home, "retry.tar.gz"));
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
  run("--home", source, "init", "--workspace", scratch, ...initOptions);
  const token = await readFile(join(source, "credentials/operator.token"));
  const archive = join(scratch, "restore-source.tar.gz");
  run("--home", source, "backup", "--output", archive);
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
      run("--home", target, "backup", "--output", `${target}.tar.gz`);
      console.log(`restore durability failure passed: ${empty ? "empty" : "absent"} / ${name}`);
    }
  }
}

/** The provider's library test binary, built for syscall tracing. */
function providerTestBinary() {
  const messages = execFileSync(
    "cargo",
    ["test", "--locked", "-p", "carillon-discord", "--lib", "--no-run", "--message-format=json"],
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
  assert.equal(artifacts.length, 1, "one provider test binary must be built");
  return artifacts[0].executable;
}

async function testParticipation() {
  const args = [
    providerTestBinary(),
    "--exact",
    "conversations::tests::participation_persistence_retries_without_bypassing_activation",
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

/**
 * Installing a Discord module clears the registration record durably before
 * contacting Discord, then saves the module with a synced file and directory.
 * A failure at each boundary must be reported as what it is, and repeating the
 * install must finish the change.
 */
async function testModule() {
  const args = [
    providerTestBinary(),
    "--exact",
    "module::tests::saving_reports_its_boundary_and_a_repeat_confirms_it",
    "--nocapture",
    "--test-threads=1",
  ];
  const options = [
    ...traceOptions.slice(0, -1),
    "trace=mkdir,rename,fsync,fdatasync,unlink,unlinkat",
  ];
  const trace = join(scratch, "module.trace");
  const baseline = spawnSync("strace", [...options, "-o", trace, ...args], {
    encoding: "utf8",
    timeout: 60_000,
  });
  assert.equal(baseline.status, 0, `${baseline.stdout}\n${baseline.stderr}`);
  assert.match(baseline.stderr, /first change saved: durable; Discord requests 1/u);
  const lines = (await readFile(trace, "utf8")).split("\n");
  const invalidation = lines.findIndex((line) =>
    /unlink(at)?\(.*\/state\/commands\.json"/u.test(line),
  );
  assert.ok(invalidation >= 0, "the record must be removed before registering");
  const state = /"([^"]+)\/commands\.json"/u.exec(lines[invalidation])[1];
  const recordSync = lines.findIndex(
    (line, position) =>
      position > invalidation && line.includes(" fsync(") && line.includes(`<${state}>`),
  );
  const publication = lines.findIndex(
    (line) => line.includes(" rename(") && line.includes(`, "${state}/module.wasm")`),
  );
  const firstRegistration = lines.findIndex(
    (line) => line.includes(" rename(") && line.includes(`, "${state}/commands.json")`),
  );
  assert.ok(
    recordSync > invalidation && firstRegistration > recordSync && publication > firstRegistration,
    "record clearing must be synced before registering, and the module saved after",
  );
  const temporary = /rename\("([^"]+)",/u.exec(lines[publication])[1];
  const fileSync = lines.findLastIndex(
    (line, position) =>
      position < publication && line.includes(" fsync(") && line.includes(`<${temporary}>`),
  );
  const directorySync = lines.findIndex(
    (line, position) =>
      position > publication && line.includes(" fsync(") && line.includes(`<${state}>`),
  );
  assert.ok(
    fileSync >= 0 && directorySync > publication,
    "module save must sync file and directory",
  );
  for (const [boundary, position, expected] of [
    ["record", recordSync, /first change refused: record .*; Discord requests 0/u],
    ["file", fileSync, /first change saved: failed/u],
    ["directory", directorySync, /first change saved: unsynced/u],
  ]) {
    const tid = lines[position].trim().split(/\s+/u)[0];
    const ordinal = lines
      .slice(0, position + 1)
      .filter((line) => line.trim().startsWith(`${tid} `) && line.includes(" fsync(")).length;
    const log = join(scratch, `module-${boundary}.trace`);
    const result = spawnSync(
      "strace",
      [...options, "-o", log, "-e", `inject=fsync:error=EIO:when=${ordinal}`, ...args],
      { encoding: "utf8", timeout: 60_000 },
    );
    assert.ifError(result.error);
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    assert.match(result.stdout, /1 passed; 0 failed/u);
    assert.match(result.stderr, expected, `wrong report for the ${boundary} boundary`);
    assert.match(result.stderr, /repeat saved: durable/u);
    const injected = (await readFile(log, "utf8"))
      .split("\n")
      .filter((line) => line.includes("INJECTED"));
    assert.equal(injected.length, 1, "only the intended test-thread syscall may fail");
    // Each run has its own temporary installation, so compare within it.
    assert.match(
      injected[0],
      boundary === "file" ? /\/state\/\.tmp[^/>]*>\)/u : /\/state>\)/u,
      `wrong Discord module boundary: ${injected[0]}`,
    );
    console.log(`Discord module durability failure passed: ${boundary}`);
  }
}

/**
 * The first install creates the provider's state directory. A failed sync of
 * that directory or its parent must be reported as a save that did not
 * happen, and a repeat must sync both again before publishing: an existing
 * directory does not show that its entry is durable.
 */
async function testModuleDirectory() {
  const args = [
    providerTestBinary(),
    "--exact",
    "module::tests::a_new_state_directory_is_synced_again_by_a_repeat",
    "--nocapture",
    "--test-threads=1",
  ];
  const trace = join(scratch, "module-directory.trace");
  const baseline = spawnSync("strace", [...traceOptions, "-o", trace, ...args], {
    encoding: "utf8",
    timeout: 60_000,
  });
  assert.equal(baseline.status, 0, `${baseline.stdout}\n${baseline.stderr}`);
  assert.match(baseline.stderr, /first save: durable/u);
  // Each run has its own temporary installation, so paths are read per trace.
  const created = (lines) => {
    const index = lines.findIndex((line) => / mkdir\("[^"]+\/state", /u.test(line));
    assert.ok(index >= 0, "the first save must create the state directory");
    const state = /mkdir\("([^"]+)"/u.exec(lines[index])[1];
    return { index, state, parent: dirname(state) };
  };
  const synced = (line, path) => line.includes(" fsync(") && line.includes(`<${path}>)`);
  const lines = (await readFile(trace, "utf8")).split("\n");
  const baselineDirectory = created(lines);
  for (const boundary of ["state", "parent"]) {
    const position = lines.findIndex(
      (line, index) => index > baselineDirectory.index && synced(line, baselineDirectory[boundary]),
    );
    assert.ok(position > baselineDirectory.index, `the new directory's ${boundary} must be synced`);
    const tid = lines[position].trim().split(/\s+/u)[0];
    const ordinal = lines
      .slice(0, position + 1)
      .filter((line) => line.trim().startsWith(`${tid} `) && line.includes(" fsync(")).length;
    const log = join(scratch, `module-directory-${boundary}.trace`);
    const result = spawnSync(
      "strace",
      [...traceOptions, "-o", log, "-e", `inject=fsync:error=EIO:when=${ordinal}`, ...args],
      { encoding: "utf8", timeout: 60_000 },
    );
    assert.ifError(result.error);
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    assert.match(result.stdout, /1 passed; 0 failed/u);
    assert.match(result.stderr, /first save: failed/u, `wrong report for the ${boundary} sync`);
    assert.match(result.stderr, /repeat saved: durable/u);
    const observed = (await readFile(log, "utf8")).split("\n");
    const failures = observed.filter((line) => line.includes("INJECTED"));
    assert.equal(failures.length, 1, "only the intended test-thread syscall may fail");
    const injected = observed.indexOf(failures[0]);
    const directory = created(observed);
    assert.ok(
      synced(observed[injected], directory[boundary]),
      `wrong directory boundary: ${observed[injected]}`,
    );
    const publication = observed.findIndex(
      (line, index) =>
        index > injected && line.includes(" rename(") && line.includes('/module.wasm")'),
    );
    assert.ok(publication > injected, "the repeat must publish the module");
    for (const path of ["state", "parent"]) {
      assert.ok(
        observed.some(
          (line, index) =>
            index > injected &&
            index < publication &&
            synced(line, directory[path]) &&
            line.endsWith("= 0"),
        ),
        `the repeat must sync the ${path} directory again before publishing`,
      );
    }
    console.log(`Discord module directory failure passed: ${boundary}`);
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
  run("--home", root, "init", "--workspace", scratch, ...initOptions);
  const helper = join(root, "agents/default/skills/example/scripts/helper");
  await mkdir(dirname(helper), { recursive: true });

  await writeFile(helper, "#!/bin/sh\nprintf durable-helper");
  await chmod(helper, 0o750);
  // SQLite's startup sync count varies with its schema. Keep the snapshot
  // worker's injected ordinal beyond it, as attachment capture does below.
  const references = join(dirname(dirname(helper)), "references");
  await mkdir(references);
  for (let index = 0; index < STAGED_FILES; index++) {
    await writeFile(join(references, `${index}.md`), "durable reference");
  }
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
        JSON.parse(run("--home", root, "recent")),
        [],
        "failed snapshot publication must not accept durable work",
      );
    }
    const response = await accept();
    assert.equal(response.status, 200, await response.clone().text());
    const receipt = await response.json();
    assert.equal(receipt.state, "queued");
    const turns = JSON.parse(run("--home", root, "recent"));
    assert.equal(turns.length, 1);
    const inspection = JSON.parse(run("--home", root, "inspect", receipt.turn_id));
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

/** Fail each durability step of an attachment capture in the real core. */
async function testAttachments() {
  const baseline = await attachmentAttempt("attachments-baseline", null);
  const last = STAGED_FILES - 1;
  const copying = baseline.find((line) => /\/attachments\/\.partial-[^/>]+\/0>/u.test(line));
  assert.ok(copying, "captured files must be synced before publication");
  const worker = copying.split(" ")[0];
  // Tokio wakes its I/O driver by writing to an eventfd, from whichever
  // thread finished blocking work, so whether the capture's thread writes
  // one before its files depends on the blocking pool's thread reuse. Writes
  // to those descriptors are not counted here, nor by strace (`trace-fds`).
  const wakers = [
    ...new Set(baseline.flatMap((line) => / write\((\d+)<\{eventfd/u.exec(line)?.slice(1) ?? [])),
  ];
  const counted = (line) => !wakers.some((fd) => line.includes(` write(${fd}<`));
  const calls = (name, thread = worker) =>
    baseline.filter(
      (line) =>
        line.startsWith(`${thread} `) && new RegExp(` ${name}\\(`, "u").test(line) && counted(line),
    );
  const others = (name) => {
    const threads = new Set(baseline.map((line) => line.split(" ")[0]).filter(Boolean));
    threads.delete(worker);
    return Math.max(0, ...[...threads].map((thread) => calls(name, thread).length));
  };
  const publication = calls("renameat2");
  assert.equal(publication.length, 1, "a capture is published by one rename");
  const order = (line) => baseline.indexOf(line);
  const lastFile = new RegExp(`/attachments/\\.partial-[^/>]+/${String(last)}>`, "u");
  const points = [
    ["file", "fsync", (line) => lastFile.test(line)],
    ["directory", "fsync", (line) => /\/attachments\/\.partial-[^/>]+>/u.test(line)],
    ["publication", "renameat2", (line) => line.includes("renameat2(")],
    ["store", "fsync", (line) => /\/state\/attachments>/u.test(line)],
    ["full", "write", (line) => lastFile.test(line)],
  ];
  for (const [name, call, predicate] of points) {
    const candidates = calls(call);
    const index = candidates.findIndex(predicate);
    assert.ok(index >= 0, `missing capture boundary: ${name}`);
    assert.ok(index + 1 > others(call), `injection must not hit another thread: ${name}`);
    const published = order(publication[0]);
    if (name === "store") assert.ok(order(candidates[index]) > published, "store sync follows");
    else if (call !== "renameat2")
      assert.ok(order(candidates[index]) < published, `${name} precedes publication`);
    const error = name === "full" ? "ENOSPC" : "EIO";
    await attachmentAttempt(name, { call, error, ordinal: index + 1, predicate, wakers });
    console.log(`attachment durability failure passed: ${name}`);
  }
}

async function attachmentAttempt(name, fault) {
  // Distinct from the other cases sharing this scratch directory.
  const root = join(scratch, `capture-${name}`);
  run("--home", root, "init", "--workspace", scratch, ...initOptions);
  const config = join(root, "carillon.toml");
  // Captures here must not depend on the host's free space.
  await writeFile(
    config,
    `\n[core.attachments]\nfree_space_mib = 0\nmax_files = ${String(STAGED_FILES)}\n`,
    { flag: "a" },
  );
  run("--home", root, "provider", "runtime-pi", "enable");
  const batch = join(root, "state/components/discord-private/incoming/batch");
  await mkdir(batch, { recursive: true });
  const files = Array.from(
    { length: STAGED_FILES },
    (_, ordinal) => `attached file ${String(ordinal)}`,
  );
  for (const [ordinal, content] of files.entries())
    await writeFile(join(batch, String(ordinal)), content);
  const token = (await readFile(join(root, "credentials/discord-private.token"), "utf8")).trim();
  const accept = (base) =>
    fetch(`${base}/v1/events`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${token}`,
        "x-carillon-provider": "discord-private",
        "content-type": "application/json",
      },
      body: JSON.stringify({
        event_id: "attached",
        agent_id: "default",
        user_id: "200",
        audience_id: "test",
        delivery_target_id: "300",
        feedback_target_id: "300:attached",
        content: "",
        attachments: {
          batch: "batch",
          files: files.map((content, ordinal) => ({
            name: `${String(ordinal)}.txt`,
            size: Buffer.byteLength(content),
          })),
        },
      }),
      signal: AbortSignal.timeout(10_000),
    });
  const store = join(root, "state/attachments");
  const entries = async () => {
    try {
      return (await readdir(store)).sort();
    } catch (error) {
      if (error.code === "ENOENT") return [];
      throw error;
    }
  };
  // What core kept: one published directory holding every file.
  const verify = async () => {
    const [turn] = JSON.parse(run("--home", root, "recent"));
    const inspection = JSON.parse(run("--home", root, "inspect", turn.turn_id));
    assert.equal(inspection.attachments.length, STAGED_FILES);
    assert.deepEqual(await entries(), [inspection.attachments[0].path.split("/")[0]]);
    for (const [ordinal, attachment] of inspection.attachments.entries())
      assert.equal(await readFile(join(store, attachment.path), "utf8"), files[ordinal]);
  };
  const log = join(scratch, `capture-${name}.trace`);
  const trace = "trace=mkdir,rename,renameat2,fsync,fdatasync,write";
  const args = ["-f", "-qq", "-yy", "-s", "256", "-e", trace, "-o", log];
  if (fault) {
    args.push("-e", `inject=${fault.call}:error=${fault.error}:when=${String(fault.ordinal)}`);
    if (fault.call === "write" && fault.wakers.length > 0)
      args.push("-e", `trace-fds=!${fault.wakers.join(",")}`);
  }
  await withCore(["strace", [...args, binary, "core", "--config", config]], async (base) => {
    const response = await accept(base);
    const body = await response.text();
    if (fault === null) {
      assert.equal(response.status, 200, body);
      await verify();
      return;
    }
    assert.equal(response.status, fault.error === "ENOSPC" ? 507 : 500, body);
    const injected = (await readFile(log, "utf8"))
      .split("\n")
      .filter((line) => line.includes("INJECTED"));
    assert.equal(injected.length, 1, injected.join("\n"));
    assert.ok(fault.predicate(injected[0]), `wrong injection boundary: ${injected[0]}`);
    assert.deepEqual(
      JSON.parse(run("--home", root, "recent")),
      [],
      "a failed capture accepted work",
    );
    assert.ok(
      (await entries()).every((entry) => !entry.startsWith(".partial-")),
      "an unpublished copy survived its failure",
    );
  });
  if (fault !== null) {
    // Exclusive startup removes a published copy no turn references, and
    // the same submission then succeeds.
    await withCore([binary, ["core", "--config", config]], async (base) => {
      assert.deepEqual(await entries(), []);
      const response = await accept(base);
      assert.equal(response.status, 200, await response.text());
      await verify();
    });
  }
  return (await readFile(log, "utf8")).split("\n");
}

/** Run core, wait until it listens, run `body`, then stop it. */
async function withCore([command, args], body) {
  const server = spawn(command, args, {
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
    return await body(base);
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
