import {
  createAgentSession,
  createCodemodeExtension,
  createEventBus,
  createMcpExtension,
  createToolSearchExtension,
  type AgentSession,
  DefaultResourceLoader,
  type ExtensionFactory,
  type ModelRuntime,
  type SessionManager,
  SettingsManager,
  type ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import type { Api, ImageContent, Model, ModelThinkingLevel } from "@earendil-works/pi-ai";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { ExplainedFailure } from "./failure.ts";

interface PiExecutionSessionOptions {
  readonly resourceRoot: string;
  readonly workspace: string;
  readonly manager: SessionManager;
  readonly modelRuntime: ModelRuntime;
  readonly model?: Model<Api>;
  readonly thinkingLevel?: ModelThinkingLevel;

  readonly customTools?: ToolDefinition[];
  /** Per-session host data, delivered to native extensions but never to the model. */
  readonly extensionContext?: unknown;
  /** Host context for this turn's attachments, as a native extension. */
  readonly attachmentContext?: ExtensionFactory;
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
  const eventBus = createEventBus();
  const loader = new DefaultResourceLoader({
    eventBus,
    cwd: options.workspace,
    agentDir: options.resourceRoot,
    settingsManager,
    extensionFactories: [
      { name: "codemode", factory: createCodemodeExtension(), builtin: true, replaceable: true },
      {
        name: "tool-search",
        factory: createToolSearchExtension(),
        builtin: true,
        replaceable: true,
      },
      { name: "mcp", factory: createMcpExtension(), builtin: true, replaceable: true },
      ...(options.attachmentContext === undefined
        ? []
        : [{ name: "carillon-attachments", factory: options.attachmentContext, hidden: true }]),
    ],
    ...(systemPrompt === undefined ? {} : { systemPromptOverride: () => systemPrompt }),
  });
  await loader.reload();
  eventBus.emit("carillon:context", options.extensionContext);
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

/**
 * Cancel native execution on disconnect, and settle it before releasing the serial queue.
 * Images are Pi's native prompt input; the text keeps its command semantics.
 */
export async function promptSession(
  session: AgentSession,
  input: string,
  signal: AbortSignal,
  images: ImageContent[] = [],
) {
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
    await session.prompt(input, images.length === 0 ? undefined : { images });
    signal.throwIfAborted();
    const last = session.messages.at(-1);
    if (last?.role !== "assistant") return;
    // Pi reports provider and agent-loop failures alike as the assistant's
    // `errorMessage`, its explanation for Pi's user. Which of it may be shown
    // is decided where it leaves the runtime.
    if (last.stopReason === "error")
      throw last.errorMessage === undefined
        ? new Error("Pi reported an error without a message")
        : new ExplainedFailure(502, last.errorMessage);
    if (last.stopReason === "aborted") throw new Error("Pi execution was interrupted");
  } finally {
    signal.removeEventListener("abort", cancel);
    unsubscribe();
    await cancellation;
  }
}
