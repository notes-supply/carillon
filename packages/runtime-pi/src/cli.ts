import { registerBunOAuthFlows } from "@earendil-works/pi-ai/bun-oauth";
import { CodemodeSandbox, loadQuickJSWasm } from "@earendil-works/pi-codemode";
import { MemoryStorage } from "@earendil-works/pi-durable";
import { createCodemodeExtension } from "@earendil-works/pi-durable/codemode";
import { createPhotonImages } from "@earendil-works/pi-durable/images";
import { createNodePhotonImages } from "@earendil-works/pi-durable/images/node";
import { createToolSearchExtension } from "@earendil-works/pi-durable/tool-search";
import { createCodingTools } from "@earendil-works/pi-durable/tools";
import { createDiscordExtension, DISCORD_SKILL } from "@carillon/discord-pi";
import type { Setting } from "@carillon/protocol";
import { once } from "node:events";
import { writeSync } from "node:fs";
import { mkdir, readFile, stat } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Type } from "typebox";
import { Check } from "typebox/value";
import { parse as parseToml } from "smol-toml";
import {
  DEFAULT_INLINE_IMAGES,
  InlineImagePolicySchema,
  type InlineImagePolicy,
} from "./attachments.ts";
import { checkResources, LOAD_RESOURCES, REPORT_FD, type Report } from "./check.ts";
import { deterministicCatalogue } from "./deterministic.ts";
import { ConversationInfo } from "./documents.ts";
import { createMcpServer, loadMcpConfig, mcpTransport } from "./mcp.ts";
import { modelSettings, openPiCatalogue, type PiCatalogue } from "./models.ts";
import { BUILT_IN_SKILLS } from "./resources.ts";
import { createPiRuntime, type PiRuntime, type PiRuntimeOptions } from "./runtime.ts";
import { createCoreClient } from "@carillon/runtime/core";
import { createRuntimeServer } from "@carillon/runtime/http";
import { mismatch } from "@carillon/runtime/mismatch";
import { SettingsLedger } from "@carillon/runtime/settings";
import packageInfo from "../package.json" with { type: "json" };
import { providerAddress } from "@carillon/runtime/endpoint";
import {
  loadInstallation,
  loadRuntimeCredential,
  type Installation,
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
  const [command, ...rest] = values;
  if (command === "check-resources") {
    // A compiled executable is its own script; otherwise the runtime runs this file.
    const script = embedded === undefined ? process.argv.slice(1, 2) : [];
    process.exitCode = await checkResources([...process.execArgv, ...script], rest);
    return;
  }
  if (command === LOAD_RESOURCES) {
    await loadResources(rest, embedded);
    return;
  }
  const args = parseArguments(values, ["deterministic", "check", "models", "configure"]);
  const providerId = requiredArgument(args, "provider");
  const config = await loadInstallation(requiredArgument(args, "config"), providerId);
  const setup = args.has("models") || args.has("configure");
  if ((args.has("check") || setup) && typeof config === "string")
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
  if (args.has("check")) {
    try {
      const offered = await (await openCatalogue()).models.getAvailable();
      if (offered.length === 0) throw new Error("Pi offers no model; sign in with /login");
    } catch (failure) {
      process.stderr.write(
        `Pi is not ready: ${failure instanceof Error ? failure.message : String(failure)}\n`,
      );
      process.exitCode = 1;
      return;
    }
    process.stdout.write("Pi is signed in and offers a model.\n");
    return;
  }
  if (setup) {
    await agentModel(args, config, await openCatalogue());
    return;
  }
  const listenAddress = providerAddress(config.runtime.url);
  const token = await loadRuntimeCredential(requiredArgument(args, "token-file"), config.runtime);
  const { runtime, close } = await openRuntime(config, stateDirectory, embedded, {
    agents: config.agents,
    catalogue: await openCatalogue(),
    core: createCoreClient({ url: config.coreUrl, providerId: config.runtime.id, token }),
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
    await close();
    server.closeAllConnections();
    await stopped;
  }
}

/** The runtime as it serves `parts.agents`, and what closes it with its MCP servers and sandbox. */
async function openRuntime(
  config: Installation,
  stateDirectory: string,
  embedded: Embedded | undefined,
  parts: Pick<PiRuntimeOptions, "agents" | "catalogue" | "core" | "inlineImages" | "storage">,
): Promise<{ readonly runtime: PiRuntime; readonly close: () => Promise<void> }> {
  const mcp = [...(await loadMcpConfig(join(stateDirectory, "mcp.json")))].map(([name, server]) =>
    createMcpServer(name, () => mcpTransport(server)),
  );
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
        agent_id: conversation?.agent_id,
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
    ...parts,
    componentDirectory: config.componentDirectory,
    skillsDirectory:
      embedded === undefined
        ? fileURLToPath(BUILT_IN_SKILLS)
        : join(dirname(process.execPath), "..", "lib", "pi-skills"),
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
  });
  return {
    runtime,
    close: async () => {
      await runtime.close();
      await Promise.allSettled(mcp.map((server) => server.close()));
      await sandbox.close();
    },
  };
}

