/**
 * Carillon's Pi runtime on pi-durable. Each agent's conversations live in
 * one harness over `<component>/agents/<agent>.sqlite`; the runtime holds
 * every turn core hands over and settles it through core's client.
 */
import type { Extension, Storage } from "@earendil-works/pi-durable";
import { openNodeSqliteStorage } from "@earendil-works/pi-durable/storage/sqlite/node";
import type { ImageProcessor } from "@earendil-works/pi-durable/tools";
import type { CoreClient } from "@carillon/runtime/core";
import { ExplainedFailure } from "@carillon/runtime/failure";
import type { InboxTurns, RuntimeOperations } from "@carillon/runtime/operations";
import { ModelSelections } from "@carillon/runtime/selections";
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { openAgent, type Clock, type DurableAgent } from "./agent.ts";
import { DEFAULT_INLINE_IMAGES, type InlineImagePolicy } from "./attachments.ts";
import type { McpServer } from "./mcp.ts";
import { checkDefault, modelChoices, type Catalogue } from "./models.ts";

export interface PiRuntimeOptions {
  /** The agents the runtime serves, each with the existing directory its tools work in. */
  readonly agents: readonly { readonly id: string; readonly workspace: string }[];
  /** The runtime's private state: agents' storage and Carillon's model selections. */
  readonly componentDirectory: string;
  readonly catalogue: Catalogue;
  readonly core: CoreClient;
  readonly images: ImageProcessor;
  /** The built-in extensions, named as agents' settings select them. */
  readonly builtIns: readonly Extension[];
  /** What agents' settings select when they name no extensions. */
  readonly defaults: readonly string[];
  /** The MCP servers agents' settings may select; their owner closes them. */
  readonly mcp?: readonly McpServer[];
  /** Which attached images are passed inline; the rest stay files. */
  readonly inlineImages?: InlineImagePolicy;
  /** An agent's storage; its SQLite file by default. */
  readonly storage?: (agentId: string) => Promise<Storage>;
  readonly clock?: Clock;
}

/** Refuses to start, before any storage opens, unless the runtime default model can run. */
export async function createPiRuntime(
  options: PiRuntimeOptions,
): Promise<RuntimeOperations<InboxTurns>> {
  await mkdir(join(options.componentDirectory, "agents"), { recursive: true, mode: 0o700 });
  const selections = await ModelSelections.open(
    join(options.componentDirectory, "model-selection.json"),
  );
  await checkDefault(options.catalogue);
  const choices = modelChoices(options.catalogue, selections);
  const storage =
    options.storage ??
    ((agentId: string) =>
      openNodeSqliteStorage(join(options.componentDirectory, "agents", `${agentId}.sqlite`)));
  const agents = new Map<string, DurableAgent>();
  try {
    for (const agent of options.agents)
      agents.set(
        agent.id,
        await openAgent({
          id: agent.id,
          workspace: agent.workspace,
          storage: await storage(agent.id),
          catalogue: options.catalogue,
          choices,
          core: options.core,
          images: options.images,
          inlineImages: options.inlineImages ?? DEFAULT_INLINE_IMAGES,
          builtIns: options.builtIns,
          defaults: options.defaults,
          mcp: options.mcp ?? [],
          ...(options.clock === undefined ? {} : { clock: options.clock }),
        }),
      );
  } catch (failure) {
    await Promise.allSettled([...agents.values()].map((agent) => agent.close()));
    throw failure;
  }

  function agentOf(id: string): DurableAgent {
    const agent = agents.get(id);
    if (agent === undefined)
      throw new ExplainedFailure(422, "This conversation's agent does not run on Pi here.");
    return agent;
  }

  return {
    turns: {
      mode: "inbox",
      submit: async (request) => agentOf(request.agent_id).submit(request),
      async control(turnId, interfaceId, action) {
        for (const agent of agents.values())
          if (await agent.knows(turnId)) return agent.control(turnId, interfaceId, action);
        throw new ExplainedFailure(404, "This runtime holds no such message.");
      },
    },
    activity(correlationId, interfaceId) {
      for (const agent of agents.values()) {
        const activity = agent.activity(correlationId, interfaceId);
        if (activity !== undefined) return activity;
      }
      return undefined;
    },
    modelState: (query) => choices.state(query),
    setModel: (update) => choices.update(update),
    // The durable runtime runs no commands of its own yet.
    commands: () => Promise.resolve({ commands: [] }),
    restartReadiness() {
      return [...agents.values()].some((agent) => agent.calling)
        ? { state: "unready", reason: "a tool call is running" }
        : { state: "ready" };
    },
    async close() {
      await Promise.allSettled([...agents.values()].map((agent) => agent.close()));
    },
  };
}
