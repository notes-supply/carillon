import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, it } from "vitest";
import { loadInstallation } from "./installation.ts";

const directories: string[] = [];
afterEach(async () => {
  await Promise.all(directories.splice(0).map((path) => rm(path, { recursive: true })));
});

async function registry(
  interfaceUrl: string,
  interfaceEnabled = true,
  runtimeEnabled = true,
): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "carillon-installation-"));
  directories.push(root);
  const hash = "0".repeat(64);
  const provider = (id: string, role: string, url: string, enabled: boolean) =>
    `[[providers]]\nid = "${id}"\nrole = "${role}"\nversion = "0.0.0"\nurl = "${url}"\ntoken_hash = "${hash}"\nenabled = ${String(enabled)}\n`;
  await writeFile(
    join(root, "carillon.toml"),
    [
      `data_dir = "data"\n[core]\noperator_token_hash = "${hash}"\n[agent]\nresource_root = "agent"\nworkspace = "workspace"\n`,
      provider("runtime-t3", "runtime", "http://127.0.0.1:3101", runtimeEnabled),
      provider("discord", "interface", interfaceUrl, interfaceEnabled),
    ].join("\n"),
  );
  return join(root, "carillon.toml");
}

it("hands extensions only loopback interface endpoints, for every runtime", async () => {
  const loaded = await loadInstallation(await registry("http://127.0.0.1:3102"), "runtime-t3");
  if (loaded === "disabled") throw new Error("an enabled runtime was reported disabled");
  expect(loaded.interfaces.map((provider) => provider.url)).toEqual(["http://127.0.0.1:3102"]);
  await expect(
    loadInstallation(await registry("http://192.0.2.1:8080"), "runtime-t3"),
  ).rejects.toThrow(/provider URL must use an HTTP loopback socket address/u);
  // A disabled interface is not handed to extensions.
  const disabled = await loadInstallation(
    await registry("http://192.0.2.1:8080", false),
    "runtime-t3",
  );
  if (disabled === "disabled") throw new Error("an enabled runtime was reported disabled");
  expect(disabled.interfaces).toEqual([]);
});

it("reports a disabled runtime instead of loading it, for every runtime", async () => {
  await expect(
    loadInstallation(await registry("http://127.0.0.1:3102", true, false), "runtime-t3"),
  ).resolves.toBe("disabled");
  // A runtime missing from the registry is still an error.
  await expect(
    loadInstallation(await registry("http://127.0.0.1:3102"), "runtime-other"),
  ).rejects.toThrow(/runtime provider runtime-other is not configured/u);
});
