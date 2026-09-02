import {
  createAgentSession,
  DefaultResourceLoader,
  type ModelRuntime,
  type SessionEntry,
  SessionManager,
  SettingsManager,
  type ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import type { Api, Model, ModelThinkingLevel } from "@earendil-works/pi-ai";
import { readFile, readdir } from "node:fs/promises";
import { join } from "node:path";
import { modelRuntimeForServiceTier, type ServiceTier } from "./profile.ts";

interface PiExecutionSessionOptions {
  readonly resourceRoot: string;
  readonly workspace: string;
  readonly manager: SessionManager;
  readonly modelRuntime: ModelRuntime;
  readonly model?: Model<Api>;
  readonly thinkingLevel?: ModelThinkingLevel;
  readonly serviceTier?: ServiceTier;
  readonly customTools?: ToolDefinition[];
}

export interface RecoveredPiTurn {
  readonly output: string;
  readonly stopReason: string | undefined;
  readonly error: string | undefined;
  readonly capabilityActivity: readonly unknown[];
}

export function recoverPiTurn(entries: readonly SessionEntry[]): RecoveredPiTurn {
  let output = "";
  let stopReason: string | undefined;
  let error: string | undefined;
  const capabilityActivity: unknown[] = [];
  for (const entry of entries) {
    if (entry.type !== "message") continue;
    const message = entry.message;
    if (message.role === "toolResult") {
      if (message.toolName === "carillon_echo" && !message.isError && message.details !== undefined)
        capabilityActivity.push(message.details);
      continue;
    }
    if (message.role !== "assistant") continue;
    stopReason = message.stopReason;
    error = message.errorMessage;
    for (const content of message.content) if (content.type === "text") output += content.text;
  }
  return { output, stopReason, error, capabilityActivity };
}

export async function openOrCreatePiSession(
  workspace: string,
  sessionDirectory: string,
  sessionId: string,
  continuation?: string,
): Promise<SessionManager> {
  if (continuation !== undefined)
    return SessionManager.open(continuation, sessionDirectory, workspace);
  const suffix = `_${sessionId}.jsonl`;
  const existing = (await readdir(sessionDirectory))
    .filter((name) => name.endsWith(suffix))
    .sort()
    .at(-1);
  return existing === undefined
    ? SessionManager.create(workspace, sessionDirectory, { id: sessionId })
    : SessionManager.open(join(sessionDirectory, existing), sessionDirectory, workspace);
}

export async function createPiExecutionSession(options: PiExecutionSessionOptions) {
  const systemPrompt = await readFile(join(options.resourceRoot, "SYSTEM.md"), "utf8");
  const settingsManager = SettingsManager.create(options.workspace, options.resourceRoot);
  const loader = new DefaultResourceLoader({
    cwd: options.workspace,
    agentDir: options.resourceRoot,
    settingsManager,
    systemPromptOverride: () => systemPrompt,
  });
  await loader.reload();
  const created = await createAgentSession({
    cwd: options.workspace,
    agentDir: options.resourceRoot,
    modelRuntime: modelRuntimeForServiceTier(options.modelRuntime, options.serviceTier),
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
