import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { stringify } from "smol-toml";
import { expect, it } from "vitest";

async function cli(args: readonly string[]) {
  const child = spawn(process.execPath, [join(import.meta.dirname, "cli.ts"), ...args], {
    signal: AbortSignal.timeout(10_000),
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk: Buffer) => {
    stdout += chunk.toString();
    if (stdout.includes("runtime-pi ready at")) child.kill("SIGTERM");
  });
  child.stderr.on("data", (chunk: Buffer) => (stderr += chunk.toString()));
  await once(child, "close");
  return { status: child.exitCode, stdout, stderr };
}

it.each([
  { mode: "check", root: "missing" },
  { mode: "check", root: "file" },
  { mode: "startup", root: "missing" },
  { mode: "startup", root: "file" },
])("refuses a $root resource root during $mode for every Pi agent", async ({ mode, root }) => {
  const home = await mkdtemp(join(tmpdir(), "carillon-pi-cli-"));
  try {
    await mkdir(join(home, "resources"));
    if (root === "file") await writeFile(join(home, "broken-resources"), "not a directory");
    const token = "pi-cli-test-token";
    const hash = createHash("sha256").update(token).digest("hex");
    const listener = createServer();
    listener.listen(0, "127.0.0.1");
    await once(listener, "listening");
    const address = listener.address();
    if (address === null || typeof address === "string") throw new Error("no TCP address");
    await new Promise<void>((done) => listener.close(() => done()));
    const config = join(home, "carillon.toml");
    const credential = join(home, "runtime.token");
    await writeFile(credential, token);
    await writeFile(
      config,
      stringify({
        data_dir: "state",
        core: { operator_token_hash: hash },
        providers: [
          {
            id: "runtime-pi",
            role: "runtime",
            version: "0.0.0",
            url: `http://127.0.0.1:${String(address.port)}`,
            token_hash: hash,
            enabled: true,
          },
        ],
        agents: [
          {
            id: "ready",
            name: "Ready",
            runtime: "runtime-pi",
            workspace: home,
            resources: "resources",
          },
          {
            id: "broken",
            name: "Broken",
            runtime: "runtime-pi",
            workspace: home,
            resources: "broken-resources",
          },
        ],
      }),
    );
    const args = [
      "--config",
      config,
      "--provider",
      "runtime-pi",
      "--state-dir",
      join(home, "pi"),
      "--token-file",
      credential,
      "--deterministic",
      ...(mode === "check" ? ["--check"] : []),
    ];
    const result = await cli(args);
    expect(result.status, result.stderr).toBe(1);
    expect(result.stderr).toContain("agent broken: resources must be an existing directory");
    if (mode === "check") return;
    if (root === "file") await rm(join(home, "broken-resources"));
    await mkdir(join(home, "broken-resources"));
    const recovered = await cli(args);
    expect(recovered.status, recovered.stderr).toBe(0);
    expect(recovered.stdout).toContain("runtime-pi ready at");
  } finally {
    await rm(home, { recursive: true, force: true });
  }
});
