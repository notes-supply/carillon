import { once } from "node:events";
import { readFile, stat } from "node:fs/promises";
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
import { createT3Runtime } from "./runtime.ts";
import packageInfo from "../package.json" with { type: "json" };
import { join } from "node:path";
import { text } from "node:stream/consumers";
import { configure, discover } from "./setup.ts";
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
  if (!(await stat(installation.workspace)).isDirectory())
    throw new Error("workspace must be an existing directory");
  const listenAddress = providerAddress(installation.runtime.url);
  const token = await loadRuntimeCredential(
    requiredArgument(args, "token-file"),
    installation.runtime,
  );
  const { settings, endpoint } = await saved(installation);
  // Fail closed before listening: the server must speak the verified
  // protocol and accept the credential, or this runtime does not start.
  (await connectT3(endpoint)).close();
  const runtime = createT3Runtime({
    endpoint,
    projectId: settings.projectId,
    selection: settings.selection,
    workspace: installation.workspace,
    title: settings.title,
  });
  const server = createRuntimeServer({
    runtime,
    token,
    providerId: installation.runtime.id,
    provider: "t3",
    version: packageInfo.version,
    features: ["native-continuations"],
    workspace: installation.workspace,
  });
  try {
    server.listen(listenAddress);
    await once(server, "listening");
    process.stdout.write(
      `runtime-t3 ready at ${installation.runtime.url}, executing on ${settings.url} project ${settings.projectId} as ${settings.selection.instanceId}/${settings.selection.model}\n`,
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

/** The saved settings and the T3 endpoint they name, as startup reads them before reaching T3. */
async function saved(
  installation: Installation,
): Promise<{ settings: T3Settings; endpoint: T3Endpoint }> {
  const settings = await loadSettings(installation.providerFile, installation.root);
  const token = (await readFile(settings.tokenFile, "utf8")).trim();
  if (token.length === 0) throw new Error(`${settings.tokenFile}: the T3 token file is empty`);
  return { settings, endpoint: { url: settings.url, token } };
}

/** The T3 token, given on standard input rather than as an argument. */
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

/** `discover --url URL`: the server's projects and models, as JSON. */
async function discoverCommand(values: readonly string[]): Promise<void> {
  const args = parseArguments(values);
  const offer = await discover({ url: originArgument(args), token: await token() });
  process.stdout.write(`${JSON.stringify(offer)}\n`);
}

/**
 * `configure --url URL --project ID --model INSTANCE/MODEL`: the runtime's
 * files for the installation `--config` names, or `CARILLON_HOME`'s.
 */
async function configureCommand(values: readonly string[]): Promise<void> {
  const args = parseArguments(values);
  const url = originArgument(args);
  const [instanceId = "", ...slug] = requiredArgument(args, "model").split("/");
  const model = slug.join("/");
  if (instanceId.length === 0 || model.length === 0)
    throw new Error("--model must be INSTANCE/MODEL, as T3 names them");
  const { providerId, installation } = await registered(args);
  if (installation === "unregistered")
    throw new Error(
      `${providerId} is not in carillon.toml; create the installation with \`carillon init --runtime t3\``,
    );
  if (installation === "disabled")
    throw new Error(`${providerId} is disabled in carillon.toml; enable it first`);
  const project = await configure(
    installation,
    { url, token: await token() },
    { projectId: requiredArgument(args, "project"), instanceId, model },
  );
  // The runtime reads its settings only when it starts.
  process.stdout.write(
    `Saved ${providerId}'s settings: project ${project.title} (${project.workspace}) on ${url}, with ${instanceId}/${model}. It reads them when it starts, so a running Carillon needs \`carillon restart\`.\n`,
  );
}

/**
 * `check`: whether the saved settings and T3 token read as startup reads
 * them, for the installation `--config` names, or `CARILLON_HOME`'s. It
 * reaches no T3 server and changes nothing. A runtime that would not serve,
 * disabled or not registered, starts from nothing, so it passes.
 */
async function checkCommand(values: readonly string[]): Promise<void> {
  const { providerId, installation } = await registered(parseArguments(values));
  if (typeof installation === "string") {
    process.stdout.write(`${providerId} is ${notServing(installation)}; nothing to check\n`);
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

const commands = { discover: discoverCommand, configure: configureCommand, check: checkCommand };
const [command, ...rest] = process.argv.slice(2);
if (command === "discover" || command === "configure" || command === "check") {
  try {
    await commands[command](rest);
  } catch (failure) {
    process.stderr.write(`error: ${explained(failure)}\n`);
    process.exitCode = 1;
  }
} else await serve(process.argv.slice(2));
