/**
 * Subagents: the agent hands a self-contained task to a child conversation
 * that starts as a copy of its own agent, without subagents or schedules,
 * serving the same Carillon conversation. The call may give the child
 * another model, thinking level or speed. In the background, the default, a
 * reporter task of the conversation owns the child, so stopping the answer
 * that started it leaves the child working, and its answer comes back in a
 * turn the runtime opens. With `wait`, the call owns the child and returns
 * its answer.
 */
import { copyJson } from "@earendil-works/chord";
import {
  clampThinkingLevel,
  StringEnum,
  type Api,
  type Model,
  type ModelThinkingLevel,
} from "@earendil-works/pi-ai";
import {
  AgentDoc,
  AssistantEntry,
  configure,
  defineDoc,
  defineDocFamily,
  defineExtension,
  defineTask,
  defineTool,
  GenerationTask,
  hook,
  type Agent,
  type ConversationId,
  type Extension,
  type ModelRef,
  type SettledSubmissionRecord,
  type TaskId,
  type Tx,
} from "@earendil-works/pi-durable";
import { Type } from "typebox";
import { ATTACH } from "./answers.ts";
import { recordCutOffs } from "./delivery.ts";
import { ConversationInfo } from "./documents.ts";
import { THINKING_LEVELS } from "./models.ts";
import type { OpeningTask } from "./opening.ts";
import { noted } from "./resources.ts";
import { SCHEDULES } from "./schedules.ts";

const SUBAGENTS = "subagents";
const REPORTER = "carillon.subagent";
const SPEEDS = ["standard", "fast"] as const;
type Speed = (typeof SPEEDS)[number];
/** The pi-ai APIs whose request body takes OpenAI's `service_tier`, which `fast` sets to `priority`. */
const SERVICE_TIER_APIS: ReadonlySet<Api> = new Set([
  "openai-responses",
  "openai-codex-responses",
  "azure-openai-responses",
]);

/** Each background call, by its tool task: the reporter it started. */
const Calls = defineDocFamily<{ reporter?: TaskId }, null>({
  kind: "carillon.subagent-call",
  version: 1,
  scope: "session",
  family: true,
  initial: () => ({}),
  checkpointWhen: () => true,
});

/**
 * A child conversation's speed, and the thinking level its call asked for
 * when its model lacks that level; pi-durable's agent holds the model and
 * thinking it runs.
 */
const ChildChoices = defineDoc<{ speed: Speed; unsupportedThinking?: ModelThinkingLevel }>({
  kind: "carillon.subagent-choices",
  version: 1,
  scope: "conversation",
  history: "latest",
  fork: "current",
  initial: () => ({ speed: "standard" }),
  checkpointWhen: () => true,
});

interface Choice {
  readonly model: ModelRef;
  readonly thinkingLevel: ModelThinkingLevel;
  readonly unsupportedThinking: ModelThinkingLevel | undefined;
  readonly speed: Speed;
}

interface Requested {
  readonly model?: string | undefined;
  readonly thinking?: ModelThinkingLevel | undefined;
  readonly speed?: Speed | undefined;
}

/** What a child made of its task. */
type Answer =
  | { readonly kind: "answered"; readonly text: string }
  | { readonly kind: "failed"; readonly reason: string }
  | { readonly kind: "stopped" };

type ReporterState =
  | { readonly phase: "start" }
  | { readonly phase: "run"; readonly child: ConversationId; readonly cutOff?: number };

const COMPLETED = { status: "terminal", outcome: { status: "completed", result: null } } as const;

/**
 * `opening` opens the turns that bring background subagents' answers, and
 * `refresh` runs the agent on its resources as they are when a subagent
 * starts, returning the note that says why it cannot.
 */
