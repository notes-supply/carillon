/**
 * Setting the runtime up on an existing T3 server: what the server offers,
 * the runtime's settings for an agent's project and model, and Carillon's
 * profiles: T3 instances of their own whose homes link to the assistant's
 * resources. The T3 token is given as a value, never as an argument, and
 * written only to a private credential file the runtime's settings name.
 */
import { open, readFile, realpath, rename, rm } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import type { AvailableModel } from "@carillon/protocol";
import type { Agent, Installation } from "@carillon/runtime/installation";
import { stringify } from "smol-toml";
import { Type } from "typebox";
import { Check } from "typebox/value";
import {
  instanceCreate,
  ORCHESTRATION_PROTOCOL,
  RPC,
  ServerSettingsSchema,
  type InstanceDefinition,
} from "./contracts.ts";
import { catalog } from "./models.ts";
import {
  assistantResources,
  HARNESSES,
  profileHome,
  reconcileProfile,
  type Harness,
} from "./profiles.ts";
import { offers, parseSettings, readSettings, type SettingsFile } from "./settings.ts";
import { connectT3, DEFAULT_TIMEOUT_MS, T3Failure, type Connect, type T3Endpoint } from "./t3.ts";

/** A T3 project: an agent works in its workspace. */
export interface Project {
  readonly id: string;
  readonly title: string;
  readonly workspace: string;
}

/** What a T3 server offers the runtime: its projects and usable models. */
export interface Offer {
  readonly projects: readonly Project[];
  readonly models: readonly AvailableModel[];
}

/** An agent's project and default model, the model as T3 names its provider instance and slug. */
export interface Choice {
  readonly agentId: string;
  readonly projectId: string;
  readonly instanceId: string;
  readonly model: string;
}

const ShellSchema = Type.Object({
  projects: Type.Array(
    Type.Object({
      id: Type.String({ minLength: 1 }),
      title: Type.String({ minLength: 1 }),
      workspaceRoot: Type.String({ minLength: 1 }),
    }),
  ),
});

/**
 * The projects and models the server at `endpoint` offers, after T3 checks
 * the `refresh` instances again: T3 keeps what it last found of an
 * instance's sign-in, so one signed in since then has to be checked again.
 * With a scope, only models the named agent may use are offered.
 */
export async function discover(
  endpoint: T3Endpoint,
  connect: Connect = connectT3,
  refresh: readonly string[] = [],
  scope?: { readonly installation: Installation; readonly agentId: string },
): Promise<Offer> {
  const agent = scope === undefined ? undefined : agentOf(scope.installation, scope.agentId);
  const profiles = new Set(
    scope === undefined ? [] : ((await readExisting(scope.installation))?.profiles ?? []),
  );
  const connection = await connect(endpoint);
  try {
    for (const instanceId of refresh) await connection.call(RPC.refreshProviders, { instanceId });
    const { available } = catalog(await connection.call(RPC.getConfig, {}));
    return {
      projects: await projects(endpoint),
      models:
        agent === undefined
          ? available
          : available.filter(({ provider }) =>
              offers(profiles, agent.resources !== undefined, provider),
            ),
    };
  } finally {
    connection.close();
  }
}

/** T3's projects, from its authenticated shell snapshot. */
async function projects(endpoint: T3Endpoint): Promise<Project[]> {
  let response: Response;
  try {
    response = await fetch(new URL("/api/orchestration/shell", endpoint.url), {
      headers: {
        authorization: `Bearer ${endpoint.token}`,
        "x-t3-orchestration-protocol": String(ORCHESTRATION_PROTOCOL),
      },
      redirect: "manual",
      signal: AbortSignal.timeout(DEFAULT_TIMEOUT_MS),
    });
  } catch (failure) {
    throw new T3Failure(
      "unreachable",
      `T3 did not list its projects: ${failure instanceof Error ? failure.message : String(failure)}`,
    );
  }
  if (!response.ok)
    throw new T3Failure(
      response.status === 401 || response.status === 403 ? "refused" : "rejected",
      `T3 answered the project list with HTTP ${String(response.status)}`,
    );
  const shell: unknown = await response.json().catch(() => undefined);
  if (!Check(ShellSchema, shell))
    throw new T3Failure(
      "malformed",
      "T3 listed its projects in a form this runtime does not understand",
    );
  return shell.projects.map(({ id, title, workspaceRoot }) => ({
    id,
    title,
    workspace: workspaceRoot,
  }));
}

/** The agent of this runtime the installation names `id`. */
function agentOf(installation: Installation, id: string): Agent {
  const agent = installation.agents.find((candidate) => candidate.id === id);
  if (agent === undefined)
    throw new Error(
      `${id} is not an agent of ${installation.runtime.id} in carillon.toml; add it there first`,
    );
  return agent;
}

