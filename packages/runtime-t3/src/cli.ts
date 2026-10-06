import { once } from "node:events";
import { readFile, stat } from "node:fs/promises";
import { providerAddress } from "@carillon/runtime/endpoint";
import { createRuntimeServer } from "@carillon/runtime/http";
import {
  loadInstallation,
  loadRuntimeCredential,
  parseArguments,
  requiredArgument,
} from "@carillon/runtime/installation";
import { createT3Runtime } from "./runtime.ts";
import packageInfo from "../package.json" with { type: "json" };
import { loadSettings } from "./settings.ts";
import { connectT3 } from "./t3.ts";

async function main(values: readonly string[]): Promise<void> {
  const args = parseArguments(values);
  const installation = await loadInstallation(
    requiredArgument(args, "config"),
    requiredArgument(args, "provider"),
  );
  if (!(await stat(installation.workspace)).isDirectory())
    throw new Error("workspace must be an existing directory");
  const listenAddress = providerAddress(installation.runtime.url);
  const token = await loadRuntimeCredential(
    requiredArgument(args, "token-file"),
    installation.runtime,
  );
  const settings = await loadSettings(installation.providerFile, installation.root);
  const t3Token = (await readFile(settings.tokenFile, "utf8")).trim();
  if (t3Token.length === 0) throw new Error(`${settings.tokenFile}: the T3 token file is empty`);
  const endpoint = { url: settings.url, token: t3Token };
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

await main(process.argv.slice(2));
