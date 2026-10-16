import {
  lstat,
  mkdir,
  readdir,
  readlink,
  rename,
  rm,
  stat,
  symlink,
  unlink,
} from "node:fs/promises";
import { homedir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import type { Installation } from "@carillon/runtime/installation";
import type { Static } from "typebox";
import { Check } from "typebox/value";
import { RPC, ServerSettingsSchema } from "./contracts.ts";
import { T3Failure, type T3Connection } from "./t3.ts";

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

/** Whether the name is one of Carillon's supported harnesses. */
export function isHarness(value: string): value is Harness {
  return Object.hasOwn(HARNESSES, value);
}

/** The default home for a new profile, under the setup user's XDG data directory. */
export function profileHome(harness: Harness): string {
  const data = process.env["XDG_DATA_HOME"];
  return resolve(
    data === undefined || data === "" ? join(homedir(), ".local", "share") : data,
    "carillon",
    "profiles",
    harness,
  );
}

/** T3's authoritative home for an existing Carillon profile, refusing incomplete or conflicting instances. */
export function instanceHome(
  harness: Harness,
  instance: NonNullable<Static<typeof ServerSettingsSchema>["providerInstances"]>[string],
): string {
  const instanceId = `carillon_${harness}`;
  if (instance.driver !== HARNESSES[harness].driver)
    throw new Error(
      `T3 instance ${instanceId} is not this profile; correct its driver in T3 settings`,
    );
  const home = instance.config?.homePath;
  if (home === undefined || home.trim().length === 0)
    throw new Error(
      `T3 profile ${instanceId} has no homePath; set its config.homePath in T3 settings`,
    );
  return home;
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

/** Reconciles saved profiles at T3's homes before the runtime starts serving. */
export async function reconcileProfiles(
  installation: Installation,
  profiles: ReadonlySet<string>,
  connection: T3Connection,
): Promise<void> {
  const resources = assistantResources(installation);
  if (resources === undefined || profiles.size === 0) return;
  const settings: unknown = await connection.call(RPC.getSettings, {});
  if (!Check(ServerSettingsSchema, settings))
    throw new T3Failure("malformed", "T3 described unreadable provider settings");
  const homes = [...profiles].map((instanceId) => {
    const harness = Object.keys(HARNESSES)
      .filter(isHarness)
      .find((name) => instanceId === `carillon_${name}`);
    if (harness === undefined) throw new Error(`unsupported Carillon profile ${instanceId}`);
    const instance = settings.providerInstances?.[instanceId];
    if (instance === undefined)
      throw new Error(`T3 has no saved profile ${instanceId}; set the profile up again`);
    return { harness, home: instanceHome(harness, instance) };
  });
  for (const { harness, home } of homes) await reconcileProfile(harness, home, resources);
}

/** Shares Carillon's skills without giving it ownership of the harness's skills directory. */
export async function reconcileProfile(
  harness: Harness,
  home: string,
  resources: string,
): Promise<void> {
  if (
    !(await stat(resources).then(
      (found) => found.isDirectory(),
      () => false,
    ))
  )
    throw new Error(`${resources}: the assistant's resources must be an existing directory`);
  const source = join(resources, "skills");
  await rejectResourceSymlinks(join(resources, "AGENTS.md"));
  await rejectResourceSymlinks(source);
  const instructions = join(home, HARNESSES[harness].instructions);
  const previousInstructions = await targetOf(instructions);
  const previousResources =
    previousInstructions === undefined ? undefined : dirname(previousInstructions);
  const skills = join(home, "skills");
  await mkdir(home, { recursive: true, mode: 0o700 });
  await mkdir(source, { recursive: true, mode: 0o700 });
  const present = await entry(skills);
  if (present?.isSymbolicLink() === true)
    await unlink(skills).catch((failure: unknown) => {
      // A concurrent reconcile may already have replaced the legacy directory link.
      if (
        failure instanceof Error &&
        "code" in failure &&
        (failure.code === "ENOENT" || failure.code === "EISDIR")
      )
        return;
      throw failure;
    });
  else if (present !== undefined && !present.isDirectory())
    throw new Error(
      `${skills} must be the harness's own skills directory; move it away, then retry`,
    );
  await mkdir(skills, { recursive: true, mode: 0o700 });
  const names = new Set<string>();
  for (const directory of await readdir(source, { withFileTypes: true })) {
    if (!directory.isDirectory()) continue;
    const skill = await stat(join(source, directory.name, "SKILL.md")).catch((failure: unknown) => {
      if (
        failure instanceof Error &&
        "code" in failure &&
        (failure.code === "ENOENT" || failure.code === "ENOTDIR")
      )
        return undefined;
      throw failure;
    });
    if (skill?.isFile() === true) names.add(directory.name);
  }
  for (const name of await readdir(skills)) {
    const path = join(skills, name);
    const target = await targetOf(path);
    const skillName =
      /^\.(.+)\.[\da-f]{8}-[\da-f]{4}-[\da-f]{4}-[\da-f]{4}-[\da-f]{12}\.tmp$/u.exec(name)?.[1] ??
      name;
    if (
      !names.has(name) &&
      target !== undefined &&
      (target === join(source, skillName) ||
        (previousResources !== undefined &&
          target === join(previousResources, "skills", skillName)))
    )
      await rm(path, { force: true });
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

async function rejectResourceSymlinks(path: string): Promise<void> {
  const found = await entry(path);
  if (found?.isSymbolicLink() === true)
    throw new Error(`resource symlinks are not supported: ${path}`);
  if (found?.isDirectory() === true)
    for (const name of await readdir(path)) await rejectResourceSymlinks(join(path, name));
}

async function entry(path: string) {
  return lstat(path).catch((failure: unknown) => {
    if (failure instanceof Error && "code" in failure && failure.code === "ENOENT")
      return undefined;
    throw failure;
  });
}

async function targetOf(path: string): Promise<string | undefined> {
  return readlink(path).then(
    (target) => resolve(dirname(path), target),
    (failure: unknown) => {
      if (
        failure instanceof Error &&
        "code" in failure &&
        (failure.code === "ENOENT" || failure.code === "EINVAL")
      )
        return undefined;
      throw failure;
    },
  );
}

async function link(path: string, target: string, previous: string | undefined): Promise<void> {
  const present = await entry(path);
  if (present !== undefined) {
    const actual = present.isSymbolicLink()
      ? resolve(dirname(path), await readlink(path))
      : undefined;
    if (actual === target) return;
    if (actual === undefined || actual !== previous)
      throw new Error(`${path} is not a link to ${target}; move it away, then restart the runtime`);
  }
  for (;;) {
    const temporary = join(dirname(path), `.${basename(path)}.${crypto.randomUUID()}.tmp`);
    try {
      await symlink(target, temporary);
      try {
        await rename(temporary, path);
        return;
      } catch (failure) {
        // Another reconcile can remove this temporary link before it is published.
        if (!(failure instanceof Error && "code" in failure && failure.code === "ENOENT"))
          throw failure;
      }
    } finally {
      await rm(temporary, { force: true });
    }
  }
}
