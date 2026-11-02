/** A runtime on a scripted model, real pi-durable storage and a fake core, for tests. */
import {
  createModels,
  fauxAssistantMessage,
  fauxProvider,
  Type,
  type FauxResponseFactory,
  type FauxResponseStep,
  type Message,
  type MutableModels,
} from "@earendil-works/pi-ai";
import {
  defineExtension,
  defineTool,
  MemoryStorage,
  type Extension,
  type SetTimer,
  type Storage,
} from "@earendil-works/pi-durable";
import { openNodeSqliteStorage } from "@earendil-works/pi-durable/storage/sqlite/node";
import type { ImageProcessor } from "@earendil-works/pi-durable/tools";
import {
  TurnOutcomeSchema,
  type Setting,
  type Subject,
  type TurnOutcome,
} from "@carillon/protocol";
import { createCoreClient } from "@carillon/runtime/core";
import type { RuntimeOperations, TurnRequest } from "@carillon/runtime/operations";
import { startFakeCore, type FakeCore } from "@carillon/runtime/testing";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { Parse } from "typebox/value";
import type { Clock } from "./agent.ts";
import type { McpServer } from "./mcp.ts";
import type { PiCatalogue } from "./models.ts";
import { createPiRuntime } from "./runtime.ts";

/** Passes images through as they are. */
const images: ImageProcessor = {
  prepare: (bytes, mimeType) =>
    Promise.resolve({ data: Buffer.from(bytes).toString("base64"), mimeType }),
};

export interface Fixture {
  readonly runtime: RuntimeOperations;
  readonly core: FakeCore;
  readonly model: ReturnType<typeof fauxProvider>;
  /** The catalogue's models, which a test may change. */
  readonly models: MutableModels;
  readonly workspace: string;
  readonly componentDirectory: string;
  turn(id: string, input: string, overrides?: Partial<TurnRequest>): TurnRequest;
  set(subject: Subject, setting: string, choice: string | null): Promise<Setting[]>;
  /** A resource snapshot holding these files. */
  snapshot(files: Record<string, string>): Promise<string>;
  /** The outcome core received for the turn, once it did. */
  outcome(turnId: string): Promise<TurnOutcome>;
  /** Hands the agent a message in a turn of its own, and returns its answer. */
  ask(turnId: string, input: string, overrides?: Partial<TurnRequest>): Promise<string>;
  /** The turns the runtime asked core to open, in order. */
  openings(): unknown[];
  /**
   * Closes the runtime and opens it again once `meanwhile` is done, over
   * the same storage or, `fresh`, new storage in its place, and with `mcp`
   * as its MCP servers once given.
   */
  restart(changes?: {
    readonly meanwhile?: () => unknown;
    readonly fresh?: true;
    readonly mcp?: readonly McpServer[];
  }): Promise<Fixture>;
  close(): Promise<void>;
}

interface FixtureOptions {
  /** SQLite storage in the component directory; memory otherwise. */
  readonly durable?: boolean;
  /** The agents' IDs; `default` alone without them. */
  readonly agents?: readonly string[];
  readonly agentModel?: string | null;
  readonly builtIns?: readonly Extension[];
  readonly defaults?: readonly string[];
  readonly answers?: FauxResponseStep[];
  readonly clock?: Clock;
  readonly mcp?: readonly McpServer[];
}

