/**
 * What a held turn is doing, from pi-durable's view of its conversation,
 * bounded for display. Tool inputs are described by the shared runtime
 * package's deny-by-default descriptions. Steers placed into a run share its
 * activity.
 *
 * The model's thinking, as it streams, becomes the turn's notes: each
 * thinking block one bounded note from its beginning, never the answer.
 */
import type { Context } from "@earendil-works/chord";
import type { AssistantMessage, Message, Models } from "@earendil-works/pi-ai";
import {
  AgentDoc,
  InboxDoc,
  LiveDoc,
  type AgentState,
  type ConversationId,
  type ConversationView,
  type EntryId,
  type Harness,
  type InboxItem,
  type InboxState,
  type LiveState,
  type SubmissionId,
} from "@earendil-works/pi-durable";
import {
  NOTE_LENGTH,
  type ActivityPhase,
  type AgentInfo,
  type FinishedTool,
  type RunningTool,
  type TurnActivity,
} from "@carillon/protocol";
import { commentary, describe, keepNote, toolName } from "@carillon/runtime/activity";

/** The most finished calls a snapshot lists; older calls are only counted. */
const RECENT_CALLS = 5;
/** The most running calls a snapshot lists, in the order they started. */
const RUNNING_CALLS = 8;
/** The most of a thinking block a note reads: a note shows only its beginning. */
const THINKING_READ = 2 * NOTE_LENGTH;

export interface ActivityTracker {
  /** The run's live state now, with the messages appended since the last update. */
  update(live: LiveState, appended: readonly Message[]): void;
  agent(info: AgentInfo): void;
  snapshot(): TurnActivity;
}

type Call = Pick<RunningTool, "tool" | "detail">;

/** Tracks one run from its start until it ends. */
export function trackActivity(workspace: string): ActivityTracker {
  let phase: ActivityPhase = "model";
  let headline: string | undefined;
  const requested = new Map<string, Call>();
  const running = new Map<string, RunningTool>();
  const finished = new Set<string>();
  const recent: FinishedTool[] = [];
  let answers = 0;
  const notes = new Map<string, string>();
  let agent: AgentInfo | undefined;

  const call = (name: string, args: unknown): Call => {
    const detail = describe(name, args, workspace);
    return { tool: toolName(name), ...(detail === undefined ? {} : { detail }) };
  };

  function think(content: readonly AssistantMessage["content"][number][]): void {
    for (const [index, block] of content.entries())
      if (block.type === "thinking")
        keepNote(
          notes,
          `${String(answers)}:${String(index)}`,
          block.thinking.slice(0, THINKING_READ),
        );
  }

  function finish(callId: string, name: string, ok: boolean, durationMs?: number): void {
    if (finished.has(callId)) return;
    finished.add(callId);
    const about = running.get(callId) ?? requested.get(callId) ?? { tool: toolName(name) };
    running.delete(callId);
    requested.delete(callId);
    recent.push({
      tool: about.tool,
      ...(about.detail === undefined ? {} : { detail: about.detail }),
      ok,
      ...(durationMs === undefined ? {} : { duration_ms: Math.round(durationMs) }),
    });
    recent.splice(0, Math.max(0, recent.length - RECENT_CALLS));
  }

  return {
    update(live, appended) {
      for (const message of appended) {
        if (message.role === "assistant") {
          think(message.content);
          const text = message.content.flatMap((block) =>
            block.type === "text" ? [block.text] : [],
          );
          headline = commentary(text.join("\n")) ?? headline;
          for (const block of message.content)
            if (block.type === "toolCall")
              requested.set(block.id, call(block.name, block.arguments));
          answers += 1;
        } else if (message.role === "toolResult")
          finish(message.toolCallId, message.toolName, !message.isError, message.durationMs);
      }
      think(live.generation?.message?.content ?? []);

      const slots = [...(live.tools ?? []), ...(live.nestedTools ?? [])];
      for (const slot of slots) {
        if (slot.status === "running" && !running.has(slot.callId) && !finished.has(slot.callId)) {
          const about =
            "parentCallId" in slot
              ? call(slot.name, slot.arguments)
              : (requested.get(slot.callId) ?? { tool: toolName(slot.name) });
          running.set(slot.callId, { ...about, started_at: Math.floor(Date.now() / 1000) });
        } else if (slot.status === "done" && running.has(slot.callId)) {
          const summary = "summary" in slot ? slot.summary : undefined;
          finish(slot.callId, slot.name, summary?.isError === false, summary?.durationMs);
        }
      }
      for (const [callId, { tool }] of running)
        if (!slots.some((slot) => slot.callId === callId)) finish(callId, tool, false);

      phase =
        live.generation?.retry !== undefined
          ? "retrying"
          : live.compactions?.some((compaction) => compaction.blocking) === true
            ? "compacting"
            : running.size > 0
              ? "tools"
              : "model";
    },
    agent(info) {
      agent = info;
    },
    snapshot() {
      return {
        phase,
        ...(headline === undefined ? {} : { headline }),
        running: [...running.values()].slice(0, RUNNING_CALLS),
        recent: [...recent],
        tool_calls: finished.size + running.size,
        ...(notes.size === 0 ? {} : { notes: [...notes.values()] }),
        ...(agent === undefined ? {} : { agent }),
      };
    },
  };
}

