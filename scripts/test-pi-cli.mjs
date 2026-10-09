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
  console.log(
    "cli: packaged Pi mode, version, help, runtime default and bundled login flows passed",
  );
} finally {
  await rm(root, { recursive: true, force: true });
}
