import { spawnSync } from "node:child_process";
import { readFile, readdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { expect, it } from "vitest";
import { installation } from "./setup-fixtures.ts";

it.each(["pi", "t3", "t3-stale"])(
  "exits successfully from serve and check when %s has no assigned agent",
  async (scenario) => {
    const { root, config } = await installation();
    const runtime = scenario === "pi" ? "pi" : "t3";
    await writeFile(
      config,
      (await readFile(config, "utf8"))
        .replaceAll('runtime = "runtime-t3"', 'runtime = "runtime-other"')
        .replace('id = "runtime-t3"', `id = "runtime-${runtime}"`),
    );
    if (scenario === "t3-stale")
      await writeFile(
        join(root, "providers", "runtime-t3.toml"),
        'url = "http://127.0.0.1:1"\ntoken_file = "missing.token"\n[agents.assistant]\nproject_id = "p"\n[agents.assistant.model]\ninstance_id = "carillon_claude"\nmodel = "m"\n',
      );
    for (const check of [false, true]) {
      const args = ["--config", config, "--provider", `runtime-${runtime}`];
      if (check) {
        if (runtime === "pi") args.push("--check");
        else args.unshift("check");
      }
      const result = spawnSync(
        process.execPath,
        [join(import.meta.dirname, `../../runtime-${runtime}/src/cli.ts`), ...args],
        {
          encoding: "utf8",
          timeout: 10_000,
          env: { ...process.env, HOME: root, XDG_DATA_HOME: join(root, "data") },
        },
      );
      expect(result.error).toBeUndefined();
      expect(result.status, result.stderr).toBe(0);
      expect(result.stdout).toContain(`runtime-${runtime} serves no agent`);
    }
    expect(await readdir(join(root, "credentials"))).toEqual([]);
    expect(await readdir(root)).not.toContain("state");
    expect(await readdir(root)).not.toContain("data");
  },
);
