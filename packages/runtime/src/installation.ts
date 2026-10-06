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

export interface Installation {
  readonly root: string;
  readonly resourceRoot: string;
  readonly workspace: string;
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
  agent: Type.Object({ resource_root: Type.String(), workspace: Type.String() }),
  providers: Type.Array(ProviderSchema),
});

/** The registry as it concerns the named, enabled runtime provider. */
export async function loadInstallation(path: string, providerId: string): Promise<Installation> {
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
  if (runtime === undefined) throw new Error(`runtime provider ${providerId} is not configured`);
  if (!runtime.enabled) throw new Error(`runtime provider ${providerId} is disabled`);
  const interfaces = parsed.providers.filter(
    (provider) => provider.role === "interface" && provider.enabled,
  );
  // Extensions receive these endpoints with the operator credential.
  for (const provider of interfaces) providerAddress(provider.url);
  return {
    root,
    resourceRoot: resolve(root, parsed.agent.resource_root),
    workspace: resolve(root, parsed.agent.workspace),
    runtime,
    interfaces,
    operatorTokenHash: parsed.core.operator_token_hash,
    providerFile: join(root, "providers", `${providerId}.toml`),
  };
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
