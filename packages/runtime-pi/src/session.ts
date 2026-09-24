import {
  createAgentSession,
  type AgentSession,
  DefaultResourceLoader,
  type ModelRuntime,
  type SessionManager,
  SettingsManager,
  type ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import type { Api, Model, ModelThinkingLevel } from "@earendil-works/pi-ai";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { displayableExplanation, ExplainedFailure } from "./failure.ts";

interface PiExecutionSessionOptions {
  readonly resourceRoot: string;
  readonly workspace: string;
  readonly manager: SessionManager;
  readonly modelRuntime: ModelRuntime;
  readonly model?: Model<Api>;
  readonly thinkingLevel?: ModelThinkingLevel;

  readonly customTools?: ToolDefinition[];
}

export async function createPiExecutionSession(options: PiExecutionSessionOptions) {
  const systemPrompt = await readFile(join(options.resourceRoot, "SYSTEM.md"), "utf8").catch(
    (failure: unknown) => {
      if (failure instanceof Error && "code" in failure && failure.code === "ENOENT")
        return undefined;
      throw failure;
    },
  );
  const settingsManager = SettingsManager.create(options.workspace, options.resourceRoot);
  const loader = new DefaultResourceLoader({
    cwd: options.workspace,
    agentDir: options.resourceRoot,
    settingsManager,
    ...(systemPrompt === undefined ? {} : { systemPromptOverride: () => systemPrompt }),
  });
  await loader.reload();
  const created = await createAgentSession({
    cwd: options.workspace,
    agentDir: options.resourceRoot,
    modelRuntime: options.modelRuntime,
    ...(options.model === undefined ? {} : { model: options.model }),
    ...(options.thinkingLevel === undefined ? {} : { thinkingLevel: options.thinkingLevel }),
    resourceLoader: loader,
    sessionManager: options.manager,
    settingsManager,
    customTools: options.customTools ?? [],
  });
  await created.session.bindExtensions({ mode: "print" });
  return created;
}

/** Cancel native execution on disconnect, and settle it before releasing the serial queue. */
export async function promptSession(session: AgentSession, input: string, signal: AbortSignal) {
  signal.throwIfAborted();
  let cancellation: Promise<void> | undefined;
  const cancel = () => {
    cancellation = session.abort();
  };
  signal.addEventListener("abort", cancel, { once: true });
  // Preparation can await extensions before Pi starts its agent loop.
  const unsubscribe = session.subscribe((event) => {
    if (event.type === "agent_start" && signal.aborted) cancel();
  });
  try {
    await session.prompt(input);
    signal.throwIfAborted();
    const last = session.messages.at(-1);
    if (last?.role !== "assistant") return;
    if (last.stopReason === "error") throw piFailure(last.errorMessage);
    if (last.stopReason === "aborted") throw new Error("Pi execution was interrupted");
  } finally {
    signal.removeEventListener("abort", cancel);
    unsubscribe();
    await cancellation;
  }
}

/**
 * Pi reports a failed turn, whether from the model provider or its agent loop,
 * as the assistant's `errorMessage`: its explanation for Pi's user. What of it
 * is safe to display becomes the turn's explanation; the whole text remains the
 * diagnostic.
 */
function piFailure(errorMessage: string | undefined): Error {
  const detail = errorMessage ?? "Pi reported an error without a message";
  const explanation = displayableExplanation(detail);
  return explanation === undefined
    ? new Error(detail)
    : new ExplainedFailure(502, explanation, { cause: detail });
}
