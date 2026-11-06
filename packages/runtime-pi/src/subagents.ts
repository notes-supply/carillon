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
  type Models,
  type ModelThinkingLevel,
} from "@earendil-works/pi-ai";
import {
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
import { SCHEDULES } from "./schedules.ts";

const SUBAGENTS = "subagents";
const REPORTER = "carillon.subagent";
const SPEEDS = ["standard", "fast"] as const;
type Speed = (typeof SPEEDS)[number];
/** The pi-ai APIs whose request body takes OpenAI's `service_tier`, which `fast` sets to `priority`. */
const SERVICE_TIER_APIS: ReadonlySet<Api> = new Set(["openai-responses", "openai-codex-responses"]);

/** Each background call, by its tool task: the reporter it started. */
const Calls = defineDocFamily<{ reporter?: TaskId }, null>({
  kind: "carillon.subagent-call",
  version: 1,
  scope: "session",
  family: true,
  initial: () => ({}),
  checkpointWhen: () => true,
});

/** A child conversation's speed; pi-durable's agent holds its model and thinking. */
const ChildSpeed = defineDoc<{ speed: Speed }>({
  kind: "carillon.subagent-speed",
  version: 1,
  scope: "conversation",
  history: "latest",
  fork: "current",
  initial: () => ({ speed: "standard" }),
  checkpointWhen: () => true,
});

interface ChildAgent {
  readonly model: ModelRef;
  readonly thinkingLevel: ModelThinkingLevel;
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

/** `opening` opens the turns that bring background subagents' answers. */
export function createSubagentsExtension(host: { readonly opening: OpeningTask }): Extension {
  const reporter = defineTask<
    { readonly task: string; readonly agent: ChildAgent },
    ReporterState,
    null
  >({
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
              child: await childOf(tx, task.id, runtime.conversationId, task.input.agent),
            },
          }),
          context,
        ),
      run: async (task, runtime, context) => {
        const { child: id } = await recordCutOffs(task, runtime, context);
        const child = await runtime.conversation(id, context);
        if (child === undefined) throw new Error(`subagent ${String(task.id)} has no conversation`);
        const submission = await child.submit(
          { type: "input", content: task.input.task, requestId: `subagent:${String(task.id)}` },
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
              "fast asks the provider for priority processing, which OpenAI's Responses APIs offer; refused for other models.",
          }),
        ),
      },
      { additionalProperties: false },
    ),
    // A rerun after a crash finds the child, the submission or the reporter its call made.
    replay: "safe",
    async execute({ task, wait, ...requested }, api, context) {
      const { agent, note } = await choose(requested, await api.agent(context), api.models);
      if (wait !== true) {
        const started = await api.commit(async (tx) => {
          const call = await tx.doc(Calls, String(api.taskId), null);
          call.reporter ??= await tx.createTask(
            reporter,
            { task, agent },
            { ownership: { kind: "conversation" }, background: true },
          );
          return call.reporter;
        }, context);
        return text(
          note,
          `Subagent ${String(started)} is working on it in the background; its answer will come to you as a message of its own.`,
        );
      }
      const id = await api.commit(
        (tx) => childOf(tx, api.taskId, api.conversationId, agent),
        context,
      );
      const child = await api.conversation(id, context);
      if (child === undefined) throw new Error("The subagent's conversation is gone.");
      const submission = await child.submit(
        { type: "input", content: task, requestId: `subagent:${String(api.taskId)}` },
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
   * The child conversation `owner` owns, created once as a copy of the
   * agent of `parent`, its conversation, running `agent`, that cannot start
   * subagents or schedules, or attach files to an answer. It serves the
   * parent's Carillon conversation, so tools such as Discord's find its
   * interface and origin.
   */
  async function childOf(
    tx: Tx,
    owner: TaskId,
    parent: ConversationId,
    agent: ChildAgent,
  ): Promise<ConversationId> {
    const [existing] = (await tx.scanConversations({ ownerTaskId: owner }, 1)).items;
    if (existing !== undefined) return existing.id;
    const created = await tx.createConversation({ ownership: { kind: "task", taskId: owner } });
    await configure(tx, created.id, {
      model: agent.model,
      thinkingLevel: agent.thinkingLevel,
      extensions: { remove: [{ name: SCHEDULES }] },
      tools: { remove: [ATTACH, subagent] },
    });
    (await tx.doc(ChildSpeed, created.id)).speed = agent.speed;
    Object.assign(
      await tx.doc(ConversationInfo, created.id),
      copyJson(await tx.doc(ConversationInfo, parent)),
    );
    return created.id;
  }

  return defineExtension({
    name: SUBAGENTS,
    tasks: [reporter],
    tools: [subagent],
    hooks: [
      hook(GenerationTask, {
        async beforeProviderRequest({ model, payload }, api, context) {
          const fast =
            (await api.snapshot(ChildSpeed, api.conversationId, context))?.speed === "fast";
          if (!fast || !SERVICE_TIER_APIS.has(model.api) || !isObject(payload)) return undefined;
          return { payload: { ...payload, service_tier: "priority" } };
        },
      }),
    ],
  });
}

/**
 * What the child runs: the requested model among those Pi offers now,
 * else the parent's; the requested thinking level, else the parent's,
 * clamped to what that model supports; and the requested speed. The note
 * tells the calling agent what it got.
 */
async function choose(
  requested: Requested,
  parent: Agent,
  models: Models,
): Promise<{ readonly agent: ChildAgent; readonly note: string }> {
  const name =
    requested.model ??
    (parent.model === undefined ? undefined : `${parent.model.provider}/${parent.model.modelId}`);
  const available = await models.getAvailable();
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
      `${name} runs the ${model.api} API, which has no faster service tier; only OpenAI's Responses APIs do.`,
    );
  const clamped = thinkingLevel === wanted ? "" : ` (${name} does not support thinking ${wanted})`;
  return {
    agent: { model: { provider: model.provider, modelId: model.id }, thinkingLevel, speed },
    note: `The subagent runs ${name} with thinking ${thinkingLevel}${clamped}, at ${speed} speed.`,
  };
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

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function text(...values: string[]) {
  return { output: values.map((value) => ({ type: "text" as const, text: value })) };
}
