// The packaged Pi command line: a mode of the runtime executable, selected by
// its first argument, with Pi's own version, help and bundled login flows. No
// login starts, no provider is reached and no real credential is read.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const release = resolve(
  process.argv[2] ?? assert.fail("usage: node scripts/test-pi-cli.mjs RELEASE_DIRECTORY"),
);
const executable = join(release, "bin/carillon-runtime-pi");
const root = await mkdtemp(join(tmpdir(), "carillon-packaged-pi-cli-"));

/** Runs the executable with only an isolated home and Pi directory. */
function run(args, home) {
  const result = spawnSync(executable, args, {
    cwd: home,
    encoding: "utf8",
    timeout: 30_000,
    env: {
      HOME: home,
      PATH: "/usr/bin:/bin",
      PI_CODING_AGENT_DIR: join(home, "pi"),
      PI_OFFLINE: "1",
    },
  });
  assert.equal(result.error, undefined, String(result.error));
  return result;
}

try {
  const home = join(root, "home");
  await mkdir(join(home, "pi"), { recursive: true });
  const { version } = JSON.parse(await readFile(join(release, "bin/package.json"), "utf8"));

  const reported = run(["pi", "--version"], home);
  assert.equal(reported.status, 0, reported.stderr);
  assert.equal(reported.stdout.trim(), version);

  // Pi reads its own arguments: the mode is not one of them.
  const help = run(["pi", "--help"], home);
  assert.equal(help.status, 0, help.stderr);
  assert.match(help.stdout, /^Usage:\n {2}pi \[options\]/mu);

  // Without the mode, the executable is the runtime, and Pi's arguments are not its own.
  const runtime = run(["--version"], home);
  assert.notEqual(runtime.status, 0);
  assert.notEqual(runtime.stdout.trim(), version);

  // Pi's Bun setup registers the login flows embedded in the executable: a
  // stored subscription credential resolves through its flow without a request.
  await writeFile(
    join(home, "pi", "auth.json"),
    JSON.stringify({
      anthropic: {
        type: "oauth",
        access: "fixture-access-token",
        refresh: "fixture-refresh-token",
        expires: 4_102_444_800_000,
      },
    }),
    { mode: 0o600 },
  );
  const bearer = run(["pi", "auth", "print-bearer-token", "--provider", "anthropic"], home);
  assert.equal(bearer.status, 0, bearer.stderr);
  assert.equal(bearer.stdout.trim(), "fixture-access-token");
  // The resource check runs the packaged executable again as its child.
  const hash = "0".repeat(64);
  await mkdir(join(home, "agents", "assistant", "extensions"), { recursive: true });
  await writeFile(
    join(home, "carillon.toml"),
    [
      'data_dir = "state"',
      "[core]",
      'listen = "127.0.0.1:1"',
      `operator_token_hash = "${hash}"`,
      "[[providers]]",
      'id = "runtime-pi"',
      'role = "runtime"',
      'program = "runtime-pi"',
      'version = "0.0.0"',
      'url = "http://127.0.0.1:2"',
      `token_hash = "${hash}"`,
      "enabled = true",
      "[[agents]]",
      'id = "assistant"',
      'name = "assistant"',
      'runtime = "runtime-pi"',
      'workspace = "."',
      'resources = "agents/assistant"',
      "",
    ].join("\n"),
  );
  await writeFile(
    join(home, "agents", "assistant", "settings.json"),
    JSON.stringify({ extensions: { add: ["extensions/mine.ts"] } }),
  );
  const module = join(home, "agents", "assistant", "extensions", "mine.ts");
  const check = () =>
    run(["check-resources", "--config", join(home, "carillon.toml"), "--agent", "assistant"], home);
  await writeFile(module, "const hooks: unknown[] = []; export default { hooks };");
  const loads = check();
  assert.equal(loads.status, 0, loads.stderr);
  assert.match(loads.stdout, /agents\/assistant loads\.\n$/u);
  await writeFile(module, "export default { hooks: [ };");
  const broken = check();
  assert.equal(broken.status, 1);
  assert.match(broken.stderr, /extensions\/mine\.ts:1:27: Unexpected \}/u);
  await writeFile(module, "process.exit(0);");
  const exits = check();
  assert.equal(exits.status, 1);
  assert.match(exits.stderr, /something ended the process \(exit code 0\)/u);
  console.log(
    "cli: packaged Pi mode, version, help, runtime default, bundled login flows and resource check passed",
  );
} finally {
  await rm(root, { recursive: true, force: true });
}
