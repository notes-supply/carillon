/** A runtime on a scripted model, real pi-durable storage and a fake core, for tests. */
import {
  createModels,
  fauxAssistantMessage,
  fauxProvider,
  Type,
  type FauxResponseStep,
  type MutableModels,
} from "@earendil-works/pi-ai";
import {
  defineExtension,
  defineTool,
  MemoryStorage,
  type Extension,
  type Storage,
} from "@earendil-works/pi-durable";
import { openNodeSqliteStorage } from "@earendil-works/pi-durable/storage/sqlite/node";
import type { ImageProcessor } from "@earendil-works/pi-durable/tools";
import { TurnOutcomeSchema, type TurnOutcome } from "@carillon/protocol";
import { createCoreClient } from "@carillon/runtime/core";
import type { InboxTurns, RuntimeOperations, TurnRequest } from "@carillon/runtime/operations";
import { startFakeCore, type FakeCore } from "@carillon/runtime/testing";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { Parse } from "typebox/value";
import type { Catalogue } from "./models.ts";
import { createPiRuntime } from "./runtime.ts";

/** Passes images through as they are. */
const images: ImageProcessor = {
  prepare: (bytes, mimeType) =>
    Promise.resolve({ data: Buffer.from(bytes).toString("base64"), mimeType }),
};

export interface Fixture {
  readonly runtime: RuntimeOperations<InboxTurns>;
  readonly core: FakeCore;
  readonly model: ReturnType<typeof fauxProvider>;
  /** The catalogue's models, which a test may change. */
  readonly models: MutableModels;
  readonly workspace: string;
  readonly componentDirectory: string;
  /** A turn on the agent's conversation with these resources. */
  turn(id: string, input: string, overrides?: Partial<TurnRequest>): TurnRequest;
  /** A resource snapshot holding these files. */
  snapshot(files: Record<string, string>): Promise<string>;
  /** The outcome core received for the turn, once it did. */
  outcome(turnId: string): Promise<TurnOutcome>;
  /** Closes the runtime and opens it again over the same storage. */
  restart(): Promise<Fixture>;
  close(): Promise<void>;
}

interface FixtureOptions {
  /** SQLite storage in the component directory; memory otherwise. */
  readonly durable?: boolean;
  readonly builtIns?: readonly Extension[];
  readonly defaults?: readonly string[];
  readonly answers?: FauxResponseStep[];
}

export async function startFixture(options: FixtureOptions = {}): Promise<Fixture> {
  const directory = await mkdtemp(join(tmpdir(), "carillon-runtime-pi-"));
  const workspace = join(directory, "workspace");
  const componentDirectory = join(directory, "component");
  await mkdir(workspace);
  const core = await startFakeCore();
  const model = fauxProvider({ models: [{ id: "scripted", name: "Scripted" }] });
  model.setResponses(options.answers ?? []);
  const models = createModels();
  models.setProvider(model.provider);
  const catalogue: Catalogue = {
    models,
    runtimeDefault: () => ({ provider: model.provider.id, model: "scripted", options: {} }),
    defaultThinking: () => "medium",
  };
  const memory = new MemoryStorage();
  const storage = (): Promise<Storage> =>
    options.durable === true
      ? openNodeSqliteStorage(join(componentDirectory, "agents", "default.sqlite"))
      : Promise.resolve(memory);
  let snapshots = 0;

  async function open(): Promise<Fixture> {
    const runtime = await createPiRuntime({
      agents: [{ id: "default", workspace }],
      componentDirectory,
      catalogue,
      core: createCoreClient({ url: core.url, providerId: "runtime-pi", token: "test-only" }),
      images,
      builtIns: options.builtIns ?? [],
      defaults: options.defaults ?? [],
      storage,
    });
    return {
      runtime,
      core,
      model,
      models,
      workspace,
      componentDirectory,
      turn: (id, input, overrides = {}) => ({
        correlation_id: id,
        interface_id: "discord-private",
        agent_id: "default",
        audience_id: "conversation",
        input,
        resource_root: join(directory, "empty"),
        attachment_root: join(directory, "attachments"),
        ...overrides,
      }),
      async snapshot(files) {
        snapshots += 1;
        const root = join(directory, "snapshots", String(snapshots));
        await mkdir(root, { recursive: true });
        for (const [path, content] of Object.entries(files)) {
          await mkdir(dirname(join(root, path)), { recursive: true });
          await writeFile(join(root, path), content);
        }
        return root;
      },
      async outcome(turnId) {
        const path = `/v1/runtime/turns/${encodeURIComponent(turnId)}/outcome`;
        for (let attempt = 0; attempt < 400; attempt += 1) {
          const received = core.received.findLast((call) => call.path === path);
          if (received !== undefined) return Parse(TurnOutcomeSchema, received.body);
          await new Promise((resolve) => setTimeout(resolve, 10));
        }
        throw new Error(`core received no outcome of turn ${turnId}`);
      },
      async restart() {
        await runtime.close();
        return open();
      },
      async close() {
        await runtime.close();
        await core.close();
        await rm(directory, { recursive: true, force: true });
      },
    };
  }
  return open();
}

/** A scripted answer that waits for `gate`, or its request's abort, before answering `text`. */
export function heldAnswer(text: string, gate: Promise<void>): FauxResponseStep {
  return async (_context, options) => {
    await Promise.race([gate, aborted(options?.signal)]);
    return fauxAssistantMessage(text);
  };
}

/** Rejects once `signal` aborts. */
function aborted(signal: AbortSignal | undefined): Promise<never> {
  return new Promise((_resolve, reject) => {
    signal?.addEventListener("abort", () => reject(new Error("aborted")), { once: true });
  });
}

/**
 * A tool `pause` that runs until released, saying when a call started. A
 * stubborn one ignores aborts too, so ending its run waits for the release.
 */
export function pauseTool({ stubborn = false } = {}) {
  const started = Promise.withResolvers<void>();
  const released = Promise.withResolvers<void>();
  const extension = defineExtension({
    name: "pause",
    tools: [
      defineTool({
        name: "pause",
        description: "Waits until the test releases it.",
        parameters: Type.Object({}),
        async execute(_args, _api, context) {
          started.resolve();
          await Promise.race([
            released.promise,
            ...(stubborn ? [] : [aborted(context.abortSignal)]),
          ]);
          return { output: [{ type: "text", text: "resumed" }] };
        },
      }),
    ],
  });
  return { extension, started: started.promise, release: () => released.resolve() };
}
