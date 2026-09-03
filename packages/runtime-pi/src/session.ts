import {
  createAgentSession,
  DefaultResourceLoader,
  type ModelRuntime,
  type SessionManager,
  SettingsManager,
  type ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import type { Api, Model, ModelThinkingLevel } from "@earendil-works/pi-ai";
import { readFile } from "node:fs/promises";
import { join } from "node:path";

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