export function createSubagentsExtension(host: {
  readonly opening: OpeningTask;
  readonly refresh: () => Promise<string | undefined>;
}): Extension {
  const reporter = defineTask<{ readonly task: string }, ReporterState, null>({
    name: REPORTER,
    version: 1,
    initial: () => ({ phase: "start" }),
    phases: {
      start: (task, runtime, context) =>
        runtime.commit(
          async (tx) => ({
            status: "running",
            checkpoint: {
              phase: "run",
              child: await childOf(tx, task.id, runtime.conversationId),
            },
          }),
          context,
        ),
      run: async (task, runtime, context) => {
        const { child: id } = await recordCutOffs(task, runtime, context);
        const child = await runtime.conversation(id, context);
        if (child === undefined) throw new Error(`subagent ${String(task.id)} has no conversation`);
        const submission = await child.submit(
          {
            type: "input",
            content: noted(task.input.task, await host.refresh()),
            requestId: `subagent:${String(task.id)}`,
          },
          context,
        );
        const settled = await submission.wait(context);
        await runtime.commit(async (tx) => {
          const answer = await answerOf(tx, settled);
          if (answer.kind !== "stopped")
            await tx.createTask(
              host.opening,
              {
                key: `subagent:${String(task.id)}`,
                label: "Subagent report",
                content: [
                  {
                    type: "text",
                    text:
                      answer.kind === "answered"
                        ? `[Carillon] The subagent you started in the background finished this task: ${task.input.task}`
                        : `[Carillon] The subagent you started in the background gave no answer (${answer.reason}) to this task: ${task.input.task}`,
                  },
                  ...(answer.kind === "answered"
                    ? [{ type: "text" as const, text: answer.text }]
                    : []),
                ],
              },
              { ownership: { kind: "conversation" }, background: true },
            );
          return COMPLETED;
        }, context);
      },
    },
    abort: (_task, runtime, context) =>
      runtime.commit(() => ({ status: "terminal", outcome: { status: "aborted" } }), context),
  });

  const subagent = defineTool({
    name: "subagent",
    description:
      "Hands a self-contained task to a subagent: a new conversation with your tools, but none of this conversation's history, so say everything it needs. It runs your model, thinking level and speed, except those you set with model, thinking and speed. By default it works in the background and this call returns at once; its answer comes to you later as a message of its own, and you can carry on meanwhile. With wait: true, this call waits for its answer and returns it.",
    parameters: Type.Object(
      {
        task: Type.String({ minLength: 1, description: "What the subagent should do." }),
        wait: Type.Optional(
          Type.Boolean({
            description: "Wait for the answer instead of working in the background.",
          }),
        ),
        model: Type.Optional(
          Type.String({
            minLength: 1,
            description:
              "The model it runs, as <provider>/<model>. An unknown or unavailable one is refused with the models you can choose.",
          }),
        ),
        thinking: Type.Optional(
          StringEnum(THINKING_LEVELS, {
            description:
              "Its thinking level, moved to the nearest level its model supports when the model does not support this one.",
          }),
        ),
        speed: Type.Optional(
          StringEnum(SPEEDS, {
            description:
              "fast asks the provider for priority processing, which the OpenAI and Azure OpenAI Responses APIs offer; refused for other models.",
          }),
        ),
      },
      { additionalProperties: false },
    ),
    // A rerun after a crash finds the child, the submission or the reporter its call made.
    replay: "safe",
    async execute({ task, wait, ...requested }, api, context) {
      const parent = await api.agent(context);
      const available = await api.models.getAvailable();
      if (wait !== true) {
        const { started, note } = await api.commit(async (tx) => {
          const call = await tx.doc(Calls, String(api.taskId), null);
          if (call.reporter !== undefined)
            return {
              started: call.reporter,
              note: await describe(tx, await childOf(tx, call.reporter, api.conversationId)),
            };
          const choice = choose(requested, parent, available);
          call.reporter = await tx.createTask(
            reporter,
            { task },
            { ownership: { kind: "conversation" }, background: true },
          );
          const child = await createChild(tx, call.reporter, api.conversationId, choice);
          return { started: call.reporter, note: await describe(tx, child) };
        }, context);
        return text(
          note,
          `Subagent ${String(started)} is working on it in the background; its answer will come to you as a message of its own.`,
        );
      }
      const { id, note } = await api.commit(async (tx) => {
        const child =
          (await childOwnedBy(tx, api.taskId)) ??
          (await createChild(
            tx,
            api.taskId,
            api.conversationId,
            choose(requested, parent, available),
          ));
        return { id: child, note: await describe(tx, child) };
      }, context);
      const child = await api.conversation(id, context);
      if (child === undefined) throw new Error("The subagent's conversation is gone.");
      const submission = await child.submit(
        {
          type: "input",
          content: noted(task, await host.refresh()),
          requestId: `subagent:${String(api.taskId)}`,
        },
        context,
      );
      const settled = await submission.wait(context);
      const answer = await api.commit((tx) => answerOf(tx, settled), context);
      if (answer.kind !== "answered")
        throw new Error(
          `The subagent gave no answer (${answer.kind === "failed" ? answer.reason : "stopped"}).`,
        );
      return text(note, answer.text);
    },
  });

  /**
   * Creates the child conversation `owner` owns, a copy of the agent of
   * `parent`, its conversation, that cannot start subagents or schedules,
   * or attach files to an answer, and runs `choice`, else what it copied at
   * standard speed. It serves the parent's Carillon conversation, so tools
   * such as Discord's find its interface and origin.
   */
  async function createChild(
    tx: Tx,
    owner: TaskId,
    parent: ConversationId,
    choice?: Choice,
  ): Promise<ConversationId> {
    const created = await tx.createConversation({ ownership: { kind: "task", taskId: owner } });
    await configure(tx, created.id, {
      ...(choice === undefined ? {} : { model: choice.model, thinkingLevel: choice.thinkingLevel }),
      extensions: { remove: [{ name: SCHEDULES }] },
      tools: { remove: [ATTACH, subagent] },
    });
    if (choice !== undefined)
      Object.assign(await tx.doc(ChildChoices, created.id), {
        speed: choice.speed,
        ...(choice.unsupportedThinking === undefined
          ? {}
          : { unsupportedThinking: choice.unsupportedThinking }),
      });
    Object.assign(
      await tx.doc(ConversationInfo, created.id),
      copyJson(await tx.doc(ConversationInfo, parent)),
    );
    return created.id;
  }

  async function childOf(tx: Tx, owner: TaskId, parent: ConversationId): Promise<ConversationId> {
    return (await childOwnedBy(tx, owner)) ?? createChild(tx, owner, parent);
  }

  return defineExtension({
    name: SUBAGENTS,
    tasks: [reporter],
    tools: [subagent],
    hooks: [
      hook(GenerationTask, {
        async beforeProviderRequest({ model, payload }, api, context) {
          if (!SERVICE_TIER_APIS.has(model.api) || !isObject(payload)) return undefined;
          const choices = await api.snapshot(ChildChoices, api.conversationId, context);
          if (choices?.speed !== "fast") return undefined;
          return { payload: { ...payload, service_tier: "priority" } };
        },
      }),
    ],
  });
}

