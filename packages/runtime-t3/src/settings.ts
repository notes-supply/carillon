/** The runtime's own settings, `providers/<id>.toml`. Checked strictly: an unknown or mistyped setting refuses startup. */
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { Type } from "typebox";
import { Check, Errors } from "typebox/value";
import { parse as parseToml } from "smol-toml";
import type { NativeSelection } from "./contracts.ts";

const SettingsSchema = Type.Object(
  {
    /** The T3 server's HTTP origin. */
    url: Type.String({ pattern: "^https?://[^/]+$" }),
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

export interface T3Settings {
  readonly url: string;
  readonly tokenFile: string;
  readonly projectId: string;
  readonly title: string;
  readonly selection: NativeSelection;
}

export async function loadSettings(path: string, root: string): Promise<T3Settings> {
  let text: string;
  try {
    text = await readFile(path, "utf8");
  } catch (failure) {
    if (failure instanceof Error && "code" in failure && failure.code === "ENOENT")
      throw new Error(`${path}: the T3 runtime needs its settings file`, { cause: failure });
    throw failure;
  }
  const settings: unknown = parseToml(text);
  if (!Check(SettingsSchema, settings)) {
    const [first] = Errors(SettingsSchema, settings);
    throw new Error(
      `${path}: ${first === undefined ? "invalid settings" : `${first.instancePath || "/"} ${first.message}`}`,
    );
  }
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
