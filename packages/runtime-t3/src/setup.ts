/**
 * Setting the runtime up on an existing T3 server: what the server offers,
 * and the runtime's own files for a chosen project and model. The T3 token
 * is given as a value, never as an argument, and written only to a private
 * credential file the runtime's settings name.
 */
import { open, readFile, realpath, rename, rm } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import type { AvailableModel } from "@carillon/protocol";
import type { Installation } from "@carillon/runtime/installation";
import { stringify } from "smol-toml";
import { Type } from "typebox";
import { Check } from "typebox/value";
import { ORCHESTRATION_PROTOCOL, RPC } from "./contracts.ts";
import { catalog } from "./models.ts";
import { parseSettings, readSettings } from "./settings.ts";
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
 * Writes the runtime's settings for `choice` on the server at `endpoint`,
 * naming the token's file. The server must offer the project, rooted at the
 * agent's workspace, and the model. Settings already there keep their title,
 * and their model's options while the model stays the same; settings that do
 * not read as valid are refused before anything is written.
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
  const existing = await readSettings(installation.providerFile).catch((failure: unknown) => {
    throw new Error(
      `${reason(failure)}; nothing was changed: correct or remove it, then configure again`,
    );
  });
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
  const settings = stringify({
    url: endpoint.url,
    token_file: tokenFile,
    project_id: project.id,
    title: existing?.title,
    model: {
      instance_id: choice.instanceId,
      model: choice.model,
      // Options are the model's own; another model starts from its defaults.
      options:
        existing?.model.instance_id === choice.instanceId && existing.model.model === choice.model
          ? existing.model.options
          : undefined,
    },
  });
  try {
    parseSettings(installation.providerFile, settings);
  } catch (failure) {
    throw new Error(`${unchanged}: ${reason(failure)}`);
  }
  if (tokenFile !== existing?.token_file)
    await publish(resolve(installation.root, tokenFile), `${endpoint.token}\n`).catch(
      (failure: unknown) => {
        throw new Error(`${unchanged}: ${reason(failure)}`);
      },
    );
  await publish(installation.providerFile, settings).catch((failure: unknown) => {
    throw failure instanceof Unsynced ? failure : new Error(`${unchanged}: ${reason(failure)}`);
  });
  return project;
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
