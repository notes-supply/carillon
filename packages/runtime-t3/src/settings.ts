/** The runtime's own settings, `providers/<id>.toml`. Checked strictly: an unknown or mistyped setting refuses startup. */
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { Type, type Static } from "typebox";
import { Check, Errors } from "typebox/value";
import { parse as parseToml, TomlError } from "smol-toml";
import type { NativeSelection } from "./contracts.ts";

const SettingsSchema = Type.Object(
  {
    /** The T3 server's HTTP origin, checked by {@link origin}. */
    url: Type.String({ minLength: 1 }),
    /** A file holding a bearer token T3 issued, relative to the installation. */
    token_file: Type.String({ minLength: 1 }),
    /** The T3 project whose workspace is the agent's workspace. */
    project_id: Type.String({ minLength: 1 }),
    /** Prefix of the T3 thread titles this runtime opens. */
    title: Type.Optional(Type.String({ minLength: 1, maxLength: 60 })),
    /** The native selection a new conversation thread starts with, as T3 names it. */
    model: Type.Object(
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
    ),
  },
  { additionalProperties: false },
);

/** The settings file as written, its URL an origin. */
export type SettingsFile = Static<typeof SettingsSchema>;

export interface T3Settings {
  readonly url: string;
  readonly tokenFile: string;
  readonly projectId: string;
  readonly title: string;
  readonly selection: NativeSelection;
}

export async function loadSettings(path: string, root: string): Promise<T3Settings> {
  const settings = await readSettings(path);
  if (settings === undefined) throw new Error(`${path}: the T3 runtime needs its settings file`);
  return {
    url: settings.url,
    tokenFile: resolve(root, settings.token_file),
    projectId: settings.project_id,
    title: settings.title ?? "Carillon",
    selection: {
      instanceId: settings.model.instance_id,
      model: settings.model.model,
      options: Object.entries(settings.model.options ?? {}).map(([id, value]) => ({ id, value })),
    },
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