/**
 * Writes the runtime's settings for `choice` on the server at `endpoint`,
 * naming the token's file. The server must offer the project, rooted at the
 * agent's workspace, and the model, among the instances of the agent's kind:
 * the profiles for the assistant, the agent with resources, the others for
 * plain T3 Code. The settings of the other agents this runtime serves and the
 * profiles are kept, as are the title and the agent's model options while its
 * model stays the same; settings that do not read as valid are refused before
 * anything is written.
 *
 * The new settings are checked as the runtime reads them first. A token
 * other than the one they name goes to a new private file, and the settings
 * naming it then replace the file in place at once: until then the previous
 * settings and the token they name stay as they were. Returns the project.
 */
export async function configure(
  installation: Installation,
  endpoint: T3Endpoint,
  choice: Choice,
  connect: Connect = connectT3,
): Promise<Project> {
  const existing = await readExisting(installation);
  const agent = agentOf(installation, choice.agentId);
  const offer = await discover(endpoint, connect, [], { installation, agentId: choice.agentId });
  const project = offer.projects.find(({ id }) => id === choice.projectId);
  if (project === undefined) throw new Error(`T3 has no project ${choice.projectId}`);
  const rooted = await realpath(project.workspace).catch(() => undefined);
  if (rooted !== (await realpath(agent.workspace)))
    throw new Error(
      `T3 project ${project.title} works in ${project.workspace}, but ${agent.name} works in ` +
        `${agent.workspace}; choose a project rooted there`,
    );
  const assistant = agent.resources !== undefined;
  if (
    !offer.models.some(
      ({ provider, model }) => provider === choice.instanceId && model === choice.model,
    )
  )
    throw new Error(
      `T3 offers ${agent.name} no usable model ${choice.instanceId}/${choice.model}` +
        (assistant ? ", among Carillon's profiles" : ""),
    );
  const before = existing?.agents?.[choice.agentId];
  return publishSettings(installation, endpoint, existing, {
    ...existing?.agents,
    [choice.agentId]: {
      project_id: project.id,
      model: {
        instance_id: choice.instanceId,
        model: choice.model,
        // Options are the model's own; another model starts from its defaults.
        ...(before?.model.instance_id === choice.instanceId &&
        before.model.model === choice.model &&
        before.model.options !== undefined
          ? { options: before.model.options }
          : {}),
      },
    },
  }).then(() => project);
}

/** A Carillon profile: a T3 instance of its own for one harness. */
export interface ProfileChoice {
  readonly harness: Harness;
  /** An API key the harness signs in with instead of its own login. */
  readonly key?: string;
  /** A compatible endpoint the key belongs to, instead of the harness's own. */
  readonly endpoint?: string;
}

/**
 * Makes Carillon's profile for a harness on the server at `endpoint`: the T3
 * instance `carillon_HARNESS`, whose home holds the harness's own state,
 * with links to the assistant's resources: its instructions to their
 * `AGENTS.md`, each shared skill into its own `skills` directory, so Carillon's instructions
 * and skills have one copy, its resources. T3 then gets the instance, created
 * live, and the runtime's settings name it a profile. Each step keeps what an
 * earlier run made as it should be, and refuses what is otherwise. Returns
 * the instance and home.
 */
export async function profile(
  installation: Installation,
  endpoint: T3Endpoint,
  choice: ProfileChoice,
  connect: Connect = connectT3,
): Promise<{ readonly instanceId: string; readonly home: string }> {
  const existing = await readExisting(installation);
  const harness = HARNESSES[choice.harness];
  const resources = assistantResources(installation);
  if (resources === undefined)
    throw new Error(
      `no agent of ${installation.runtime.id} in carillon.toml has resources for a profile to share`,
    );
  const instanceId = `carillon_${choice.harness}`;
  const home = profileHome(choice.harness);
  await reconcileProfile(choice.harness, resources);
  const connection = await connect(endpoint);
  try {
    const settings: unknown = await connection.call(RPC.getSettings, {});
    if (!Check(ServerSettingsSchema, settings))
      throw new T3Failure(
        "malformed",
        "T3 described its settings in a form this runtime does not understand",
      );
    const instance = settings.providerInstances?.[instanceId];
    if (instance === undefined)
      await connection.call(
        RPC.updateSettings,
        instanceCreate(instanceId, {
          driver: harness.driver,
          displayName: harness.name,
          homePath: home,
          environment: environmentOf(harness, choice),
        }),
      );
    else if (
      instance.driver !== harness.driver ||
      resolve(instance.config?.homePath ?? "") !== home
    )
      throw new Error(
        `T3 already has an instance ${instanceId} that is not this profile; remove or rename it in T3, then set the profile up again`,
      );
    else if (choice.key !== undefined || choice.endpoint !== undefined)
      throw new Error(
        `T3 already has profile ${instanceId}; change its environment in T3 settings. The supplied key or endpoint was not applied`,
      );
  } finally {
    connection.close();
  }
  const profiles = [...new Set([...(existing?.profiles ?? []), instanceId])];
  await publishSettings(installation, endpoint, existing, existing?.agents, profiles);
  return { instanceId, home };
}

