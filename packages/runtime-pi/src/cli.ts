import { registerBunOAuthFlows } from "@earendil-works/pi-ai/bun-oauth";
import { CodemodeSandbox, loadQuickJSWasm } from "@earendil-works/pi-codemode";
import { createCodemodeExtension } from "@earendil-works/pi-durable/codemode";
import { createPhotonImages } from "@earendil-works/pi-durable/images";
import { createNodePhotonImages } from "@earendil-works/pi-durable/images/node";
import { createToolSearchExtension } from "@earendil-works/pi-durable/tool-search";
import { createCodingTools } from "@earendil-works/pi-durable/tools";
import { createDiscordExtension, DISCORD_SKILL } from "@carillon/discord-pi";
import { once } from "node:events";
import { readFile, stat } from "node:fs/promises";
import { join, resolve } from "node:path";
import { Type } from "typebox";
import { Check, Errors } from "typebox/value";
import { parse as parseToml } from "smol-toml";
import {
  DEFAULT_INLINE_IMAGES,
  InlineImagePolicySchema,
  type InlineImagePolicy,
} from "./attachments.ts";
import { deterministicCatalogue } from "./deterministic.ts";
import { ConversationInfo } from "./documents.ts";
import { createMcpServer, loadMcpConfig, mcpTransport } from "./mcp.ts";
import { checkDefault, openPiCatalogue } from "./models.ts";
import { createPiRuntime } from "./runtime.ts";
import { createCoreClient } from "@carillon/runtime/core";
import { createRuntimeServer } from "@carillon/runtime/http";
import packageInfo from "../package.json" with { type: "json" };
import { providerAddress } from "@carillon/runtime/endpoint";
import {
  loadInstallation,
  loadRuntimeCredential,
  notServing,
  parseArguments,
  requiredArgument,
} from "@carillon/runtime/installation";

/** The runtime's own settings, `providers/<id>.toml`; every setting is optional. */
const ProviderFileSchema = Type.Object(
  {
    inline_images: Type.Optional(
      Type.Partial(InlineImagePolicySchema, { additionalProperties: false }),
    ),
  },
  { additionalProperties: false },
);

/**
 * Files a compiled executable embeds, which it cannot find in installed
 * packages: Photon's and QuickJS's WebAssembly, code mode's worker entry,
 * and the Discord skill's text.
 */
interface Embedded {
  readonly photon: string;
  readonly quickjs: string;
  readonly codemodeWorker: string;
  readonly discordSkill: string;
}

export async function main(values: readonly string[], embedded?: Embedded): Promise<void> {
  const args = parseArguments(values, ["deterministic", "check"]);
  const providerId = requiredArgument(args, "provider");
  const config = await loadInstallation(requiredArgument(args, "config"), providerId);
  if (args.has("check") && typeof config === "string")
    throw new Error(`runtime provider ${providerId} is ${notServing(config)}`);
  // Disabled on purpose, or not the runtime this installation chose, it ends
  // successfully, so its service is not restarted, before it reads its
  // credential or creates any state.
  if (typeof config === "string") {
    process.stdout.write(`runtime provider ${providerId} is ${notServing(config)}; not serving\n`);
    return;
  }
  if (config.agents.length === 0) {
    process.stdout.write(`runtime provider ${providerId} serves no agent; not serving\n`);
    return;
  }
  const inlineImages = await loadInlineImages(config.providerFile);
  for (const agent of config.agents) {
    const workspace = await stat(agent.workspace).catch(() => undefined);
    if (workspace?.isDirectory() !== true)
      throw new Error(`agent ${agent.id}: workspace must be an existing directory`);
    if (agent.resources !== undefined) {
      const resources = await stat(agent.resources).catch(() => undefined);
      if (resources?.isDirectory() !== true)
        throw new Error(`agent ${agent.id}: resources must be an existing directory`);
    }
  }
  const deterministic = args.has("deterministic");
  const stateDirectory = resolve(requiredArgument(args, "state-dir"));
  if (!deterministic) registerBunOAuthFlows();
  const openCatalogue = () =>
    deterministic ? Promise.resolve(deterministicCatalogue()) : openPiCatalogue(stateDirectory);
  // The check is the runtime's own startup check: Pi is signed in and its
  // default model is available. Nothing listens.
  if (args.has("check")) {
    try {
      await checkDefault(await openCatalogue());
    } catch (failure) {
      process.stderr.write(
        `Pi is not ready: ${failure instanceof Error ? failure.message : String(failure)}\n`,
      );
      process.exitCode = 1;
      return;
    }
    process.stdout.write("Pi is signed in and has a default model.\n");
    return;
  }
  const listenAddress = providerAddress(config.runtime.url);
  const token = await loadRuntimeCredential(requiredArgument(args, "token-file"), config.runtime);
  const mcp = [...(await loadMcpConfig(join(stateDirectory, "mcp.json")))].map(([name, server]) =>
    createMcpServer(name, () => mcpTransport(server)),
  );
  const catalogue = await openCatalogue();
  const images =
    embedded === undefined
      ? await createNodePhotonImages()
      : createPhotonImages(await readFile(embedded.photon));
  const sandbox = new CodemodeSandbox(
    embedded === undefined
      ? {}
      : { wasm: loadQuickJSWasm(embedded.quickjs), workerUrl: embedded.codemodeWorker },
  );
  const discord = createDiscordExtension({
    guidance: embedded?.discordSkill ?? (await readFile(DISCORD_SKILL, "utf8")),
    // The conversation's own interface, with the operator credential's file:
    // host data, never the model's.
    async context(api, context) {
      const conversation = await api.snapshot(ConversationInfo, api.conversationId, context);
      const provider = config.interfaces.find((item) => item.id === conversation?.interface_id);
      return {
        interface_id: conversation?.interface_id,
        native: conversation?.native,
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
  const runtime = await createPiRuntime({
    agents: config.agents,
    componentDirectory: config.componentDirectory,
    catalogue,
    core: createCoreClient({ url: config.coreUrl, providerId: config.runtime.id, token }),
    images,
    builtIns: [
      { ...createCodingTools({ images }), name: "coding" },
      createCodemodeExtension({ sandbox }),
      createToolSearchExtension(),
      discord,
    ],
    // Carillon's interface providers are its Discord providers.
    defaults: ["coding", "tool-search", ...(config.interfaces.length > 0 ? ["discord"] : [])],
    mcp,
    inlineImages,
  });
  const server = createRuntimeServer({
    runtime,
    token,
    providerId: config.runtime.id,
    provider: deterministic ? "deterministic-local-substitute" : "pi",
    version: packageInfo.version,
    features: ["native-extensions", "skills"],
    agents: config.agents,
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
    await Promise.allSettled(mcp.map((server) => server.close()));
    await sandbox.close();
    server.closeAllConnections();
    await stopped;
  }
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

if (import.meta.main) await main(process.argv.slice(2));
