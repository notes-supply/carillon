import { registerBunOAuthFlows } from "@earendil-works/pi-ai/bun-oauth";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { readFile, realpath, stat } from "node:fs/promises";
import { join, resolve } from "node:path";
import { Type } from "typebox";
import { Check, Errors, Parse } from "typebox/value";
import { parse as parseToml } from "smol-toml";
import { createPiRuntime } from "./index.ts";
import {
  DEFAULT_INLINE_IMAGES,
  InlineImagePolicySchema,
  type InlineImagePolicy,
} from "./attachments.ts";
import { createDeterministicRuntime } from "./testing.ts";
import { createRuntimeServer } from "./http.ts";
import { providerAddress } from "./endpoint.ts";

interface RuntimeProviderConfig {
  readonly id: string;
  readonly url: string;
  readonly token_hash: string;
}

interface Config {
  readonly root: string;
  readonly resourceRoot: string;
  readonly workspace: string;
  readonly runtime: RuntimeProviderConfig;
  readonly interfaces: RuntimeProviderConfig[];
  readonly operatorTokenHash: string;
  readonly inlineImages: InlineImagePolicy;
}

/** The runtime's own settings, `providers/<id>.toml`; every setting is optional. */
const ProviderFileSchema = Type.Object(
  {
    inline_images: Type.Optional(
      Type.Partial(InlineImagePolicySchema, { additionalProperties: false }),
    ),
  },
  { additionalProperties: false },
);

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

async function main(values: readonly string[]): Promise<void> {
  const args = parseArguments(values);
  const providerId = requiredArgument(args, "provider");
  const config = await loadConfig(requiredArgument(args, "config"), providerId);
  // Disabled on purpose, the runtime ends successfully, so its service is not
  // restarted, before it reads its credential or creates any state.
  if (config === "disabled") {
    process.stdout.write(`runtime provider ${providerId} is disabled; not serving\n`);
    return;
  }
  if (!(await stat(config.workspace)).isDirectory())
    throw new Error("workspace must be an existing directory");
  if (!(await stat(config.resourceRoot)).isDirectory())
    throw new Error("resource root must be an existing directory");
  const listenAddress = providerAddress(config.runtime.url);
  const token = (await readFile(requiredArgument(args, "token-file"), "utf8")).trim();
  if (
    token.length === 0 ||
    createHash("sha256").update(token).digest("hex") !== config.runtime.token_hash
  )
    throw new Error("runtime credential does not match configuration");
  const deterministic = args.has("deterministic");
  const stateDirectory = resolve(requiredArgument(args, "state-dir"));
  const sessionDirectory = join(config.resourceRoot, "sessions");
  if (!deterministic) registerBunOAuthFlows();
  const runtime = deterministic
    ? await createDeterministicRuntime(stateDirectory, sessionDirectory)
    : await createPiRuntime({
        workspace: config.workspace,
        stateDirectory,
        sessionDirectory,
        allowModelNetwork: true,
        inlineImages: config.inlineImages,
        extensionContext: (request) => {
          const provider = config.interfaces.find((item) => item.id === request.interface_id);
          return {
            interface_id: request.interface_id,
            native: request.native,
            connection:
              provider === undefined
                ? undefined
                : {
                    url: provider.url,
                    token_file: join(config.root, "credentials", "operator.token"),
                    token_hash: config.operatorTokenHash,
                  },
          };
        },
      });
  const server = createRuntimeServer({
    runtime,
    token,
    providerId: config.runtime.id,
    provider: deterministic ? "deterministic-local-substitute" : "pi",
    workspace: config.workspace,
  });
  try {
    server.listen(listenAddress);
    await once(server, "listening");
    process.stdout.write(
      `runtime-pi ready at ${config.runtime.url}${deterministic ? " (deterministic provider substitute)" : ""}\n`,
    );
    await new Promise<void>((done) => {
      const stop = () => {
        process.off("SIGINT", stop);
        process.off("SIGTERM", stop);
        done();
      };
      process.once("SIGINT", stop);
      process.once("SIGTERM", stop);
    });
  } finally {
    const stopped = new Promise<void>((done) => server.close(() => done()));
    await runtime.close();
    server.closeAllConnections();
    await stopped;
  }
}

/** The registry as it concerns this runtime, unless it disables it. */
async function loadConfig(path: string, providerId: string): Promise<Config | "disabled"> {
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
  if (!runtime.enabled) return "disabled";
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
    inlineImages: await loadInlineImages(join(root, "providers", `${providerId}.toml`)),
  };
}

/** Inline-image limits from the provider's own file, when it sets any. */
async function loadInlineImages(path: string): Promise<InlineImagePolicy> {
  let text: string;
  try {
    text = await readFile(path, "utf8");
  } catch (failure) {
    if (failure instanceof Error && "code" in failure && failure.code === "ENOENT")
      return DEFAULT_INLINE_IMAGES;
    throw failure;
  }
  // Checked, not parsed: an unknown or mistyped setting is an error, never
  // silently dropped or converted.
  const settings: unknown = parseToml(text);
  if (!Check(ProviderFileSchema, settings)) {
    const [first] = Errors(ProviderFileSchema, settings);
    throw new Error(
      `${path}: ${first === undefined ? "invalid settings" : `${first.instancePath || "/"} ${first.message}`}`,
    );
  }
  const policy = { ...DEFAULT_INLINE_IMAGES, ...settings.inline_images };
  if (policy.max_total_mib < policy.max_image_mib)
    throw new Error(`${path}: inline_images.max_total_mib must be at least max_image_mib`);
  return policy;
}

function parseArguments(values: readonly string[]): ReadonlyMap<string, string> {
  const result = new Map<string, string>();
  for (let index = 0; index < values.length; index += 1) {
    const value = values[index];
    if (typeof value !== "string" || !value.startsWith("--"))
      throw new Error(`unexpected argument ${value ?? ""}`);
    const name = value.slice(2);
    if (name === "deterministic") {
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

function requiredArgument(values: ReadonlyMap<string, string>, name: string): string {
  const value = values.get(name);
  if (value === undefined) throw new Error(`--${name} is required`);
  return value;
}

await main(process.argv.slice(2));