/** The profile's sign-in, as the harness's environment: none for its own login. */
function environmentOf(
  harness: (typeof HARNESSES)[Harness],
  choice: ProfileChoice,
): InstanceDefinition["environment"] {
  if (choice.key === undefined) return [];
  if (choice.endpoint === undefined)
    return [{ name: harness.key, value: choice.key, sensitive: true }];
  return [
    { name: harness.endpoint, value: choice.endpoint, sensitive: false },
    { name: harness.endpointKey, value: choice.key, sensitive: true },
  ];
}

async function readExisting(installation: Installation): Promise<SettingsFile | undefined> {
  return readSettings(installation.providerFile).catch((failure: unknown) => {
    throw new Error(
      `${reason(failure)}; nothing was changed: correct or remove it, then configure again`,
      { cause: failure },
    );
  });
}

/**
 * Publishes the settings with `agents` and `profiles` for `endpoint`, the
 * rest as they were, naming the token's file as described at
 * {@link configure}. Only the agents `carillon.toml` gives this runtime keep
 * their settings: the runtime refuses to start with any other's.
 */
async function publishSettings(
  installation: Installation,
  endpoint: T3Endpoint,
  existing: SettingsFile | undefined,
  agents: SettingsFile["agents"],
  profiles: SettingsFile["profiles"] = existing?.profiles,
): Promise<void> {
  // Another token gets a file of its own, so the file the settings in place
  // name is never rewritten. One that cannot be read is not the same token.
  const named =
    existing === undefined
      ? undefined
      : await readFile(resolve(installation.root, existing.token_file), "utf8").then(
          (text) => text.trim(),
          () => undefined,
        );
  const tokenFile =
    existing !== undefined && named === endpoint.token
      ? existing.token_file
      : `credentials/t3.${crypto.randomUUID()}.token`;
  const unchanged = "the T3 settings and the token they name were not changed";
  const serves = new Set(installation.agents.map(({ id }) => id));
  const served = Object.entries(agents ?? {}).filter(([id]) => serves.has(id));
  const settings = stringify({
    url: endpoint.url,
    token_file: tokenFile,
    ...(existing?.title === undefined ? {} : { title: existing.title }),
    ...(profiles === undefined || profiles.length === 0 ? {} : { profiles }),
    ...(served.length === 0 ? {} : { agents: Object.fromEntries(served) }),
  });
  try {
    parseSettings(installation.providerFile, settings);
  } catch (failure) {
    throw new Error(`${unchanged}: ${reason(failure)}`, { cause: failure });
  }
  if (tokenFile !== existing?.token_file)
    await publish(resolve(installation.root, tokenFile), `${endpoint.token}\n`).catch(
      (failure: unknown) => {
        throw new Error(`${unchanged}: ${reason(failure)}`, { cause: failure });
      },
    );
  await publish(installation.providerFile, settings).catch((failure: unknown) => {
    throw failure instanceof Unsynced
      ? failure
      : new Error(`${unchanged}: ${reason(failure)}`, { cause: failure });
  });
}

/** A publication in place whose directory did not sync, so a crash may still undo it. */
class Unsynced extends Error {}

/**
 * Replaces `path` with `text`, readable only by the owner, durably. A failure
 * before the rename leaves `path` as it was; {@link Unsynced} leaves the new
 * text in place.
 */
async function publish(path: string, text: string): Promise<void> {
  const temporary = `${path}.${crypto.randomUUID()}.tmp`;
  const file = await open(temporary, "wx", 0o600);
  try {
    try {
      await file.writeFile(text);
      await file.sync();
    } finally {
      await file.close();
    }
    await rename(temporary, path);
  } catch (failure) {
    await rm(temporary, { force: true });
    throw failure;
  }
  try {
    const directory = await open(dirname(path), "r");
    try {
      await directory.sync();
    } finally {
      await directory.close();
    }
  } catch (failure) {
    throw new Unsynced(`${path}: changed, but not confirmed durable: ${reason(failure)}`);
  }
}

function reason(failure: unknown): string {
  return failure instanceof Error ? failure.message : String(failure);
}
