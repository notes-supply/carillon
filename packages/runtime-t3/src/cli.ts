import { once } from "node:events";
import { mkdir, readFile, stat } from "node:fs/promises";
import { createCoreClient } from "@carillon/runtime/core";
import { providerAddress } from "@carillon/runtime/endpoint";
import { createRuntimeServer } from "@carillon/runtime/http";
import {
  loadInstallation,
  loadRuntimeCredential,
  notServing,
  parseArguments,
  requiredArgument,
  type Installation,
} from "@carillon/runtime/installation";
import { ModelSelections } from "@carillon/runtime/selections";
import { readStops } from "./held.ts";
import { createT3Runtime, type T3Agent } from "./runtime.ts";
import packageInfo from "../package.json" with { type: "json" };
import { join } from "node:path";
import { text } from "node:stream/consumers";
import { configure, discover, plain } from "./setup.ts";
import { loadSettings, origin, type T3Settings } from "./settings.ts";
import { connectT3, T3Failure, type T3Endpoint } from "./t3.ts";

async function serve(values: readonly string[]): Promise<void> {
  const args = parseArguments(values);
  const providerId = requiredArgument(args, "provider");
  const installation = await loadInstallation(requiredArgument(args, "config"), providerId);
  // Disabled on purpose, or not the runtime this installation chose, it ends
  // successfully, so its service is not restarted, before it reads any
  // credential, settings or state, or reaches T3.
  if (typeof installation === "string") {
    process.stdout.write(
      `runtime provider ${providerId} is ${notServing(installation)}; not serving\n`,
    );
    return;
  }
  if (installation.agents.length === 0) {
    process.stdout.write(`runtime provider ${providerId} serves no agent; not serving\n`);
    return;
  }
  const listenAddress = providerAddress(installation.runtime.url);
  const token = await loadRuntimeCredential(
    requiredArgument(args, "token-file"),
    installation.runtime,
  );
  const { settings, endpoint, agents } = await saved(installation);
  // Fail closed before listening: the server must speak the verified
  // protocol and accept the credential, or this runtime does not start.
  (await connectT3(endpoint)).close();
  await mkdir(installation.componentDirectory, { recursive: true, mode: 0o700 });
  const runtime = createT3Runtime({
    endpoint,
    agents,
    selections: await ModelSelections.open(
      join(installation.componentDirectory, "model-selection.json"),
    ),
    stops: await readStops(join(installation.componentDirectory, "stops.json")),
    core: createCoreClient({
      url: installation.coreUrl,
      providerId: installation.runtime.id,
      token,
    }),
  });
  const server = createRuntimeServer({
    runtime,
    token,
    providerId: installation.runtime.id,
    provider: "t3",
    version: packageInfo.version,
    features: ["native-continuations"],
    agents: installation.agents,
  });
  try {
    server.listen(listenAddress);
    await once(server, "listening");
    process.stdout.write(
      `runtime-t3 ready at ${installation.runtime.url}, executing on ${settings.url} for ${[...agents.keys()].join(", ") || "no agent"}\n`,
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

/**
 * The saved settings, the T3 endpoint they name and the agents as the
 * runtime runs them, as startup reads them before reaching T3. Every agent
 * of this runtime in `carillon.toml` is plain T3 Code, without resources,
 * works in an existing directory and has settings here, which name no other
 * agent.
 */
async function saved(installation: Installation): Promise<{
  settings: T3Settings;
  endpoint: T3Endpoint;
  agents: ReadonlyMap<string, T3Agent>;
}> {
  const settings = await loadSettings(installation.providerFile, installation.root);
  const agents = new Map<string, T3Agent>();
  for (const agent of installation.agents.map(plain)) {
    const workspace = await stat(agent.workspace).then(
      (entry) => entry.isDirectory(),
      () => false,
    );
    if (!workspace)
      throw new Error(
        `${agent.workspace}: ${agent.name}'s workspace must be an existing directory`,
      );
    const configured = settings.agents.get(agent.id);
    if (configured === undefined)
      throw new Error(
        `${installation.providerFile}: ${agent.id} has no T3 settings; set its project and model with \`carillon runtime-t3 configure --agent ${agent.id}\``,
      );
    agents.set(agent.id, { ...configured, workspace: agent.workspace });
  }
  for (const id of settings.agents.keys())
    if (!agents.has(id))
      throw new Error(
        `${installation.providerFile}: /agents/${id} is not an agent of ${installation.runtime.id} in carillon.toml`,
      );
  const token = (await readFile(settings.tokenFile, "utf8")).trim();
  if (token.length === 0) throw new Error(`${settings.tokenFile}: the T3 token file is empty`);
  return { settings, endpoint: { url: settings.url, token }, agents };
}

/** The T3 token, given on standard input rather than as an argument: never a terminal's. */
async function token(): Promise<string> {
  if (process.stdin.isTTY)
    throw new Error("give the T3 token on standard input, for example `< t3.token`");
  const value = (await text(process.stdin)).trim();
  if (value.length === 0) throw new Error("the T3 token on standard input is empty");
  return value;
}

function originArgument(args: ReadonlyMap<string, string>): string {
  const url = origin(requiredArgument(args, "url"));
  if (url === undefined)
    throw new Error(
      "--url must be T3's HTTP or HTTPS origin, without credentials, a path, a query or a fragment",
    );
  return url;
}

/**
 * `discover --url URL [--agent ID --config PATH]`: the server's projects and
 * models, as JSON. `--agent` offers them to that agent of the installation
 * `--config` names, or `CARILLON_HOME`'s.
 */
async function discoverCommand(values: readonly string[]): Promise<void> {
  const args = parseArguments(values);
  const agentId = args.get("agent");
  const scope =
    agentId === undefined
      ? undefined
      : { installation: (await serving(args)).installation, agentId };
  const offer = await discover(
    { url: originArgument(args), token: await token() },
    scope === undefined ? {} : { scope },
  );
  process.stdout.write(`${JSON.stringify(offer)}\n`);
}

/**
 * `configure --url URL --agent ID --project ID --model INSTANCE/MODEL`: an
 * agent's settings, for the installation `--config` names, or
 * `CARILLON_HOME`'s.
 */
async function configureCommand(values: readonly string[]): Promise<void> {
  const args = parseArguments(values);
  const url = originArgument(args);
  const [instanceId = "", ...slug] = requiredArgument(args, "model").split("/");
  const model = slug.join("/");
  if (instanceId.length === 0 || model.length === 0)
    throw new Error("--model must be INSTANCE/MODEL, as T3 names them");
  const { providerId, installation } = await serving(args);
  const agentId = requiredArgument(args, "agent");
  const project = await configure(
    installation,
    { url, token: await token() },
    {
      agentId,
      projectId: requiredArgument(args, "project"),
      instanceId,
      model,
    },
  );
  process.stdout.write(
    `Saved ${providerId}'s settings for ${agentId}: project ${project.title} (${project.workspace}) on ${url}, with ${instanceId}/${model}. It reads them when it starts, so a running Carillon needs \`carillon restart\`.\n`,
  );
}

/** The runtime `--provider` names, as the installation registers it and serving. */
async function serving(
  args: ReadonlyMap<string, string>,
): Promise<{ providerId: string; installation: Installation }> {
  const { providerId, installation } = await registered(args);
  if (installation === "unregistered")
    throw new Error(
      `${providerId} is not in carillon.toml; set it up with \`carillon setup --runtime t3\``,
    );
  if (installation === "disabled")
    throw new Error(`${providerId} is disabled in carillon.toml; enable it first`);
  return { providerId, installation };
}

/**
 * `check`: whether the saved settings and T3 token read as startup reads
 * them, for the installation `--config` names, or `CARILLON_HOME`'s. It
 * reaches no T3 server. A runtime that would not serve, disabled or not
 * registered, starts from nothing, so it passes.
 */
async function checkCommand(values: readonly string[]): Promise<void> {
  const { providerId, installation } = await registered(parseArguments(values));
  if (typeof installation === "string") {
    process.stdout.write(`${providerId} is ${notServing(installation)}; nothing to check\n`);
    return;
  }
  if (installation.agents.length === 0) {
    process.stdout.write(`${providerId} serves no agent; nothing to check\n`);
    return;
  }
  await saved(installation);
  process.stdout.write(`${providerId}'s saved settings and T3 token read as startup reads them.\n`);
}

/** The runtime `--provider` names, `runtime-t3` by default, as the installation `--config` names registers it. */
async function registered(
  args: ReadonlyMap<string, string>,
): Promise<{ providerId: string; installation: Installation | "disabled" | "unregistered" }> {
  const providerId = args.get("provider") ?? "runtime-t3";
  const home = process.env["CARILLON_HOME"];
  const config =
    args.get("config") ?? (home === undefined ? undefined : join(home, "carillon.toml"));
  if (config === undefined) throw new Error("--config is required outside `carillon runtime-t3`");
  return { providerId, installation: await loadInstallation(config, providerId) };
}

/** A setup failure as one line, saying which side refused. */
function explained(failure: unknown): string {
  if (failure instanceof T3Failure && failure.kind === "refused")
    return `T3 refused the token: ${failure.message}`;
  if (failure instanceof T3Failure && failure.detail !== undefined)
    return `${failure.message}: ${failure.detail}`;
  return failure instanceof Error ? failure.message : String(failure);
}

const commands = {
  discover: discoverCommand,
  configure: configureCommand,
  check: checkCommand,
};
const [command, ...rest] = process.argv.slice(2);
function isCommand(value: string | undefined): value is keyof typeof commands {
  return value !== undefined && Object.hasOwn(commands, value);
}
if (isCommand(command)) {
  try {
    await commands[command](rest);
  } catch (failure) {
    process.stderr.write(`error: ${explained(failure)}\n`);
    process.exitCode = 1;
  }
} else await serve(process.argv.slice(2));
