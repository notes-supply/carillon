/**
 * A runtime for tests to kill with SIGKILL: Pi's runtime over SQLite in the
 * directory it is given, its core at the URL it is given. Given a message,
 * it hands it to the agent as turn `turn-1`. Its model schedules
 * `scheduled` soon for `schedule`, hands `hold` to a subagent for
 * `delegate`, and for `hold` calls its `hold` tool, which prints `hold` and
 * never returns, until crashes have cut that tool off `DEFAULT_RESTART_LIMIT`
 * times. It echoes anything else.
 */
import {
  createModels,
  fauxAssistantMessage,
  fauxProvider,
  fauxToolCall,
  type FauxResponseFactory,
} from "@earendil-works/pi-ai";
import { DEFAULT_RESTART_LIMIT, defineExtension, defineTool } from "@earendil-works/pi-durable";
import { createCoreClient } from "@carillon/runtime/core";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { Type } from "typebox";
import { textsOf } from "./fixture.ts";
import { BUILT_IN_SKILLS } from "./resources.ts";
import { createPiRuntime } from "./runtime.ts";

const [root, url, message] = process.argv.slice(2);
if (root === undefined || url === undefined)
  throw new Error("usage: crash-worker.ts <directory> <core URL> [message]");

const call = (name: string, args: Record<string, string>) =>
  fauxAssistantMessage(fauxToolCall(name, args), { stopReason: "toolUse" });

const answer: FauxResponseFactory = (context) => {
  const last = context.messages.at(-1);
  if (last?.role === "toolResult") {
    if (last.toolName !== "hold") return fauxAssistantMessage("Done.");
    const held = context.messages.filter(
      (earlier) => earlier.role === "toolResult" && earlier.toolName === "hold",
    ).length;
    return held < DEFAULT_RESTART_LIMIT
      ? call("hold", {})
      : fauxAssistantMessage(`Held ${String(held)} times.`);
  }
  const texts = textsOf(last);
  switch (texts.at(-1) ?? "") {
    case "schedule":
      // A schedule runs on a whole second, so this is at least one second ahead.
      return call("schedule", {
        prompt: "scheduled",
        at: new Date(Date.now() + 2_000).toISOString(),
      });
    case "delegate":
      return call("subagent", { task: "hold" });
    case "hold":
      return call("hold", {});
    default:
      return fauxAssistantMessage(`Answer: ${texts.join(" ")}`);
  }
};

const model = fauxProvider({ models: [{ id: "scripted", name: "Scripted" }] });
model.setResponses(Array.from({ length: 100 }, () => answer));
const models = createModels();
models.setProvider(model.provider);
const holding = defineExtension({
  name: "holding",
  tools: [
    defineTool({
      name: "hold",
      description: "Holds.",
      parameters: Type.Object({}),
      execute: () => {
        console.log("hold");
        return new Promise<never>(() => undefined);
      },
    }),
  ],
});
const runtime = await createPiRuntime({
  agents: [{ id: "default", workspace: root }],
  componentDirectory: root,
  skillsDirectory: fileURLToPath(BUILT_IN_SKILLS),
  catalogue: { models, defaultThinking: () => "medium" },
  core: createCoreClient({ url, providerId: "runtime-pi", token: "test-only" }),
  images: {
    prepare: (bytes, mimeType) =>
      Promise.resolve({ data: Buffer.from(bytes).toString("base64"), mimeType }),
  },
  builtIns: [holding],
  defaults: ["schedules", "subagents", "holding"],
});
if (message !== undefined) {
  const subject = { kind: "agent", agent_id: "default" } as const;
  const { settings } = await runtime.settings({ subject });
  const line = settings.find(({ id }) => id === "model");
  if (line === undefined) throw new Error("the agent shows no model line");
  await runtime.settings({
    subject,
    change: {
      setting: "model",
      value: { kind: "one", id: `${model.provider.id}/scripted` },
      expected: line.revision,
    },
  });
  await runtime.turns.submit({
    correlation_id: "turn-1",
    interface_id: "discord-private",
    agent_id: "default",
    audience_id: "conversation",
    channel_id: "300",
    input: message,
    resource_root: join(root, "empty"),
    attachment_root: join(root, "attachments"),
  });
}
console.log("ready");
