import { lstat, mkdir, readdir, readlink, rm, symlink } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import type { Installation } from "@carillon/runtime/installation";

/** The harnesses Carillon makes profiles for, as T3 drives them. */
export const HARNESSES = {
  claude: {
    driver: "claudeAgent",
    name: "Claude · Carillon",
    /** The file the harness reads its user instructions from, in its home. */
    instructions: "CLAUDE.md",
    /** The variable an API key goes in, and the endpoint and its token for another one. */
    key: "ANTHROPIC_API_KEY",
    endpoint: "ANTHROPIC_BASE_URL",
    endpointKey: "ANTHROPIC_AUTH_TOKEN",
  },
  codex: {
    driver: "codex",
    name: "Codex · Carillon",
    instructions: "AGENTS.md",
    key: "OPENAI_API_KEY",
    endpoint: "OPENAI_BASE_URL",
    endpointKey: "OPENAI_API_KEY",
  },
} as const;
export type Harness = keyof typeof HARNESSES;

/** Where the harness's profile lives: its home, under the XDG data directory. */
export function profileHome(harness: Harness): string {
  const data = process.env["XDG_DATA_HOME"];
  return resolve(
    data === undefined || data === "" ? join(homedir(), ".local", "share") : data,
    "carillon",
    "profiles",
    harness,
  );
}

/** The runtime's sole assistant resources, if it serves an assistant. */
export function assistantResources(installation: Installation): string | undefined {
  const assistants = installation.agents.filter((agent) => agent.resources !== undefined);
  if (assistants.length > 1)
    throw new Error(
      `${installation.runtime.id} may serve only one agent with resources in carillon.toml`,
    );
  return assistants[0]?.resources;
}

/** Reconciles every Carillon profile before the runtime starts. */
export async function reconcileProfiles(
  installation: Installation,
  profiles: ReadonlySet<string>,
): Promise<void> {
  const resources = assistantResources(installation);
  if (resources === undefined) return;
  for (const harness of ["claude", "codex"] as const)
    if (profiles.has(`carillon_${harness}`)) await reconcileProfile(harness, resources);
}

/** Shares Carillon's skills without giving it ownership of the harness's skills directory. */
export async function reconcileProfile(harness: Harness, resources: string): Promise<void> {
  const home = profileHome(harness);
  const instructions = join(home, HARNESSES[harness].instructions);
  const previousInstructions = await targetOf(instructions);
  const previousResources =
    previousInstructions === undefined ? undefined : dirname(previousInstructions);
  const source = join(resources, "skills");
  const skills = join(home, "skills");
  await mkdir(home, { recursive: true, mode: 0o700 });
  await mkdir(source, { recursive: true, mode: 0o700 });
  const present = await entry(skills);
  if (present?.isSymbolicLink() === true) await rm(skills);
  else if (present !== undefined && !present.isDirectory())
    throw new Error(
      `${skills} must be the harness's own skills directory; move it away, then retry`,
    );
  await mkdir(skills, { recursive: true, mode: 0o700 });
  const names = new Set(await readdir(source));
  for (const name of await readdir(skills)) {
    const path = join(skills, name);
    const target = await targetOf(path);
    if (
      !names.has(name) &&
      target !== undefined &&
      (target === join(source, name) ||
        (previousResources !== undefined && target === join(previousResources, "skills", name)))
    )
      await rm(path);
  }
  for (const name of names)
    await link(
      join(skills, name),
      join(source, name),
      previousResources === undefined ? undefined : join(previousResources, "skills", name),
    );
  // Keep the old instructions link until every skill moved, so a retry still knows their old root.
  await link(instructions, join(resources, "AGENTS.md"), previousInstructions);
}

async function entry(path: string) {
  return lstat(path).catch((failure: unknown) => {
    if (failure instanceof Error && "code" in failure && failure.code === "ENOENT")
      return undefined;
    throw failure;
  });
}

async function targetOf(path: string): Promise<string | undefined> {
  return (await entry(path))?.isSymbolicLink() === true
    ? resolve(dirname(path), await readlink(path))
    : undefined;
}

async function link(path: string, target: string, previous: string | undefined): Promise<void> {
  const present = await entry(path);
  if (present !== undefined) {
    const actual = present.isSymbolicLink()
      ? resolve(dirname(path), await readlink(path))
      : undefined;
    if (actual === target) return;
    if (actual === undefined || actual !== previous)
      throw new Error(
        `${path} is not a link to ${target}; move it away, then set the profile up again`,
      );
    await rm(path);
  }
  await symlink(target, path);
}
