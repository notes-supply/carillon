// Chooses the CI lanes a change needs. `node scripts/ci-plan.mjs BASE`
// compares BASE with HEAD; `node scripts/ci-plan.mjs --all REASON` selects
// everything. Prints `lane=true|false` lines and a JSON-encoded `reason=`
// line for $GITHUB_OUTPUT. A path no rule recognises, or a comparison git
// cannot make, selects every lane.
import { execFileSync } from "node:child_process";
import { pathToFileURL } from "node:url";

export const LANES = ["rust", "ts", "release", "workflows"];

/**
 * The lanes one changed path needs, or undefined when no rule knows it.
 * The first matching rule decides.
 */
function lanesOf(path) {
  // A change to CI itself proves itself on every lane.
  if (/^\.github\/|^scripts\/ci-plan(\.test)?\.mjs$/u.test(path)) return LANES;
  // Root tools and their lock decide every lane's toolchain.
  if (path === "mise.toml" || path === "mise.lock") return LANES;
  // The Discord skill ships in the release; Rust reads protocol and
  // operations fixtures, and core embeds each runtime's package version.
  if (path === "packages/discord-pi/skill/SKILL.md") return ["ts", "release"];
  if (/^packages\/(protocol|discord-pi)\/fixtures\//u.test(path)) return ["rust", "ts", "release"];
  if (/^packages\/runtime-(pi|t3)\/package\.json$/u.test(path)) return ["rust", "ts", "release"];
  // The release's own licence and notices.
  if (path === "LICENSE" || path.startsWith("licenses/")) return ["release"];
  // Other documentation reaches no build or test.
  if (path.endsWith(".md")) return [];
  // Rust, including the example module the release ships built.
  if (/^(crates\/|examples\/|Cargo\.(toml|lock)$|deny\.toml$)/u.test(path)) {
    return /\.(json|mjs|ts)$/u.test(path) ? ["rust", "ts", "release"] : ["rust", "release"];
  }
  if (
    path.startsWith("packages/") ||
    /^(package\.json|pnpm-lock\.yaml|pnpm-workspace\.yaml|tsconfig\.json)$/u.test(path)
  )
    return ["ts", "release"];
  // The public installer, which the release lane tests.
  if (path === "install.sh") return ["release"];
  if (path.startsWith("scripts/") || path.startsWith("deploy/")) {
    return /\.(json|mjs|ts)$/u.test(path) ? ["ts", "release"] : ["release"];
  }
  return undefined;
}

/** The lanes a set of changed paths needs, with the reason. */
export function plan(paths) {
  const selected = new Set();
  for (const path of paths) {
    const lanes = lanesOf(path);
    if (lanes === undefined) return all(`no rule for ${path}`);
    for (const lane of lanes) selected.add(lane);
  }
  return {
    ...Object.fromEntries(LANES.map((lane) => [lane, selected.has(lane)])),
    reason: `${String(paths.length)} changed path(s)`,
  };
}

export function all(reason) {
  return { ...Object.fromEntries(LANES.map((lane) => [lane, true])), reason };
}

/** The paths changed between `base` and HEAD, or undefined when git cannot tell. */
function changed(base) {
  try {
    return execFileSync("git", ["diff", "--name-only", "--no-renames", "-z", base, "HEAD"], {
      encoding: "utf8",
    })
      .split("\0")
      .filter(Boolean);
  } catch {
    return undefined;
  }
}

/**
 * The plan as $GITHUB_OUTPUT lines. The reason can quote a path, which may
 * hold a newline, so it is JSON-encoded to stay on its own line.
 */
export function outputs(result) {
  const lanes = LANES.map((lane) => `${lane}=${String(result[lane])}\n`).join("");
  return `${lanes}reason=${JSON.stringify(result.reason)}\n`;
}

function main(argv) {
  const [first, ...rest] = argv;
  let result;
  if (first === "--all") result = all(rest.join(" ") || "full run requested");
  else if (first === undefined || first === "") result = all("no comparison base");
  else {
    const paths = changed(first);
    result = paths === undefined ? all(`cannot compare ${first} with HEAD`) : plan(paths);
  }
  process.stdout.write(outputs(result));
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) main(process.argv.slice(2));