/** A message waiting behind a running answer: a follow-up waits for it to end, a steer for its next tool round. */
type Waiting = "followUp" | "steer";

/** What a person may do with a waiting message; a steer already joins the running answer. */
const QUEUED: Record<Waiting, TurnActivity> = {
  followUp: {
    phase: "queued",
    running: [],
    recent: [],
    tool_calls: 0,
    controls: ["steer", "interrupt", "stop"],
  },
  steer: {
    phase: "queued",
    running: [],
    recent: [],
    tool_calls: 0,
    controls: ["interrupt", "stop"],
  },
};

/** The waiting messages of an inbox, without its writes. */
function waiting(items: readonly InboxItem[]): Map<SubmissionId, Waiting> {
  return new Map(items.flatMap((item) => (item.mode === "write" ? [] : [[item.id, item.mode]])));
}

/** The activity of the messages a conversation holds, kept current from its view. */
export interface ConversationActivity {
  /** A held message's activity: waiting in the inbox, or what its run does. */
  of(submission: SubmissionId): TurnActivity | undefined;
  /** The message is settled with core and needs no more watching. */
  forget(submission: SubmissionId): void;
  /** Whether a tool call runs now. */
  readonly calling: boolean;
  stop(): Promise<void>;
}

export async function watchActivity(
  harness: Harness,
  conversationId: ConversationId,
  workspace: string,
  models: Models,
  context: Context,
): Promise<ConversationActivity> {
  const conversation = await harness.conversation(conversationId, context);
  if (conversation === undefined) throw new Error(`conversation ${conversationId} does not exist`);
  const watch = await conversation.watch(context);
  let queued = new Map<SubmissionId, Waiting>();
  let inputs: readonly SubmissionId[] = [];
  let agentState: AgentState = {};
  let usage: AssistantMessage["usage"] | undefined;
  let run: ActivityTracker | undefined;
  /** Runs that ended, by their messages, until each message is settled with core. */
  const ended = new Map<SubmissionId, ActivityTracker>();
  let calling = false;
  let newest: EntryId | undefined = watch.value.entries.at(-1)?.id;

  const agentInfo = (): AgentInfo | undefined => {
    const ref = agentState.model;
    if (ref === undefined) return undefined;
    const model = models.getModel(ref.provider, ref.modelId);
    return {
      model: toolName(ref.modelId),
      ...(model?.reasoning === true && agentState.thinkingLevel !== undefined
        ? { effort: agentState.thinkingLevel }
        : {}),
      ...(usage === undefined
        ? {}
        : {
            context: {
              used: usage.totalTokens,
              ...(model === undefined ? {} : { window: model.contextWindow }),
            },
          }),
    };
  };

  function apply(view: ConversationView, appended: readonly Message[]): void {
    const live = (view.docs[LiveDoc.definition.kind] ?? {}) as LiveState;
    const inbox = (view.docs[InboxDoc.definition.kind] ?? {}) as Partial<InboxState>;
    agentState = (view.docs[AgentDoc.definition.kind] ?? {}) as AgentState;
    queued = waiting(inbox.items ?? []);
    for (const message of appended) if (message.role === "assistant") usage = message.usage;
    const first = live.run?.inputs[0];
    if (run !== undefined && first === inputs[0]) run.update(live, appended);
    else {
      const before = run;
      before?.update({}, appended);
      if (before !== undefined) for (const input of inputs) ended.set(input, before);
      run = first === undefined ? undefined : trackActivity(workspace);
      run?.update(live, before === undefined ? appended : []);
    }
    inputs = live.run?.inputs ?? [];
    const info = agentInfo();
    if (info !== undefined) run?.agent(info);
    calling = [...(live.tools ?? []), ...(live.nestedTools ?? [])].some(
      (slot) => slot.status === "running",
    );
  }

  apply(watch.value, []);
  watch.start((call) => {
    if (call.kind === "value") {
      const after = newest;
      const appended = call.value.entries.filter(
        (entry) => after === undefined || entry.id > after,
      );
      newest = call.value.entries.at(-1)?.id ?? newest;
      apply(
        call.value,
        appended.flatMap((entry) => entry.model ?? []),
      );
    }
    return Promise.resolve();
  });
  return {
    of(submission) {
      const mode = queued.get(submission);
      if (mode !== undefined) return QUEUED[mode];
      if (inputs.includes(submission)) return run?.snapshot();
      return ended.get(submission)?.snapshot();
    },
    forget(submission) {
      ended.delete(submission);
    },
    get calling() {
      return calling;
    },
    async stop() {
      await watch.stop();
    },
  };
}
