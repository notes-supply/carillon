/**
 * What an executing turn is doing, derived from native Pi session events and
 * bounded for display. Tool inputs are described by the shared runtime
 * package's deny-by-default descriptions, beside Pi's tool semantics here.
 *
 * Pi has no session event for child agents: a tool that runs other tools
 * (nested calls carry `parentToolCallId`) is still one tool call, not a
 * delegation. A participating extension that runs child agents reports
 * their lifecycle explicitly as `carillon:work` facts on the session's event
 * bus (see {@link WORK_CHANNEL}); unsupported extensions keep the ordinary
 * tool view.
 *
 * The model's thinking, as Pi streams it, becomes the turn's notes: each
 * thinking block one bounded note from its beginning, never the answer.
 *
 * A call's start is Pi's own event; reported work has no start of its own,
 * so its start is when it was first reported running, marked as an
 * estimate. Durations are measured on the monotonic clock, so a wall clock
 * that steps cannot make them wrong, and work keeps the duration it
 * finished with, however often its end is reported again.
 * What the session says about its model, thinking level and context comes
 * from the runtime through {@link ActivityTracker.agent}.
 */
import type { AgentSessionEvent } from "@earendil-works/pi-coding-agent";
import {
  NOTE_LENGTH,
  WORK_ITEM_LIMIT,
  WorkItemSchema,
  type ActivityPhase,
  type AgentInfo,
  type FinishedTool,
  type RunningTool,
  type TurnActivity,
  type WorkItem,
} from "@carillon/protocol";
import {
  commentary,
  describe,
  displayed,
  keepNote,
  summarizeWork,
  toolName,
} from "@carillon/runtime/activity";
import { Type } from "typebox";
import { Check } from "typebox/value";

/**
 * The event-bus channel on which a participating Pi extension reports the
 * work it runs for the current turn: `{ id, kind, label, state, parent?,
 * actor? }` with the shared work vocabulary, one fact per change, the same
 * `id` for the same work. Each session (and so each turn) has its own bus.
 */
export const WORK_CHANNEL = "carillon:work";

const WorkReportSchema = Type.Object({
  id: WorkItemSchema.properties.id,
  kind: WorkItemSchema.properties.kind,
  label: Type.String({ minLength: 1, maxLength: 1000 }),
  state: WorkItemSchema.properties.state,
  parent: Type.Optional(WorkItemSchema.properties.id),
  actor: Type.Optional(Type.String({ minLength: 1, maxLength: 1000 })),
});
const FINISHED_WORK: ReadonlySet<WorkItem["state"]> = new Set(["succeeded", "failed", "cancelled"]);
const WORK_LABEL_LIMIT = 100;

/** The most finished calls a snapshot lists; older calls are only counted. */
const RECENT_CALLS = 5;
/** The most running calls a snapshot lists, in the order they started. */
const RUNNING_CALLS = 8;

export interface ActivityTracker {
  observe(event: AgentSessionEvent): void;
  /** What the session reports about its agent now. */
  agent(info: AgentInfo): void;
  /**
   * One fact a participating extension reported on {@link WORK_CHANNEL}.
   * A fact outside the shared vocabulary is ignored, never guessed at.
   */
  report(fact: unknown): void;
  snapshot(): TurnActivity;
}