/**
 * What a new child runs: the requested model among those Pi offers now,
 * else the parent's; the requested thinking level, else the parent's,
 * clamped to what that model supports; and the requested speed.
 */
function choose(requested: Requested, parent: Agent, available: readonly Model<Api>[]): Choice {
  const name = requested.model ?? (parent.model === undefined ? undefined : refName(parent.model));
  const model = available.find((offered) => nameOf(offered) === name);
  if (model === undefined)
    throw new Error(
      `${name === undefined ? "No model is chosen" : `Pi offers no model ${name}`}. Choose one of: ${available.map(nameOf).join(", ")}.`,
    );
  const wanted = requested.thinking ?? parent.thinkingLevel;
  const thinkingLevel = clampThinkingLevel(model, wanted);
  const speed = requested.speed ?? "standard";
  if (speed === "fast" && !SERVICE_TIER_APIS.has(model.api))
    throw new Error(
      `${name} runs the ${model.api} API, which has no faster service tier; these APIs have one: ${[...SERVICE_TIER_APIS].join(", ")}.`,
    );
  return {
    model: { provider: model.provider, modelId: model.id },
    thinkingLevel,
    unsupportedThinking: thinkingLevel === wanted ? undefined : wanted,
    speed,
  };
}

async function childOwnedBy(tx: Tx, owner: TaskId): Promise<ConversationId | undefined> {
  return (await tx.scanConversations({ ownerTaskId: owner }, 1)).items[0]?.id;
}

async function describe(tx: Tx, child: ConversationId): Promise<string> {
  const { model, thinkingLevel = "off" } = await tx.doc(AgentDoc, child);
  const { speed, unsupportedThinking } = await tx.doc(ChildChoices, child);
  const name = model === undefined ? "no model" : refName(model);
  const clamped =
    unsupportedThinking === undefined
      ? ""
      : ` (${name} does not support thinking ${unsupportedThinking})`;
  return `The subagent runs ${name} with thinking ${thinkingLevel}${clamped}, at ${speed} speed.`;
}

async function answerOf(tx: Tx, settled: SettledSubmissionRecord): Promise<Answer> {
  if (settled.status === "unanswered")
    return settled.reason === "aborted" || settled.reason === "reset"
      ? { kind: "stopped" }
      : { kind: "failed", reason: settled.reason };
  if (settled.type !== "input") return { kind: "failed", reason: "no answer" };
  const message = (await tx.entry(AssistantEntry, settled.answer))?.model?.[0];
  const answer =
    message?.role === "assistant"
      ? message.content.flatMap((part) => (part.type === "text" ? [part.text] : [])).join("")
      : "";
  return answer.trim().length === 0
    ? { kind: "failed", reason: "an empty answer" }
    : { kind: "answered", text: answer };
}

function nameOf(model: { readonly provider: string; readonly id: string }): string {
  return `${model.provider}/${model.id}`;
}

function refName(model: ModelRef): string {
  return `${model.provider}/${model.modelId}`;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function text(...values: string[]) {
  return { output: values.map((value) => ({ type: "text" as const, text: value })) };
}
