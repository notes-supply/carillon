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
import type {
  Api,
  AssistantMessage,
  ImageContent,
  Model,
  ModelThinkingLevel,
} from "@earendil-works/pi-ai";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { ExplainedFailure } from "@carillon/runtime/failure";
import { WORK_CHANNEL } from "./activity.ts";

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
  readonly attachmentContext?: ExtensionFactory;
  /** Receives each fact participating extensions report on the session's `carillon:work` channel. */
  readonly onWork?: (fact: unknown) => void;
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
  if (options.onWork !== undefined) eventBus.on(WORK_CHANNEL, options.onWork);
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
 *
 * Returns the prompt's own answer: the last assistant message Pi ended
 * while it ran, or none when Pi consumed the prompt without one, as an
 * extension's command may. Only that answer's failure is this prompt's: a
 * saved conversation can end with an earlier turn's.
 */
export async function promptSession(
  session: AgentSession,
  input: string,
  signal: AbortSignal,
  images: ImageContent[] = [],
): Promise<AssistantMessage | undefined> {
  signal.throwIfAborted();
  let cancellation: Promise<void> | undefined;
  const cancel = () => {
    cancellation = session.abort();
  };
  signal.addEventListener("abort", cancel, { once: true });
  const own: { answer?: AssistantMessage } = {};
  // Preparation can await extensions before Pi starts its agent loop.
  const unsubscribe = session.subscribe((event) => {
    if (event.type === "agent_start" && signal.aborted) cancel();
    if (event.type === "message_end" && event.message.role === "assistant")
      own.answer = event.message;
  });
  try {
    await session.prompt(input, images.length === 0 ? undefined : { images });
    signal.throwIfAborted();
    const { answer } = own;
    if (answer === undefined) return undefined;
    // Pi reports provider and agent-loop failures alike as the assistant's
    // `errorMessage`, its explanation for Pi's user. Which of it may be shown
    // is decided where it leaves the runtime.
    if (answer.stopReason === "error")
      throw answer.errorMessage === undefined
        ? new Error("Pi reported an error without a message")
        : new ExplainedFailure(502, answer.errorMessage);
    if (answer.stopReason === "aborted") throw new Error("Pi execution was interrupted");
    return answer;
  } finally {
    signal.removeEventListener("abort", cancel);
    unsubscribe();
    await cancellation;
  }
}
