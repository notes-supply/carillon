import { registerBunOAuthFlows } from "@earendil-works/pi-ai/bun-oauth";
import { once } from "node:events";
import { readFile, stat } from "node:fs/promises";
import { join, resolve } from "node:path";
import { Type } from "typebox";
import { Check, Errors } from "typebox/value";
import { parse as parseToml } from "smol-toml";
import { createPiRuntime } from "./index.ts";
import {
  DEFAULT_INLINE_IMAGES,
  InlineImagePolicySchema,
  type InlineImagePolicy,
} from "./attachments.ts";
import { createDeterministicRuntime } from "./testing.ts";
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

async function main(values: readonly string[]): Promise<void> {
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
  const inlineImages = await loadInlineImages(config.providerFile);
  if (!(await stat(config.workspace)).isDirectory())
    throw new Error("workspace must be an existing directory");
  if (!(await stat(config.resourceRoot)).isDirectory())
    throw new Error("resource root must be an existing directory");
  const deterministic = args.has("deterministic");
  const stateDirectory = resolve(requiredArgument(args, "state-dir"));
  const sessionDirectory = join(config.resourceRoot, "sessions");
  if (!deterministic) registerBunOAuthFlows();
  // The check is the runtime's own startup check: Pi is signed in and its
  // default model is available. Nothing listens.
  if (args.has("check")) {
    const runtime = await createPiRuntime({
      workspace: config.workspace,
      stateDirectory,
      sessionDirectory,
      allowModelNetwork: true,
      inlineImages,
    });
    await runtime.close();
    process.stdout.write("Pi is signed in and has a default model.\n");
    return;
  }
  const listenAddress = providerAddress(config.runtime.url);
  const token = await loadRuntimeCredential(requiredArgument(args, "token-file"), config.runtime);
  const runtime = deterministic
    ? await createDeterministicRuntime(stateDirectory, sessionDirectory)
    : await createPiRuntime({
        workspace: config.workspace,
        stateDirectory,
        sessionDirectory,
        allowModelNetwork: true,
        inlineImages,
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
    version: packageInfo.version,
    features: ["native-continuations", "native-extensions", "skills"],
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

await main(process.argv.slice(2));
