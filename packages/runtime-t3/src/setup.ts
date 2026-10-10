/**
 * Setting the runtime up on an existing T3 server: what the server offers,
 * and the runtime's own files for a chosen project and model. The T3 token
 * is given as a value, never as an argument, and written only to the
 * installation's credential file.
 */
import { open, realpath, rename, rm } from "node:fs/promises";
import { dirname, join } from "node:path";
import type { AvailableModel } from "@carillon/protocol";
import type { Installation } from "@carillon/runtime/installation";
import { stringify } from "smol-toml";
import { Type } from "typebox";
import { Check } from "typebox/value";
import { ORCHESTRATION_PROTOCOL, RPC } from "./contracts.ts";
import { catalog } from "./models.ts";
import { loadSettings } from "./settings.ts";
import { connectT3, DEFAULT_TIMEOUT_MS, T3Failure, type Connect, type T3Endpoint } from "./t3.ts";

/** A T3 project: the agent works in its workspace. */
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

/** A project and a model, the model as T3 names its provider instance and slug. */
export interface Choice {
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

/** The projects and models the server at `endpoint` offers. */
export async function discover(endpoint: T3Endpoint, connect: Connect = connectT3): Promise<Offer> {
  const connection = await connect(endpoint);
  try {
    const { available } = catalog(await connection.call(RPC.getConfig, {}));
    return { projects: await projects(endpoint), models: available };
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

/**
 * Writes the runtime's files for `choice` on the server at `endpoint`: the
 * token to `credentials/t3.token` and the settings to the runtime's own
 * file, then reads the settings back as the runtime does. The server must
 * offer the project, rooted at the agent's workspace, and the model.
 * Returns the project.
 */
export async function configure(
  installation: Installation,
  endpoint: T3Endpoint,
  choice: Choice,
  connect: Connect = connectT3,
): Promise<Project> {
  const offer = await discover(endpoint, connect);
  const project = offer.projects.find(({ id }) => id === choice.projectId);
  if (project === undefined) throw new Error(`T3 has no project ${choice.projectId}`);
  const rooted = await realpath(project.workspace).catch(() => undefined);
  if (rooted !== (await realpath(installation.workspace)))
    throw new Error(
      `T3 project ${project.title} works in ${project.workspace}, but Carillon's agent works in ` +
        `${installation.workspace}; choose a project rooted there`,
    );
  if (
    !offer.models.some(
      ({ provider, model }) => provider === choice.instanceId && model === choice.model,
    )
  )
    throw new Error(`T3 offers no usable model ${choice.instanceId}/${choice.model}`);
  await publish(join(installation.root, "credentials", "t3.token"), `${endpoint.token}\n`);
  await publish(
    installation.providerFile,
    stringify({
      url: endpoint.url,
      token_file: "credentials/t3.token",
      project_id: project.id,
      model: { instance_id: choice.instanceId, model: choice.model },
    }),
  );
  await loadSettings(installation.providerFile, installation.root);
  return project;
}

/** Replaces `path` with `text`, readable only by the owner, durably. */
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
  const directory = await open(dirname(path), "r");
  try {
    await directory.sync();
  } finally {
    await directory.close();
  }
}
