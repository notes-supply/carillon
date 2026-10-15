/** What a runtime service reads of the installation registry `carillon.toml`. */
import { createHash } from "node:crypto";
import { readFile, realpath } from "node:fs/promises";
import { join, resolve } from "node:path";
import { Type } from "typebox";
import { Parse } from "typebox/value";
import { parse as parseToml } from "smol-toml";
import { providerAddress } from "./endpoint.ts";

export interface RegisteredProvider {
  readonly id: string;
  readonly url: string;
  readonly token_hash: string;
}

/** An agent whose conversations start on this runtime. */
export interface Agent {
  readonly id: string;
  readonly name: string;
  readonly workspace: string;
  /** Carillon's resources for the agent; absent when it runs with the runtime's own alone. */
  readonly resources?: string;
}

export interface Installation {
  readonly root: string;
  /** The runtime's private state directory, `<data_dir>/components/<id>`. */
  readonly componentDirectory: string;
  /** The agents whose new conversations run on this runtime, in registry order. */
  readonly agents: readonly Agent[];
  readonly runtime: RegisteredProvider;
  readonly interfaces: RegisteredProvider[];
  readonly operatorTokenHash: string;
  /** The runtime's own settings file, `providers/<id>.toml`; it may not exist. */
  readonly providerFile: string;
}

const ProviderIdentitySchema = Type.String({
  minLength: 1,
  maxLength: 128,
  pattern: "^[A-Za-z0-9_-]+$",
});
const ProviderSchema = Type.Object({
  id: ProviderIdentitySchema,
  role: Type.String({ minLength: 1 }),
  version: Type.String({ minLength: 1 }),
  url: Type.String({ minLength: 1 }),
  token_hash: Type.String({ pattern: "^[a-f0-9]{64}$" }),
  enabled: Type.Boolean(),
});
const FileConfigSchema = Type.Object({
  data_dir: Type.String(),
  core: Type.Object({ operator_token_hash: Type.String({ pattern: "^[a-f0-9]{64}$" }) }),
  agents: Type.Optional(
    Type.Array(
      Type.Object({
        id: ProviderIdentitySchema,
        name: Type.String({ minLength: 1 }),
        runtime: ProviderIdentitySchema,
        workspace: Type.String({ minLength: 1 }),
        resources: Type.Optional(Type.String({ minLength: 1 })),
      }),
    ),
  ),
  providers: Type.Array(ProviderSchema),
});

/**
 * The registry as it concerns the named runtime provider; `"disabled"` when
 * the operator disabled it, or `"unregistered"` when the registry has no such
 * runtime, as for the bundled runtime an installation did not choose. Either
 * way the runtime serves nothing.
 */
export async function loadInstallation(
  path: string,
  providerId: string,
): Promise<Installation | "disabled" | "unregistered"> {
  Parse(ProviderIdentitySchema, providerId);
  const configPath = await realpath(path);
  const root = resolve(configPath, "..");
  const parsed = Parse(FileConfigSchema, parseToml(await readFile(configPath, "utf8")));
  const reserved = parsed.providers.find(
    (provider) => provider.id === "operator" || provider.id === "discord-bot",
  );
  if (reserved !== undefined)
    throw new Error(`provider ID ${reserved.id} is reserved for installation credentials`);
  const runtime = parsed.providers.find(
    (provider) => provider.id === providerId && provider.role === "runtime",
  );
  if (runtime === undefined) return "unregistered";
  if (!runtime.enabled) return "disabled";
  const interfaces = parsed.providers.filter(
    (provider) => provider.role === "interface" && provider.enabled,
  );
  // Extensions receive these endpoints with the operator credential.
  for (const provider of interfaces) providerAddress(provider.url);
  return {
    root,
    componentDirectory: join(resolve(root, parsed.data_dir), "components", providerId),
    agents: (parsed.agents ?? [])
      .filter((agent) => agent.runtime === providerId)
      .map(({ id, name, workspace, resources }) => ({
        id,
        name,
        workspace: resolve(root, workspace),
        ...(resources === undefined ? {} : { resources: resolve(root, resources) }),
      })),
    runtime,
    interfaces,
    operatorTokenHash: parsed.core.operator_token_hash,
    providerFile: join(root, "providers", `${providerId}.toml`),
  };
}

/** Why a runtime serves nothing, as its log says it. */
export function notServing(state: "disabled" | "unregistered"): string {
  return state === "disabled" ? "disabled" : "not in carillon.toml";
}

/** The runtime's installation credential, checked against the registry before anything listens. */
export async function loadRuntimeCredential(
  tokenFile: string,
  runtime: RegisteredProvider,
): Promise<string> {
  const token = (await readFile(tokenFile, "utf8")).trim();
  if (token.length === 0 || createHash("sha256").update(token).digest("hex") !== runtime.token_hash)
    throw new Error("runtime credential does not match configuration");
  return token;
}

/** `--name value` pairs; `flags` name the arguments that take no value. */
export function parseArguments(
  values: readonly string[],
  flags: readonly string[] = [],
): ReadonlyMap<string, string> {
  const result = new Map<string, string>();
  for (let index = 0; index < values.length; index += 1) {
    const value = values[index];
    if (typeof value !== "string" || !value.startsWith("--"))
      throw new Error(`unexpected argument ${value ?? ""}`);
    const name = value.slice(2);
    if (flags.includes(name)) {
      result.set(name, "true");
      continue;
    }
    const argument = values[index + 1];
    if (argument === undefined || argument.startsWith("--"))
      throw new Error(`missing value for --${name}`);
    result.set(name, argument);
    index += 1;
  }
  return result;
}

export function requiredArgument(values: ReadonlyMap<string, string>, name: string): string {
  const value = values.get(name);
  if (value === undefined) throw new Error(`--${name} is required`);
  return value;
}