/** Tracks one turn from before its session exists until it settles. */
export function trackActivity(workspace: string): ActivityTracker {
  let phase: ActivityPhase = "starting";
  let headline: string | undefined;
  let toolCalls = 0;
  const running = new Map<string, RunningTool>();
  const recent: FinishedTool[] = [];
  const idle = (): ActivityPhase => (running.size > 0 ? "tools" : "model");
  /** Reported work by ID, at most the snapshot limit; never more, even when all of it is live. */
  const work = new Map<string, WorkItem>();
  /** Some reported work could not be retained. */
  let partialWork = false;
  /** When each running call and reported work started, on the monotonic clock, for its duration. */
  const startedMs = new Map<string, number>();
  const workStartedMs = new Map<string, number>();
  /** Thinking blocks by message and content index: their streamed text, and their notes. */
  let messages = 0;
  const thinking = new Map<string, string>();
  const notes = new Map<string, string>();
  let agent: AgentInfo | undefined;

  return {
    observe(event) {
      switch (event.type) {
        case "agent_start":
        case "auto_retry_end":
        case "compaction_end":
          phase = idle();
          break;
        case "auto_retry_start":
          phase = "retrying";
          break;
        case "compaction_start":
          phase = "compacting";
          break;
        case "message_start":
          messages += 1;
          break;
        case "message_update": {
          // Only completed public text; thinking is the notes'.
          const update = event.assistantMessageEvent;
          if (update.type === "text_end") headline = commentary(update.content) ?? headline;
          else if (update.type === "thinking_delta" || update.type === "thinking_end") {
            const block = `${String(messages)}:${String(update.contentIndex)}`;
            // A note shows a block's beginning, so only that much is kept while it streams.
            const text =
              update.type === "thinking_end"
                ? update.content
                : `${thinking.get(block) ?? ""}${update.delta}`.slice(0, 2 * NOTE_LENGTH);
            if (update.type === "thinking_end") thinking.delete(block);
            else thinking.set(block, text);
            keepNote(notes, block, text);
          }
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
          recent.push({
            tool: started?.tool ?? toolName(event.toolName),
            ...(started?.detail === undefined ? {} : { detail: started.detail }),
            ok: !event.isError,
            ...(startedAt === undefined
              ? {}
              : { duration_ms: Math.round(performance.now() - startedAt) }),
          });
          recent.splice(0, Math.max(0, recent.length - RECENT_CALLS));
          phase = idle();
          break;
        }
        // Starts, ends and deltas of turns and messages say nothing a card
        // shows; tool output is never described.
        case "agent_end":
        case "agent_settled":
        case "turn_start":
        case "turn_end":
        case "message_end":
        case "tool_execution_update":
        case "bash_execution_update":
        case "queue_update":
        case "entry_appended":
        case "session_info_changed":
        case "thinking_level_changed":
        case "summarization_retry_scheduled":
        case "summarization_retry_attempt_start":
        case "summarization_retry_finished":
          break;
      }
    },
    agent(info) {
      agent = info;
    },
    report(fact) {
      if (!Check(WorkReportSchema, fact)) return;
      const known = work.get(fact.id);
      if (known === undefined && work.size >= WORK_ITEM_LIMIT) {
        // Make room only by forgetting finished work, oldest first.
        const finished = [...work.values()].find((item) => FINISHED_WORK.has(item.state));
        if (finished === undefined) {
          partialWork = true;
          return;
        }
        work.delete(finished.id);
        workStartedMs.delete(finished.id);
      }
      const actor = fact.actor === undefined ? undefined : toolName(fact.actor);
      const elapsed = performance.now();
      if (fact.state === "running" && !workStartedMs.has(fact.id))
        workStartedMs.set(fact.id, elapsed);
      const startedMs = workStartedMs.get(fact.id);
      const startedAt =
        known?.started_at ?? (fact.state === "running" ? Math.floor(Date.now() / 1000) : undefined);
      // Work keeps the duration it finished with when its end is reported again.
      const duration = !FINISHED_WORK.has(fact.state)
        ? undefined
        : known !== undefined && FINISHED_WORK.has(known.state)
          ? known.duration_ms
          : startedMs === undefined
            ? undefined
            : Math.round(elapsed - startedMs);
      work.set(fact.id, {
        id: fact.id,
        kind: fact.kind,
        label: displayed(fact.label, WORK_LABEL_LIMIT) ?? fact.kind,
        state: fact.state,
        ...(fact.parent === undefined ? {} : { parent: fact.parent }),
        ...(actor === undefined ? {} : { actor }),
        ...(startedAt === undefined ? {} : { started_at: startedAt, estimated: true }),
        ...(duration === undefined ? {} : { duration_ms: duration }),
      });
    },
    snapshot() {
      return {
        phase,
        ...(headline === undefined ? {} : { headline }),
        running: [...running.values()].slice(0, RUNNING_CALLS),
        recent: [...recent],
        tool_calls: toolCalls,
        ...(work.size === 0 && !partialWork
          ? {}
          : { work: summarizeWork([...work.values()], partialWork) }),
        ...(notes.size === 0 ? {} : { notes: [...notes.values()] }),
        ...(agent === undefined ? {} : { agent }),
      };
    },
  };
}
