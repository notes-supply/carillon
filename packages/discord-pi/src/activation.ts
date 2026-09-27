import type { ExtensionAPI, ToolDefinition } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

/** Exposure is session-local, not an authorization boundary. Pi persists the loadout. */
export function registerDiscordTools(
  pi: ExtensionAPI,
  tools: ToolDefinition[],
  ready: (signal?: AbortSignal) => Promise<unknown>,
): void {
  const names = tools.map((tool) => tool.name);
  for (const tool of tools) pi.registerTool(tool);
  pi.registerTool({
    name: "enable_discord",
    label: "Enable Discord",
    description:
      "Activate Discord reading, messaging and administration tools for this conversation. Read the Discord skill for guidance.",
    parameters: Type.Object({}),
    async execute(_id, _parameters, signal) {
      const connection = await ready(signal);
      pi.setActiveTools([...new Set([...pi.getActiveTools(), ...names])]);
      return {
        content: [{ type: "text", text: JSON.stringify({ enabled: names, connection }) }],
        details: { enabled: names },
      };
    },
  });
  pi.on("session_start", (_event, ctx) => {
    // Pi restores the native transcript loadout before session_start. Only a
    // new transcript needs pruning; never re-enable old tools by default.
    const hasSystem = ctx.sessionManager
      .getBranch()
      .some((entry) => entry.type === "message" && entry.message.role === "system");
    const active = pi.getActiveTools().filter((name) => hasSystem || !names.includes(name));
    pi.setActiveTools([...new Set([...active, "enable_discord"])]);
  });
}