export async function startFixture(options: FixtureOptions = {}): Promise<Fixture> {
  const directory = await mkdtemp(join(tmpdir(), "carillon-runtime-pi-"));
  const workspace = join(directory, "workspace");
  const componentDirectory = join(directory, "component");
  await mkdir(workspace);
  const core = await startFakeCore();
  const model = fauxProvider({
    models: [
      { id: "scripted", name: "Scripted" },
      { id: "reasoner", name: "Reasoner", reasoning: true },
    ],
  });
  model.setResponses(options.answers ?? []);
  const models = createModels();
  models.setProvider(model.provider);
  const catalogue: PiCatalogue = { models, defaultThinking: () => "medium" };
  const memories = new Map<string, MemoryStorage>();
  function storage(agentId: string): Promise<Storage> {
    if (options.durable === true)
      return openNodeSqliteStorage(join(componentDirectory, "agents", `${agentId}.sqlite`));
    const memory = memories.get(agentId) ?? new MemoryStorage();
    memories.set(agentId, memory);
    return Promise.resolve(memory);
  }
  let snapshots = 0;

  async function open(mcp = options.mcp): Promise<Fixture> {
    const runtime = await createPiRuntime({
      agents: (options.agents ?? ["default"]).map((id) => ({ id, workspace })),
      componentDirectory,
      catalogue,
      core: createCoreClient({ url: core.url, providerId: "runtime-pi", token: "test-only" }),
      images,
      builtIns: options.builtIns ?? [],
      defaults: options.defaults ?? [],
      storage,
      ...(options.clock === undefined ? {} : { clock: options.clock }),
      ...(mcp === undefined ? {} : { mcp }),
    });
    const fixture: Fixture = {
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
        channel_id: "300",
        input,
        resource_root: join(directory, "empty"),
        attachment_root: join(directory, "attachments"),
        ...overrides,
      }),
      async set(subject, setting, choice) {
        const { settings } = await runtime.settings({ subject });
        const line = settings.find(({ id }) => id === setting);
        if (line === undefined) throw new Error(`${subject.agent_id} shows no ${setting} line`);
        const value = choice === null ? null : { kind: "one" as const, id: choice };
        return (
          await runtime.settings({ subject, change: { setting, value, expected: line.revision } })
        ).settings;
      },
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
      async ask(turnId, input, overrides) {
        await runtime.turns.submit(fixture.turn(turnId, input, overrides));
        const outcome = await fixture.outcome(turnId);
        if (outcome.outcome !== "answered")
          throw new Error(`turn ${turnId} was ${outcome.outcome}`);
        return outcome.output;
      },
      openings: () =>
        core.received.filter((call) => call.path === "/v1/runtime/turns").map((call) => call.body),
      async restart({ meanwhile, fresh, mcp: servers = mcp } = {}) {
        await runtime.close();
        await meanwhile?.();
        if (fresh === true) {
          memories.clear();
          await rm(join(componentDirectory, "agents"), { recursive: true, force: true });
        }
        return open(servers);
      },
      async close() {
        await runtime.close();
        await core.close();
        await rm(directory, { recursive: true, force: true });
      },
    };
    return fixture;
  }
  const started = await open();
  const { agentModel = "scripted" } = options;
  if (agentModel !== null)
    for (const agent_id of options.agents ?? ["default"])
      await started.set({ kind: "agent", agent_id }, "model", `${model.provider.id}/${agentModel}`);
  return started;
}

/** The texts of a person's message or a tool's result. */
export function textsOf(message: Message | undefined): string[] {
  if (message === undefined || message.role === "assistant" || message.role === "system") return [];
  if (typeof message.content === "string") return [message.content];
  return message.content.flatMap((part) => (part.type === "text" ? [part.text] : []));
}

/**
 * A harness clock that moves only as the test advances it, firing the
 * timers then due. It declines timers that only free memory.
 */
export function fakeClock(start: string) {
  let time = Date.parse(start);
  const armed = new Set<{ readonly due: number; readonly fire: () => void }>();
  const setTimer: SetTimer = (fire, delayMs, { background }) => {
    if (background) return undefined;
    const timer = { due: time + delayMs, fire };
    armed.add(timer);
    if (delayMs <= 0) queueMicrotask(() => advance(0));
    return () => armed.delete(timer);
  };
  function advance(ms: number): void {
    time += ms;
    for (const timer of armed) if (timer.due <= time && armed.delete(timer)) timer.fire();
  }
  return { now: () => time, setTimer, advance };
}

/** A scripted answer that waits for `gate`, or its request's abort, before answering `text`. */
export function heldAnswer(text: string, gate: Promise<void>): FauxResponseFactory {
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
