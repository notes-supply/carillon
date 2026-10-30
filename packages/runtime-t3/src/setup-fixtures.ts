import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
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
 * An installation registering runtime-t3 with two agents of plain T3 Code,
 * `notes` and `code`, working in `workspace/`.
 */
export async function installation(): Promise<{ root: string; workspace: string; config: string }> {
  const root = await mkdtemp(join(tmpdir(), "carillon-t3-setup-"));
  cleanups.push(() => rm(root, { recursive: true }));
  const workspace = join(root, "workspace");
  await mkdir(workspace);
  await mkdir(join(root, "credentials"));
  await mkdir(join(root, "providers"));
  const config = join(root, "carillon.toml");
  await writeFile(
    config,
    [
      'data_dir = "state"',
      "[core]",
      'listen = "127.0.0.1:1"',
      `operator_token_hash = "${HASH}"`,
      "[[agents]]",
      'id = "notes"',
      'name = "Notes app"',
      'runtime = "runtime-t3"',
      `workspace = "${workspace}"`,
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
  const child = spawn(process.execPath, [join(import.meta.dirname, "cli.ts"), ...args]);
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk: Buffer) => (stdout += chunk.toString()));
  child.stderr.on("data", (chunk: Buffer) => (stderr += chunk.toString()));
  child.stdin.end(input);
  await once(child, "close");
  return { status: child.exitCode, stdout, stderr };
}

export function cleanup(action: () => Promise<void>): void {
  cleanups.push(action);
}
