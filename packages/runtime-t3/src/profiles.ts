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
import { basename, dirname, isAbsolute, join, resolve } from "node:path";
import type { Installation } from "@carillon/runtime/installation";
import type { Static } from "typebox";
import { Check } from "typebox/value";
import { RPC, ServerSettingsSchema } from "./contracts.ts";
import { T3Failure, type T3Connection } from "./t3.ts";

/** T3 drivers that keep a home in `config.homePath`, and the variable T3 sets from it. */
const HOME_PATH_DRIVERS = { claudeAgent: "CLAUDE_CONFIG_DIR", codex: "CODEX_HOME" } as const;

/**
 * Where T3 keeps a profile's home. A driver with `config.homePath` keeps it
 * there and sets its variable itself. Any other driver keeps it in the
 * instance's environment, in the variable the harness reads, which Carillon
 * sets when it creates the instance and T3 reports back unredacted.
 */
type Placement =
  | {
      [D in keyof typeof HOME_PATH_DRIVERS]: { readonly driver: D; readonly home: "homePath" };
    }[keyof typeof HOME_PATH_DRIVERS]
  | { readonly driver: "opencode"; readonly home: { readonly variable: "OPENCODE_CONFIG_DIR" } };

type SignIn =
  | {
      readonly kind: "own-account";
      readonly program: string;
      readonly login: readonly string[];
      readonly status: readonly string[];
      /** The variable an API key goes in, and the endpoint and its token for another one. */
      readonly key: string;
      readonly endpoint: string;
      readonly endpointKey: string;
    }
  | {
      readonly kind: "shared-account";
      readonly program: string;
      readonly login: readonly string[];
    };

export type HarnessDefinition = Placement & {
  readonly name: string;
  readonly instructions: string;
  /** Variables set for every profile of this harness, none secret. */
  readonly fixedEnvironment: Readonly<Record<string, string>>;
  readonly signIn: SignIn;
};

/**
 * The harnesses, the one list: T3 instances, reserved IDs, discovery,
 * startup and `carillon setup` derive theirs from it.
 */
const HARNESSES = {
  claude: {
    name: "Claude",
    driver: "claudeAgent",
    home: "homePath",
    instructions: "CLAUDE.md",
    fixedEnvironment: {},
    signIn: {
      kind: "own-account",
      program: "claude",
      login: ["auth", "login"],
      status: ["auth", "status"],
      key: "ANTHROPIC_API_KEY",
      endpoint: "ANTHROPIC_BASE_URL",
      endpointKey: "ANTHROPIC_AUTH_TOKEN",
    },
  },
  codex: {
    name: "Codex",
    driver: "codex",
    home: "homePath",
    instructions: "AGENTS.md",
    fixedEnvironment: {},
    signIn: {
      kind: "own-account",
      program: "codex",
      login: ["login", "--device-auth"],
      status: ["login", "status"],
      key: "OPENAI_API_KEY",
      endpoint: "OPENAI_BASE_URL",
      endpointKey: "OPENAI_API_KEY",
    },
  },
  opencode: {
    name: "OpenCode",
    driver: "opencode",
    // OpenCode reads its global configuration from here instead of ~/.config/opencode.
    home: { variable: "OPENCODE_CONFIG_DIR" },
    instructions: "AGENTS.md",
    // The user's ~/.claude instructions and skills stay their own Claude's.
    fixedEnvironment: { OPENCODE_DISABLE_CLAUDE_CODE: "1" },
    // OpenCode keeps sign-ins in its XDG data directory, which no variable of its own moves.
    signIn: { kind: "shared-account", program: "opencode", login: ["auth", "login"] },
  },
} as const satisfies Readonly<Record<string, HarnessDefinition>>;

export type Harness = keyof typeof HARNESSES;
const HARNESS_IDS: readonly Harness[] = Object.keys(HARNESSES).filter(isHarness);

function isHarness(value: string): value is Harness {
  return Object.hasOwn(HARNESSES, value);
}

function homeVariable({ driver, home }: HarnessDefinition): string {
  return home === "homePath" ? HOME_PATH_DRIVERS[driver] : home.variable;
}

export const HOME_VARIABLES: ReadonlyMap<Harness, string> = new Map(
  HARNESS_IDS.map((harness) => [harness, homeVariable(HARNESSES[harness])]),
);

export type ProfileId = `carillon_${Harness}`;

function profileId(harness: Harness): ProfileId {
  return `carillon_${harness}`;
}

