import { execFileSync } from "node:child_process";
import { expect, it } from "vitest";
import { outputs, plan } from "./ci-plan.mjs";

const lanes = (paths) => {
  const { reason: _, ...selected } = plan(paths);
  return Object.keys(selected)
    .filter((lane) => selected[lane])
    .sort();
};

it("runs nothing for documentation alone", () => {
  expect(
    lanes(["README.md", "docs/product/OPERATIONS.md", "crates/carillon-core/README.md"]),
  ).toEqual([]);
});

it("selects each language with the release it ships in", () => {
  expect(lanes(["crates/carillon-core/src/db.rs"])).toEqual(["lint", "release", "rust"]);
  expect(lanes(["Cargo.lock"])).toEqual(["lint", "release", "rust"]);
  // The release ships the example module built.
  expect(lanes(["examples/card-colours/src/lib.rs"])).toEqual(["lint", "release", "rust"]);
  expect(lanes(["packages/runtime-t3/src/runtime.ts"])).toEqual(["lint", "release", "ts"]);
  expect(lanes(["pnpm-lock.yaml"])).toEqual(["lint", "release", "ts"]);
  expect(lanes(["scripts/package-release.sh"])).toEqual(["lint", "release"]);
  expect(lanes(["install.sh"])).toEqual(["lint", "release"]);
  expect(lanes(["scripts/test-integration.mjs"])).toEqual(["lint", "release", "ts"]);
  expect(lanes(["licenses/bun.md"])).toEqual(["lint", "release"]);
});

it("follows inputs across languages", () => {
  // Rust tests read the protocol fixtures; core embeds each runtime's version.
  expect(lanes(["packages/protocol/fixtures/turns.json"])).toEqual([
    "lint",
    "release",
    "rust",
    "ts",
  ]);
  expect(lanes(["packages/runtime-pi/package.json"])).toEqual(["lint", "release", "rust", "ts"]);
  expect(lanes(["packages/runtime-t3/package.json"])).toEqual(["lint", "release", "rust", "ts"]);
  // The shipped skill is documentation the release carries.
  expect(lanes(["packages/discord-pi/skill/SKILL.md"])).toEqual(["lint", "release", "ts"]);
  // TypeScript formatting covers JSON anywhere.
  expect(lanes(["crates/carillon-discord-module/fixtures/cards.json"])).toEqual([
    "lint",
    "release",
    "rust",
    "ts",
  ]);
});

it("lints every change a lane builds, and lint configuration alone", () => {
  expect(lanes(["scripts/register-services.sh"])).toEqual(["lint", "release"]);
  expect(lanes(["sgconfig.yml", "lint/size-budget.txt"])).toEqual(["lint"]);
  expect(lanes([".oxlintrc.json"])).toEqual(["lint", "ts"]);
  expect(lanes([".coderabbit.yaml", "docs/product/PRODUCT.md"])).toEqual([]);
});

it("runs everything for tools, CI and anything unrecognised", () => {
  const every = ["lint", "release", "rust", "ts", "workflows"];
  for (const path of ["mise.toml", "mise.lock", ".github/workflows/ci.yml", "scripts/ci-plan.mjs"])
    expect(lanes([path])).toEqual(every);
  expect(lanes(["docs/guide.md", ".gitignore"])).toEqual(every);
  expect(plan([".gitignore"]).reason).toBe("no rule for .gitignore");
});

it("keeps a path's newline from rewriting the lanes it selected", () => {
  const injected = "x\nrust=false\nts=false\nrelease=false\nworkflows=false";
  const lines = outputs(plan([injected]))
    .trimEnd()
    .split("\n");
  expect(lines).toEqual([
    "rust=true",
    "ts=true",
    "release=true",
    "lint=true",
    "workflows=true",
    `reason=${JSON.stringify(`no rule for ${injected}`)}`,
  ]);
});

it("selects everything when it cannot compare, and nothing for no change", () => {
  const run = (...args) =>
    Object.fromEntries(
      execFileSync("node", ["scripts/ci-plan.mjs", ...args], { encoding: "utf8" })
        .trim()
        .split("\n")
        .map((line) => line.split(/=(.*)/su, 2))
        .map(([key, value]) => [key, key === "reason" ? JSON.parse(value) : value]),
    );
  const missing = run("0000000000000000000000000000000000000000");
  expect(missing).toMatchObject({
    rust: "true",
    ts: "true",
    release: "true",
    lint: "true",
    workflows: "true",
  });
  expect(missing.reason).toMatch(/^cannot compare/u);
  expect(run("")).toMatchObject({ rust: "true", reason: "no comparison base" });
  expect(run("--all", "release", "candidate")).toMatchObject({
    release: "true",
    reason: "release candidate",
  });
  expect(run("HEAD")).toMatchObject({
    rust: "false",
    ts: "false",
    release: "false",
    lint: "false",
    workflows: "false",
  });
});