/**
 * The child `check-resources` starts: opens the agent as the runtime does,
 * on storage of its own and a model that answers nothing, and reports on
 * file descriptor 3. It ends the process, so nothing a module started keeps
 * it running.
 */
async function loadResources(values: readonly string[], embedded?: Embedded): Promise<never> {
  const report = (line: Report) => {
    writeSync(REPORT_FD, `${JSON.stringify(line)}\n`);
  };
  report(
    await checked(values, embedded, report).catch((failure: unknown): Report => ({
      kind: "done",
      ok: false,
      text: `The check could not run: ${failure instanceof Error ? failure.message : String(failure)}`,
    })),
  );
  process.exit(0);
}

async function checked(
  values: readonly string[],
  embedded: Embedded | undefined,
  report: (line: Report) => void,
): Promise<Report> {
  const args = parseArguments(values);
  const providerId = args.get("provider") ?? "runtime-pi";
  const home = process.env["CARILLON_HOME"];
  const path = args.get("config") ?? (home === undefined ? undefined : join(home, "carillon.toml"));
  if (path === undefined) throw new Error("--config is required outside `carillon runtime-pi`");
  const config = await loadInstallation(path, providerId);
  if (typeof config === "string")
    throw new Error(`runtime provider ${providerId} is ${notServing(config)}`);
  const agentId =
    args.get("agent") ??
    process.env["CARILLON_AGENT"] ??
    (config.agents.length === 1 ? config.agents[0]?.id : undefined);
  const agent = config.agents.find(({ id }) => id === agentId);
  if (agent === undefined)
    throw new Error(
      agentId === undefined
        ? `name the agent with --agent: ${providerId} serves ${config.agents.map(({ id }) => id).join(", ")}`
        : `agent ${agentId} does not run on ${providerId}`,
    );
  if (agent.resources === undefined)
    return {
      kind: "done",
      ok: true,
      text: `${agent.id} has no resources of its own, so it runs on Carillon's defaults.`,
    };
  report({ kind: "loading", folder: agent.resources });
  const { runtime } = await openRuntime(
    config,
    resolve(args.get("state-dir") ?? join(config.root, "pi")),
    embedded,
    {
      agents: [agent],
      catalogue: deterministicCatalogue(),
      core: createCoreClient({ url: config.coreUrl, providerId, token: "" }),
      inlineImages: DEFAULT_INLINE_IMAGES,
      storage: () => Promise.resolve(new MemoryStorage()),
    },
  );
  const problem = await runtime.check(agent.id);
  return problem === undefined
    ? { kind: "done", ok: true, text: `${agent.resources} loads.` }
    : { kind: "done", ok: false, text: `${agent.resources} does not load:\n\n${problem}` };
}

/**
 * `--models` prints the models Pi offers and the agent's model, and
 * `--configure` sets the agent's model, as setup reads and writes them.
 */
async function agentModel(
  args: ReadonlyMap<string, string>,
  config: Installation,
  catalogue: PiCatalogue,
): Promise<void> {
  const agentId = requiredArgument(args, "agent");
  if (!config.agents.some(({ id }) => id === agentId))
    throw new Error(`agent ${agentId} does not run on ${config.runtime.id}`);
  await mkdir(config.componentDirectory, { recursive: true, mode: 0o700 });
  const models = modelSettings(
    catalogue,
    await SettingsLedger.open(join(config.componentDirectory, "settings.json")),
  );
  const subject = { kind: "agent", agent_id: agentId } as const;
  const { settings } = await models.settings({ subject });
  if (args.has("configure")) {
    const id = requiredArgument(args, "model");
    const value = { kind: "one", id } as const;
    const change = { setting: "model", value, expected: modelLine(settings).revision };
    const changed = modelLine((await models.settings({ subject, change })).settings);
    const name = changed.choices.find((choice) => choice.id === id)?.name ?? id;
    process.stdout.write(`${agentId} runs ${name} (${id}).\n`);
    return;
  }
  const offered = (await models.catalogue()).models.map(({ provider, model, name }) => ({
    provider,
    model,
    name,
  }));
  const { value } = modelLine(settings);
  process.stdout.write(
    `${JSON.stringify({ models: offered, value: value?.kind === "one" ? value.id : null })}\n`,
  );
}

function modelLine(settings: readonly Setting[]): Setting {
  const line = settings.find(({ id }) => id === "model");
  if (line === undefined) throw new Error("the runtime shows no model setting");
  return line;
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
    const first = mismatch(ProviderFileSchema, settings);
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
