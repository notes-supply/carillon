/**
 * Subagents: the agent hands a self-contained task to a child conversation
 * that starts as a copy of its own agent, without subagents or schedules,
 * serving the same Carillon conversation. In the background, the default, a
 * reporter task of the conversation owns the child, so stopping the answer
 * that started it leaves the child working, and its answer comes back in a
 * turn the runtime opens. With `wait`, the call owns the child and returns
 * its answer.
 */
import { copyJson } from "@earendil-works/chord";
import {
  AssistantEntry,
  configure,
  defineDocFamily,
  defineExtension,
  defineTask,
  defineTool,
  type ConversationId,
  type Extension,
  type SettledSubmissionRecord,
  type TaskId,
  type Tx,
} from "@earendil-works/pi-durable";
import { Type } from "typebox";
import { ConversationInfo } from "./documents.ts";
import type { OpeningTask } from "./opening.ts";
import { SCHEDULES } from "./schedules.ts";

const SUBAGENTS = "subagents";
export const REPORTER = "carillon.subagent";

/** Each background call, by its tool task: the reporter it started. */
const Calls = defineDocFamily<{ reporter?: TaskId }, null>({
  kind: "carillon.subagent-call",
  version: 1,
  scope: "session",
  family: true,
  initial: () => ({}),
  checkpointWhen: () => true,
});

/** What a child made of its task. */
type Answer =
  | { readonly kind: "answered"; readonly text: string }
  | { readonly kind: "failed"; readonly reason: string }
  | { readonly kind: "stopped" };

type ReporterState =
  | { readonly phase: "start" }
  | { readonly phase: "run"; readonly child: ConversationId };

const COMPLETED = { status: "terminal", outcome: { status: "completed", result: null } } as const;

/** `opening` opens the turns that bring background subagents' answers. */
export function createSubagentsExtension(host: { readonly opening: OpeningTask }): Extension {
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
        const child = await runtime.conversation(task.state.checkpoint.child, context);
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

  return defineExtension({
    name: SUBAGENTS,
    tasks: [reporter],
    tools: [
      defineTool({
        name: "subagent",
        description:
          "Hands a self-contained task to a subagent: a new conversation with your model and tools, but none of this conversation's history, so say everything it needs. By default it works in the background and this call returns at once; its answer comes to you later as a message of its own, and you can carry on meanwhile. With wait: true, this call waits for its answer and returns it.",
        parameters: Type.Object(
          {
            task: Type.String({ minLength: 1, description: "What the subagent should do." }),
            wait: Type.Optional(
              Type.Boolean({
                description: "Wait for the answer instead of working in the background.",
              }),
            ),
          },
          { additionalProperties: false },
        ),
        // A rerun after a crash finds the child, the submission or the reporter its call made.
        replay: "safe",
        async execute({ task, wait }, api, context) {
          if (wait !== true) {
            const started = await api.commit(async (tx) => {
              const call = await tx.doc(Calls, String(api.taskId), null);
              call.reporter ??= await tx.createTask(
                reporter,
                { task },
                { ownership: { kind: "conversation" }, background: true },
              );
              return call.reporter;
            }, context);
            return text(
              `Subagent ${String(started)} is working on it in the background; its answer will come to you as a message of its own.`,
            );
          }
          const id = await api.commit((tx) => childOf(tx, api.taskId, api.conversationId), context);
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
          return text(answer.text);
        },
      }),
    ],
  });
}

/**
 * The child conversation `owner` owns, created once as a copy of the
 * agent of `parent`, its conversation, that cannot start subagents or
 * schedules. It serves the parent's Carillon conversation, so tools such as
 * Discord's find its interface and origin.
 */
async function childOf(tx: Tx, owner: TaskId, parent: ConversationId): Promise<ConversationId> {
  const [existing] = (await tx.scanConversations({ ownerTaskId: owner }, 1)).items;
  if (existing !== undefined) return existing.id;
  const created = await tx.createConversation({ ownership: { kind: "task", taskId: owner } });
  await configure(tx, created.id, {
    extensions: { remove: [{ name: SUBAGENTS }, { name: SCHEDULES }] },
  });
  Object.assign(
    await tx.doc(ConversationInfo, created.id),
    copyJson(await tx.doc(ConversationInfo, parent)),
  );
  return created.id;
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

function text(value: string) {
  return { output: [{ type: "text" as const, text: value }] };
}