interface SetupHarness {
  readonly id: Harness;
  readonly name: string;
  readonly instanceId: ProfileId;
  readonly signIn:
    | {
        readonly kind: "own-account";
        readonly homeVariable: string;
        readonly program: string;
        readonly login: readonly string[];
        readonly status: readonly string[];
      }
    | {
        readonly kind: "shared-account";
        readonly program: string;
        readonly login: readonly string[];
      };
}

export function profileHarnesses(): readonly SetupHarness[] {
  return HARNESS_IDS.map((id) => {
    const definition = HARNESSES[id];
    const { signIn } = definition;
    return {
      id,
      name: definition.name,
      instanceId: profileId(id),
      signIn:
        signIn.kind === "own-account"
          ? {
              kind: "own-account",
              homeVariable: homeVariable(definition),
              program: signIn.program,
              login: signIn.login,
              status: signIn.status,
            }
          : { kind: "shared-account", program: signIn.program, login: signIn.login },
    };
  });
}

export function profileIds(): readonly ProfileId[] {
  return HARNESS_IDS.map(profileId);
}

export function isProfileId(value: string): value is ProfileId {
  return harnessOf(value) !== undefined;
}

function harnessOf(instanceId: string): Harness | undefined {
  return HARNESS_IDS.find((harness) => profileId(harness) === instanceId);
}

/** The default home for a new profile, under the setup user's XDG data directory. */
function profileHome(harness: Harness): string {
  const data = process.env["XDG_DATA_HOME"];
  return resolve(
    data === undefined || data === "" ? join(homedir(), ".local", "share") : data,
    "carillon",
    "profiles",
    harness,
  );
}

type HarnessOfKind<K extends SignIn["kind"]> = {
  [H in Harness]: (typeof HARNESSES)[H]["signIn"]["kind"] extends K ? H : never;
}[Harness];

function ownsAccount(harness: Harness): harness is HarnessOfKind<"own-account"> {
  return HARNESSES[harness].signIn.kind === "own-account";
}

type KeyRequest =
  | { readonly kind: "api-key" }
  | { readonly kind: "compatible-endpoint"; readonly endpoint: string };

export type SignInRequest = { readonly kind: "account" } | KeyRequest;

type OwnCredentials = { readonly kind: "account" } | (KeyRequest & { readonly key: string });

export type ProfileChoice =
  | {
      readonly kind: "own-account";
      readonly harness: HarnessOfKind<"own-account">;
      readonly credentials: OwnCredentials;
    }
  | { readonly kind: "shared-account"; readonly harness: HarnessOfKind<"shared-account"> };

export function profileChoice(
  harness: string,
  request: SignInRequest,
): (key: string) => ProfileChoice {
  if (!isHarness(harness)) throw new Error(`--harness must be one of ${HARNESS_IDS.join(", ")}`);
  if (!ownsAccount(harness)) {
    if (request.kind !== "account") {
      const { name, signIn } = HARNESSES[harness];
      throw new Error(
        `${name}'s profile shares your own ${name} sign-in, so it takes no key or endpoint; add a provider with \`${[signIn.program, ...signIn.login].join(" ")}\` or in ${name}'s own configuration`,
      );
    }
    return () => ({ kind: "shared-account", harness });
  }
  return (key) => ({
    kind: "own-account",
    harness,
    credentials: request.kind === "account" ? request : { ...request, key },
  });
}

export interface Profile {
  readonly instanceId: ProfileId;
  readonly home: string;
}

type ServerSettings = Static<typeof ServerSettingsSchema>;
type ServerInstance = NonNullable<ServerSettings["providerInstances"]>[string];

export async function serverSettings(connection: T3Connection): Promise<ServerSettings> {
  const settings: unknown = await connection.call(RPC.getSettings, {});
  if (!Check(ServerSettingsSchema, settings))
    throw new T3Failure("malformed", "T3 described unreadable provider settings");
  return settings;
}

export async function ensureProfile(
  connection: T3Connection,
  choice: ProfileChoice,
): Promise<Profile> {
  const instanceId = profileId(choice.harness);
  const instance = (await serverSettings(connection)).providerInstances?.[instanceId];
  if (instance === undefined) {
    const home = profileHome(choice.harness);
    await mkdir(home, { recursive: true, mode: 0o700 });
    await connection.call(RPC.updateSettings, instanceCreate(instanceId, choice, home));
    return { instanceId, home };
  }
  const home = instanceHome(choice.harness, instance);
  if (choice.kind === "own-account" && choice.credentials.kind !== "account")
    throw new Error(
      `T3 already has profile ${instanceId}; change its environment in T3 settings. The supplied key or endpoint was not applied`,
    );
  await mkdir(home, { recursive: true, mode: 0o700 });
  return { instanceId, home };
}

