/** The runtime's own settings, `providers/<id>.toml`. Checked strictly: an unknown or mistyped setting refuses startup. */
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { Type, type Static } from "typebox";
import { Check, Errors } from "typebox/value";
import { parse as parseToml, TomlError } from "smol-toml";
import type { NativeSelection } from "./contracts.ts";
import { HARNESSES } from "./profiles.ts";

const PROFILE_IDS = new Set(Object.keys(HARNESSES).map((harness) => `carillon_${harness}`));

const SelectionSchema = Type.Object(
  {
    instance_id: Type.String({ minLength: 1 }),
    model: Type.String({ minLength: 1 }),
    /** Option values by ID: a select choice's ID, or a boolean. */
    options: Type.Optional(
      Type.Record(
        Type.String({ minLength: 1 }),
        Type.Union([Type.String({ minLength: 1 }), Type.Boolean()]),
      ),
    ),
  },
  { additionalProperties: false },
);

const AgentSettingsSchema = Type.Object(
  {
    /** The T3 project the agent works in, rooted at its workspace. */
    project_id: Type.String({ minLength: 1 }),
    /** The selection a new conversation thread starts with, as T3 names it. */
    model: SelectionSchema,
  },
  { additionalProperties: false },
);

const SettingsSchema = Type.Object(
  {
    /** The T3 server's HTTP origin, checked by {@link origin}. */
    url: Type.String({ minLength: 1 }),
    /** A file holding a bearer token T3 issued, relative to the installation. */
    token_file: Type.String({ minLength: 1 }),
    /** Prefix of the titles of the assistant's T3 threads. */
    title: Type.Optional(Type.String({ minLength: 1, maxLength: 60 })),
    /** Carillon's own T3 instances, whose homes hold its profile. */
    profiles: Type.Optional(
      Type.Array(Type.String({ pattern: `^(?:${[...PROFILE_IDS].join("|")})$` }), {
        uniqueItems: true,
      }),
    ),
    /** Each agent of this runtime, by its ID in `carillon.toml`. */
    agents: Type.Optional(
      Type.Record(Type.String({ pattern: "^[A-Za-z0-9_-]{1,128}$" }), AgentSettingsSchema, {
        additionalProperties: false,
      }),
    ),
  },
  { additionalProperties: false },
);

/** The settings file as written, its URL an origin. */
export type SettingsFile = Static<typeof SettingsSchema>;
type AgentSettingsFile = Static<typeof AgentSettingsSchema>;

interface AgentSettings {
  readonly projectId: string;
  /** The agent default from the settings: what new threads start with until `/model` changes it. */
  readonly selection: NativeSelection;
}

export interface T3Settings {
  readonly url: string;
  readonly tokenFile: string;
  readonly title: string;
  readonly profiles: ReadonlySet<string>;
  readonly agents: ReadonlyMap<string, AgentSettings>;
}

export async function loadSettings(path: string, root: string): Promise<T3Settings> {
  const settings = await readSettings(path);
  if (settings === undefined) throw new Error(`${path}: the T3 runtime needs its settings file`);
  return {
    url: settings.url,
    tokenFile: resolve(root, settings.token_file),
    title: settings.title ?? "Carillon",
    profiles: new Set(settings.profiles ?? []),
    agents: new Map(
      Object.entries(settings.agents ?? {}).map(([id, agent]) => [
        id,
        { projectId: agent.project_id, selection: nativeSelection(agent.model) },
      ]),
    ),
  };
}

function nativeSelection(selection: AgentSettingsFile["model"]): NativeSelection {
  return {
    instanceId: selection.instance_id,
    model: selection.model,
    options: Object.entries(selection.options ?? {}).map(([id, value]) => ({ id, value })),
  };
}

/**
 * Whether `instanceId` is one the agent runs on: Carillon's profiles for its
 * assistant. Plain T3 Code excludes saved profiles and reserved Carillon IDs,
 * including profiles whose setup did not finish saving their settings.
 */
export function offers(
  profiles: ReadonlySet<string>,
  assistant: boolean,
  instanceId: string,
): boolean {
  return assistant
    ? profiles.has(instanceId)
    : !profiles.has(instanceId) && !PROFILE_IDS.has(instanceId);
}

/** The settings file at `path`, checked strictly, or undefined when there is none. */
export async function readSettings(path: string): Promise<SettingsFile | undefined> {
  let text: string;
  try {
    text = await readFile(path, "utf8");
  } catch (failure) {
    if (failure instanceof Error && "code" in failure && failure.code === "ENOENT")
      return undefined;
    throw failure;
  }
  return parseSettings(path, text);
}

/** `text` as the settings file at `path`, checked strictly. */
export function parseSettings(path: string, text: string): SettingsFile {
  let settings: unknown;
  try {
    settings = parseToml(text);
  } catch (failure) {
    // Refused by position, without quoting the file, which may hold a credential.
    if (failure instanceof TomlError)
      throw new Error(
        `${path}: invalid TOML at line ${String(failure.line)}, column ${String(failure.column)}`,
        { cause: failure },
      );
    throw failure;
  }
  if (!Check(SettingsSchema, settings)) {
    const [first] = Errors(SettingsSchema, settings);
    throw new Error(
      `${path}: ${first === undefined ? "invalid settings" : `${first.instancePath || "/"} ${first.message}`}`,
    );
  }
  // Refused without repeating the value, which may hold a credential.
  const url = origin(settings.url);
  if (url === undefined)
    throw new Error(
      `${path}: /url must be an HTTP or HTTPS origin, without credentials, a path, a query or a fragment`,
    );
  return { ...settings, url };
}

/** `value` as an HTTP(S) origin, or undefined when it is not only one. */
export function origin(value: string): string | undefined {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return undefined;
  }
  if (
    (url.protocol !== "http:" && url.protocol !== "https:") ||
    url.username !== "" ||
    url.password !== "" ||
    url.pathname !== "/" ||
    url.search !== "" ||
    url.hash !== ""
  )
    return undefined;
  return url.origin;
}
