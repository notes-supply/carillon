import { mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { defineConfig } from "vitest/config";
import { HOME_VARIABLES } from "./packages/runtime-t3/src/profiles.ts";

// Tests and the programs they start never see the real home: a profile, a
// harness login or an installation a test resolves by default lands here.
const home = join(tmpdir(), `carillon-vitest-home-${String(process.pid)}`);
mkdirSync(home, { recursive: true });
process.on("exit", () => {
  rmSync(home, { recursive: true, force: true });
});

export default defineConfig({
  test: {
    env: {
      HOME: home,
      XDG_CONFIG_HOME: join(home, ".config"),
      XDG_DATA_HOME: join(home, ".local", "share"),
      XDG_STATE_HOME: join(home, ".local", "state"),
      CARILLON_HOME: join(home, ".carillon"),
      ...Object.fromEntries(
        [...HOME_VARIABLES].map(([harness, variable]) => [variable, join(home, `.${harness}`)]),
      ),
    },
  },
});