function instanceHome(harness: Harness, instance: ServerInstance): string {
  const instanceId = profileId(harness);
  const definition = HARNESSES[harness];
  if (instance.driver !== definition.driver)
    throw new Error(
      `T3 instance ${instanceId} is not this profile; correct its driver in T3 settings`,
    );
  const home = placedHome(instanceId, definition.home, instance);
  for (const [name, value] of Object.entries(definition.fixedEnvironment))
    if (environmentValue(instanceId, instance, name) !== value)
      throw new Error(
        `T3 profile ${instanceId} must set ${name}=${value}; correct it in its environment in T3 settings`,
      );
  if (
    definition.driver === "opencode" &&
    instance.config?.serverUrl !== undefined &&
    instance.config.serverUrl.trim().length > 0
  )
    throw new Error(
      `T3 profile ${instanceId} names a serverUrl, so its environment would not apply; clear config.serverUrl in T3 settings`,
    );
  return home;
}

function placedHome(
  instanceId: ProfileId,
  placement: HarnessDefinition["home"],
  instance: ServerInstance,
): string {
  if (placement === "homePath") {
    const home = instance.config?.homePath;
    if (home === undefined || home.trim().length === 0)
      throw new Error(
        `T3 profile ${instanceId} has no homePath; set its config.homePath in T3 settings`,
      );
    return home;
  }
  const home = environmentValue(instanceId, instance, placement.variable);
  if (home === undefined)
    throw new Error(
      `T3 profile ${instanceId} has no ${placement.variable}; set it in its environment in T3 settings`,
    );
  if (!isAbsolute(home))
    throw new Error(
      `T3 profile ${instanceId}'s ${placement.variable} must be an absolute path, which T3 does not expand; correct it in its environment in T3 settings`,
    );
  return home;
}

function environmentValue(
  instanceId: ProfileId,
  instance: ServerInstance,
  name: string,
): string | undefined {
  const entries = (instance.environment ?? []).filter((entry) => entry.name === name);
  if (entries.length > 1)
    throw new Error(
      `T3 profile ${instanceId} sets ${name} more than once; keep one in its environment in T3 settings`,
    );
  const [entry] = entries;
  if (entry?.valueRedacted === true)
    throw new Error(
      `T3 profile ${instanceId} keeps ${name} secret; make it not sensitive in T3 settings`,
    );
  return entry === undefined || entry.value.trim().length === 0 ? undefined : entry.value;
}

/**
 * Creates the profile's instance in T3's settings, which T3 starts without
 * a restart: the harness's driver, where T3 keeps the home, the harness's
 * fixed environment, and the key the profile signs in with, if any. T3
 * keeps a sensitive value in its secret store.
 */
function instanceCreate(instanceId: ProfileId, choice: ProfileChoice, home: string) {
  const definition = HARNESSES[choice.harness];
  const environment = [
    ...(definition.home === "homePath"
      ? []
      : [{ name: definition.home.variable, value: home, sensitive: false }]),
    ...Object.entries(definition.fixedEnvironment).map(([name, value]) => ({
      name,
      value,
      sensitive: false,
    })),
    ...credentialEnvironment(choice),
  ];
  return {
    patch: {},
    providerInstanceMutation: {
      operation: "create",
      instanceId,
      instance: {
        driver: definition.driver,
        displayName: `${definition.name} · Carillon`,
        enabled: true,
        ...(definition.home === "homePath" ? { config: { homePath: home } } : {}),
        ...(environment.length === 0 ? {} : { environment }),
      },
    },
  } as const;
}

function credentialEnvironment(
  choice: ProfileChoice,
): { name: string; value: string; sensitive: boolean }[] {
  if (choice.kind === "shared-account") return [];
  const { signIn } = HARNESSES[choice.harness];
  const { credentials } = choice;
  if (credentials.kind === "account") return [];
  if (credentials.kind === "api-key")
    return [{ name: signIn.key, value: credentials.key, sensitive: true }];
  return [
    { name: signIn.endpoint, value: credentials.endpoint, sensitive: false },
    { name: signIn.endpointKey, value: credentials.key, sensitive: true },
  ];
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

export async function reconcileProfiles(
  installation: Installation,
  profiles: ReadonlySet<string>,
  connection: T3Connection,
): Promise<void> {
  const resources = assistantResources(installation);
  if (resources === undefined || profiles.size === 0) return;
  const settings = await serverSettings(connection);
  const homes = [...profiles].map((instanceId) => {
    const harness = harnessOf(instanceId);
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
