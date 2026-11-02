/**
 * `--deterministic`: scripted local models in place of Pi's catalogue, for
 * service tests. Each answer says what reached the model: how many messages
 * the conversation holds, the message, whether the agent's extension and
 * skill reached its prompt, the model and thinking level it ran with, and
 * the attachments its note listed. A message `hold <path>` is answered once
 * that file exists, so a test can keep a turn running.
 */
import {
  createFauxCore,
  createModels,
  createProvider,
  fauxAssistantMessage,
  type FauxResponseFactory,
  type Message,
} from "@earendil-works/pi-ai";
import { access } from "node:fs/promises";
import type { PiCatalogue } from "./models.ts";

const HOLD = /^hold (\S+)$/u;
const ATTACHMENT = /name: ("(?:[^"\\]|\\.)*"); size: ([^;]+);/gu;

export function deterministicCatalogue(): PiCatalogue {
  const models = createModels();
  for (const [id, name, definitions] of [
    [
      "local",
      "Local",
      [
        { id: "echo", name: "Local echo" },
        { id: "reasoner", name: "Local reasoner", reasoning: true },
      ],
    ],
    ["substitute", "Substitute", [{ id: "fixed", name: "Substitute fixed" }]],
  ] as const) {
    const core = createFauxCore({ provider: id, models: [...definitions] });
    // Minimal thinking is not one the picker offers here.
    const offered = core.models.map((model) =>
      model.reasoning ? { ...model, thinkingLevelMap: { minimal: null } } : model,
    );
    models.setProvider(
      createProvider({
        id,
        name,
        auth: { apiKey: { name, resolve: () => Promise.resolve({ auth: {} }) } },
        models: offered,
        // One scripted answer per request, queued as the request starts.
        api: {
          stream: (model, context, options) => {
            core.appendResponses([answer]);
            return core.stream(model, context, options);
          },
          streamSimple: (model, context, options) => {
            core.appendResponses([answer]);
            return core.streamSimple(model, context, options);
          },
        },
      }),
    );
  }
  return { models, defaultThinking: () => "medium" };
}

const answer: FauxResponseFactory = async (context, options, _state, model) => {
  const users = context.messages.filter((message) => message.role === "user");
  const texts = textsOf(users.at(-1));
  // Carillon's own notes travel beside the person's text.
  const input = texts.find((text) => !text.startsWith("[Carillon")) ?? "";
  const held = HOLD.exec(input)?.[1];
  while (held !== undefined && !(await exists(held))) {
    options?.signal?.throwIfAborted();
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  const prompt = Object.values(sections(context.messages)).join("\n");
  const markers = [
    prompt.includes("Carillon native extension active")
      ? "native extension active"
      : "native extension inactive",
    prompt.includes("carillon-status") ? "skill active" : "skill inactive",
    `model ${model.provider}/${model.id}${model.reasoning ? ` (thinking=${options?.reasoning ?? "off"})` : ""}`,
    ...[...texts.join("\n").matchAll(ATTACHMENT)].map(
      ([, name = '""', size = ""]) => `attachment ${String(JSON.parse(name))} ${size}`,
    ),
  ];
  return fauxAssistantMessage(
    `Local provider response ${String(users.length)}: ${input}. ${markers.join("; ")}.`,
  );
};

function textsOf(message: Message | undefined): string[] {
  if (message?.role !== "user") return [];
  if (typeof message.content === "string") return [message.content];
  return message.content.flatMap((part) => (part.type === "text" ? [part.text] : []));
}

/** The prompt sections in effect after the conversation's system messages, in order. */
function sections(messages: readonly Message[]): Record<string, string> {
  const current: Record<string, string> = {};
  for (const message of messages)
    if (message.role === "system")
      for (const [key, value] of Object.entries(message.sections ?? {}))
        if (value === null) delete current[key];
        else current[key] = value;
  return current;
}

async function exists(path: string): Promise<boolean> {
  return access(path).then(
    () => true,
    () => false,
  );
}
