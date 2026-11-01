/** The runtime's own settings, `providers/<id>.toml`. Checked strictly: an unknown or mistyped setting refuses startup. */
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { Type, type Static } from "typebox";
import { mismatch } from "@carillon/runtime/mismatch";
import { Check } from "typebox/value";
import { parse as parseToml, TomlError } from "smol-toml";

/** An agent's T3 settings. Its model is the settings ledger's, never this file's. */
const AgentSettingsSchema = Type.Object(
  {
    /** The T3 project the agent works in, rooted at its workspace. */
    project_id: Type.String({ minLength: 1 }),
  },
  { additionalProperties: false },
);

const SettingsSchema = Type.Object(
  {
    /** The T3 server's HTTP origin, checked by {@link origin}. */
    url: Type.String({ minLength: 1 }),
    /** A file holding a bearer token T3 issued, relative to the installation. */
    token_file: Type.String({ minLength: 1 }),
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

interface AgentSettings {
  readonly projectId: string;
}

export interface T3Settings {
  readonly url: string;
  readonly tokenFile: string;
  readonly agents: ReadonlyMap<string, AgentSettings>;
}

export async function loadSettings(path: string, root: string): Promise<T3Settings> {
  const settings = await readSettings(path);
  if (settings === undefined) throw new Error(`${path}: the T3 runtime needs its settings file`);
  return {
    url: settings.url,
    tokenFile: resolve(root, settings.token_file),
    agents: new Map(
      Object.entries(settings.agents ?? {}).map(([id, agent]) => [
        id,
        { projectId: agent.project_id },
      ]),
    ),
  };
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
    const first = mismatch(SettingsSchema, settings);
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
