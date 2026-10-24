/**
 * What a held turn is doing, from the agent events pi-durable derives from
 * each commit of its conversation, bounded for display. Tool inputs are
 * described by the shared runtime package's deny-by-default descriptions.
 * Steers placed into a run share its activity.
 *
 * The model's thinking, as it streams, becomes the turn's notes: each
 * thinking block one bounded note from its beginning, never the answer.
 * A call's start is when this process saw it start, on the monotonic
 * clock, so a wall clock that steps cannot make durations wrong.
 */
import type { Context } from "@earendil-works/chord";
import type { AssistantMessage, Models } from "@earendil-works/pi-ai";
import {
  watchEvents,
  type AgentEvent,
  type AgentState,
  type ConversationId,
  type Harness,
  type InboxItem,
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

export interface ActivityTracker {
  observe(event: AgentEvent): void;
  agent(info: AgentInfo): void;
  snapshot(): TurnActivity;
}

/** Tracks one run from its start until it ends. */
export function trackActivity(workspace: string): ActivityTracker {
  let phase: ActivityPhase = "model";
  let headline: string | undefined;
  let toolCalls = 0;
  const running = new Map<string, RunningTool>();
  const recent: FinishedTool[] = [];
  const idle = (): ActivityPhase => (running.size > 0 ? "tools" : "model");
  const startedMs = new Map<string, number>();
  /** Thinking blocks of the streaming message by content index: their text so far. */
  let messages = 0;
  const thinking = new Map<number, string>();
  const notes = new Map<string, string>();
  let agent: AgentInfo | undefined;

  function think(contentIndex: number, text: string): void {
    // A note shows a block's beginning, so only that much is kept while it streams.
    const kept = text.slice(0, 2 * NOTE_LENGTH);
    thinking.set(contentIndex, kept);
    keepNote(notes, `${String(messages)}:${String(contentIndex)}`, kept);
  }

  return {
    observe(event) {
      switch (event.type) {
        case "run_start":
        case "turn_start":
        case "auto_retry_end":
        case "compaction_end":
          phase = idle();
          break;
        case "auto_retry_start":
          phase = "retrying";
          break;
        case "compaction_start":
          if (event.blocking) phase = "compacting";
          break;
        case "message_start":
          messages += 1;
          thinking.clear();
          break;
        case "message_update":
          for (const change of event.changes)
            if (change.type === "thinking_delta")
              think(
                change.contentIndex,
                `${thinking.get(change.contentIndex) ?? ""}${change.delta}`,
              );
            else if (
              (change.type === "thinking_start" || change.type === "block") &&
              change.block.type === "thinking"
            )
              think(change.contentIndex, change.block.thinking);
            else if (change.type === "message")
              for (const [index, block] of change.message.content.entries())
                if (block.type === "thinking") think(index, block.thinking);
          break;
        case "message_end": {
          const message = event.entry.model?.[0];
          if (message?.role !== "assistant") break;
          for (const [index, block] of message.content.entries())
            if (block.type === "thinking") think(index, block.thinking);
          const text = message.content.flatMap((block) =>
            block.type === "text" ? [block.text] : [],
          );
          headline = commentary(text.join("\n")) ?? headline;
          break;
        }
        case "tool_execution_start": {
          toolCalls += 1;
          const detail = describe(event.toolName, event.args, workspace);
          startedMs.set(event.toolCallId, performance.now());
          running.set(event.toolCallId, {
            tool: toolName(event.toolName),
            ...(detail === undefined ? {} : { detail }),
            started_at: Math.floor(Date.now() / 1000),
          });
          phase = "tools";
          break;
        }
        case "tool_execution_end": {
          const started = running.get(event.toolCallId);
          const startedAt = startedMs.get(event.toolCallId);
          running.delete(event.toolCallId);
          startedMs.delete(event.toolCallId);
          const result = event.entry?.model?.[0];
          const failed = event.result?.isError ?? (result?.role === "toolResult" && result.isError);
          recent.push({
            tool: started?.tool ?? toolName(event.toolName),
            ...(started?.detail === undefined ? {} : { detail: started.detail }),
            ok: !failed,
            ...(startedAt === undefined
              ? {}
              : { duration_ms: Math.round(performance.now() - startedAt) }),
          });
          recent.splice(0, Math.max(0, recent.length - RECENT_CALLS));
          phase = idle();
          break;
        }
        // Ends, writes, spend and running output say nothing a card shows.
        case "snapshot":
        case "run_end":
        case "turn_end":
        case "tool_execution_update":
        case "inbox_update":
        case "submission":
        case "deferred_poll":
        case "entry_appended":
        case "agent_changed":
        case "usage_changed":
        case "task_failed":
          break;
      }
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
        tool_calls: toolCalls,
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
function waiting(
  items: readonly { readonly id: SubmissionId; readonly mode: InboxItem["mode"] }[],
): Map<SubmissionId, Waiting> {
  return new Map(items.flatMap((item) => (item.mode === "write" ? [] : [[item.id, item.mode]])));
}

/** The activity of the messages a conversation holds, kept current from its events. */
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
  const stream = await watchEvents(harness, conversationId, context);
  let queued = new Map<SubmissionId, Waiting>();
  let inputs = new Set<SubmissionId>();
  let agentState: AgentState = {};
  let usage: AssistantMessage["usage"] | undefined;
  let run: ActivityTracker | undefined;
  /** Runs that ended, by their messages, until each message is settled with core. */
  const ended = new Map<SubmissionId, ActivityTracker>();
  const calls = new Set<string>();

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
  const describeAgent = () => {
    const info = agentInfo();
    if (info !== undefined) run?.agent(info);
  };

  function apply(event: AgentEvent): void {
    switch (event.type) {
      case "snapshot":
        queued = waiting(event.inbox);
        inputs = new Set(event.run?.inputs ?? []);
        if (inputs.size > 0) run ??= trackActivity(workspace);
        agentState = event.agent;
        calls.clear();
        for (const slot of [...event.tools, ...event.nestedTools])
          if (slot.status === "running") calls.add(slot.callId);
        describeAgent();
        return;
      case "inbox_update":
        queued = waiting(event.items);
        return;
      case "run_start":
        inputs = new Set(event.inputs);
        run = trackActivity(workspace);
        describeAgent();
        break;
      case "run_end":
        for (const input of event.inputs) if (run !== undefined) ended.set(input, run);
        inputs = new Set();
        run = undefined;
        return;
      case "submission":
        if (event.record.status === "placed" && run !== undefined) inputs.add(event.record.id);
        return;
      case "agent_changed":
        agentState = event.agent;
        describeAgent();
        return;
      case "message_end": {
        const message = event.entry.model?.[0];
        if (message?.role === "assistant") usage = message.usage;
        break;
      }
      case "tool_execution_start":
        calls.add(event.toolCallId);
        break;
      case "tool_execution_end":
        calls.delete(event.toolCallId);
        break;
      case "turn_start":
      case "turn_end":
      case "message_start":
      case "message_update":
      case "tool_execution_update":
      case "auto_retry_start":
      case "auto_retry_end":
      case "deferred_poll":
      case "entry_appended":
      case "usage_changed":
      case "task_failed":
      case "compaction_start":
      case "compaction_end":
        break;
    }
    run?.observe(event);
    if (event.type === "message_end") describeAgent();
  }

  apply(stream.snapshot);
  stream.start((events) => {
    for (const event of events) apply(event);
    return Promise.resolve();
  });
  return {
    of(submission) {
      const mode = queued.get(submission);
      if (mode !== undefined) return QUEUED[mode];
      if (inputs.has(submission)) return run?.snapshot();
      return ended.get(submission)?.snapshot();
    },
    forget(submission) {
      ended.delete(submission);
    },
    get calling() {
      return calls.size > 0;
    },
    async stop() {
      await stream.stop();
    },
  };
}
