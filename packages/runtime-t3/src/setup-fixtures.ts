import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { loadInstallation, type Installation } from "@carillon/runtime/installation";
import { afterEach } from "vitest";
import { startFakeT3, type FakeT3 } from "./fake.ts";

const cleanups: (() => Promise<void>)[] = [];
afterEach(async () => {
  await Promise.all(cleanups.splice(0).map((cleanup) => cleanup()));
});

export async function fakeT3(token?: string): Promise<FakeT3> {
  const fake = await startFakeT3(token);
  cleanups.push(() => fake.close());
  return fake;
}

const HASH = "0".repeat(64);

/**
 * An installation registering runtime-t3, with its assistant, whose
 * resources are `agent/`, and plain T3 Code, `code`, working in `workspace/`.
 */
export async function installation(): Promise<{ root: string; workspace: string; config: string }> {
  const root = await mkdtemp(join(tmpdir(), "carillon-t3-setup-"));
  cleanups.push(() => rm(root, { recursive: true }));
  useDataHome(root);
  const workspace = join(root, "workspace");
  await mkdir(workspace);
  await mkdir(join(root, "agent"));
  await writeFile(join(root, "agent", "AGENTS.md"), "Carillon's instructions\n");
  await mkdir(join(root, "credentials"));
  await mkdir(join(root, "providers"));
  const config = join(root, "carillon.toml");
  await writeFile(
    config,
    [
      'data_dir = "state"',
      "[core]",
      `operator_token_hash = "${HASH}"`,
      "[[agents]]",
      'id = "assistant"',
      'name = "Carillon"',
      'runtime = "runtime-t3"',
      `workspace = "${workspace}"`,
      'resources = "agent"',
      "[[agents]]",
      'id = "code"',
      'name = "T3 Code"',
      'runtime = "runtime-t3"',
      `workspace = "${workspace}"`,
      "[[providers]]",
      'id = "runtime-t3"',
      'role = "runtime"',
      'version = "0.0.0"',
      'url = "http://127.0.0.1:1"',
      `token_hash = "${HASH}"`,
      "enabled = true",
    ].join("\n"),
  );
  return { root, workspace, config };
}

export async function loaded(config: string): Promise<Installation> {
  const result = await loadInstallation(config, "runtime-t3");
  if (typeof result === "string") throw new Error(`runtime-t3 is ${result}`);
  return result;
}

/** The CLI with `input` on standard input; the fake T3 serves from this process meanwhile. */
export async function cli(
  args: readonly string[],
  input: string,
): Promise<{ status: number | null; stdout: string; stderr: string }> {
  const configIndex = args.indexOf("--config");
  const config = configIndex === -1 ? undefined : args[configIndex + 1];
  const child = spawn(process.execPath, [join(import.meta.dirname, "cli.ts"), ...args], {
    env: {
      ...process.env,
      XDG_DATA_HOME:
        config === undefined ? process.env["XDG_DATA_HOME"] : join(dirname(config), "data"),
    },
  });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk: Buffer) => (stdout += chunk.toString()));
  child.stderr.on("data", (chunk: Buffer) => (stderr += chunk.toString()));
  child.stdin.end(input);
  await once(child, "close");
  return { status: child.exitCode, stdout, stderr };
}

function useDataHome(root: string): string {
  const data = process.env["XDG_DATA_HOME"];
  const path = join(root, "data");
  process.env["XDG_DATA_HOME"] = path;
  cleanups.push(async () => {
    if (data === undefined) delete process.env["XDG_DATA_HOME"];
    else process.env["XDG_DATA_HOME"] = data;
  });
  return path;
}

export function cleanup(action: () => Promise<void>): void {
  cleanups.push(action);
}

export async function serve(config: string): Promise<{
  status: number | null;
  stdout: string;
  stderr: string;
}> {
  const listener = createServer();
  listener.listen(0, "127.0.0.1");
  await once(listener, "listening");
  const address = listener.address();
  if (address === null || typeof address === "string") throw new Error("no TCP address");
  await new Promise<void>((done) => listener.close(() => done()));
  const token = "runtime-test-token";
  const tokenFile = join(dirname(config), "credentials", "runtime.token");
  await writeFile(tokenFile, token);
  await writeFile(
    config,
    (await readFile(config, "utf8"))
      .replace(
        /^token_hash = ".*"$/mu,
        `token_hash = "${createHash("sha256").update(token).digest("hex")}"`,
      )
      .replace('url = "http://127.0.0.1:1"', `url = "http://127.0.0.1:${String(address.port)}"`),
  );
  const child = spawn(
    process.execPath,
    [
      join(import.meta.dirname, "cli.ts"),
      "--config",
      config,
      "--provider",
      "runtime-t3",
      "--token-file",
      tokenFile,
    ],
    { cwd: dirname(config) },
  );
  const timeout = setTimeout(() => child.kill("SIGKILL"), 5_000);
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk: Buffer) => {
    stdout += chunk.toString();
    if (stdout.includes("runtime-t3 ready")) child.kill("SIGTERM");
  });
  child.stderr.on("data", (chunk: Buffer) => (stderr += chunk.toString()));
  child.stdin.end();
  try {
    await once(child, "close");
    return { status: child.exitCode, stdout, stderr };
  } finally {
    clearTimeout(timeout);
    child.kill("SIGKILL");
  }
}
