import type { Activity, ActivityPhase, FinishedTool, Progress, WorkItem } from "../api/stream.ts";
import { span } from "../time/words.ts";

export type Mark = "past" | "done" | "failed" | "running" | "waiting";
export type Line = { mark: Mark; name: string; detail: string; time: string };
export type Ledger = {
  phase: string;
  waiting: boolean;
  tally: string;
  headline: string;
  marks: Mark[];
  counted: string;
  calls: Line[];
  work: Line[];
  notes: string[];
};
export type Working = Exclude<Progress, { state: "ended" }>;

export const LEGEND =
  "One mark for each tool call: a dot for a call no longer listed, a bar for one that finished, a short bar for one that failed, a hollow bar for one under way. The ring is the turn itself.";

const MAX_PAST_MARKS = 48;

const PHASES: Record<ActivityPhase, string> = {
  starting: "Starting",
  queued: "Waiting its turn",
  model: "Waiting for the model",
  tools: "Using tools",
  waiting: "Waiting",
  retrying: "Retrying the model request",
  compacting: "Compacting the conversation",
};

const WORK_KINDS: Record<WorkItem["kind"], string> = {
  task: "Task",
  delegation: "Delegated",
  handoff: "Handed off",
  fork: "Fork",
};

const WORK_STATES: Record<WorkItem["state"], { mark: Mark; word: string }> = {
  queued: { mark: "waiting", word: "queued" },
  running: { mark: "running", word: "running" },
  waiting: { mark: "waiting", word: "waiting" },
  succeeded: { mark: "done", word: "done" },
  failed: { mark: "failed", word: "failed" },
  cancelled: { mark: "past", word: "cancelled" },
  unknown: { mark: "past", word: "state unknown" },
};

export function toolCalls(calls: number): string {
  return `${String(calls)} tool ${calls === 1 ? "call" : "calls"}`;
}

function timed(word: string, millis: number | undefined): string {
  return [word, millis === undefined ? "" : span(millis)].filter((part) => part !== "").join(", ");
}

export function finishedLine(call: FinishedTool): Line {
  return {
    mark: call.ok ? "done" : "failed",
    name: call.tool,
    detail: call.detail ?? "",
    time: timed(call.ok ? "" : "failed", call.duration_ms),
  };
}

function workLine(item: WorkItem, now: number): Line {
  const { mark, word } = WORK_STATES[item.state];
  const since = mark === "running" ? item.started_at : undefined;
  return {
    mark,
    name: WORK_KINDS[item.kind],
    detail: item.actor === undefined ? item.label : `${item.label} (${item.actor})`,
    time: timed(word, since === undefined ? item.duration_ms : (now - since) * 1000),
  };
}

function counted(calls: number, marks: Mark[]): string {
  const words = [
    { mark: "running", word: "under way" },
    { mark: "failed", word: "failed" },
    { mark: "done", word: "finished" },
  ] as const;
  const parts = words.flatMap(({ mark, word }) => {
    const count = marks.filter((each) => each === mark).length;
    return count === 0 ? [] : [`${String(count)} ${word}`];
  });
  return [toolCalls(calls), ...parts].join(", ");
}

function notesOf(activity: Activity | null): string[] {
  const notes: string[] = [];
  const work = activity?.work;
  if (work?.omitted !== undefined) notes.push(`${String(work.omitted)} more not shown.`);
  if (work?.partial === true) notes.push("The runtime could not see all of the delegated work.");
  if (activity?.partial === true) notes.push("Part of this report was left out for its size.");
  const agent = activity?.agent;
  const context = agent?.context;
  const filled =
    context?.window === undefined
      ? undefined
      : `context ${String(Math.round((context.used / context.window) * 100))}% full`;
  const model = [agent?.model, agent?.effort].filter((part) => part !== undefined).join(" ");
  const said = [model, filled].filter((part) => part !== undefined && part !== "").join(", ");
  if (said !== "") notes.push(said);
  return notes;
}

export function ledger(progress: Working, acceptedAt: number, now: number): Ledger {
  const activity = progress.state === "running" ? progress.activity : null;
  const calls = activity?.tool_calls ?? 0;
  const finished = (activity?.recent ?? []).map(finishedLine);
  const underWay = (activity?.running ?? []).map((call): Line => ({
    mark: "running",
    name: call.tool,
    detail: call.detail ?? "",
    time: `${call.estimated === true ? "about " : ""}${span((now - call.started_at) * 1000)}`,
  }));
  const listed = [...finished, ...underWay];
  const past = Math.min(Math.max(calls - listed.length, 0), MAX_PAST_MARKS);
  const marks = [...Array<Mark>(past).fill("past"), ...listed.map((line) => line.mark)];
  const waiting = progress.state === "queued" || activity?.phase === "queued";
  const elapsed = span((now - acceptedAt) * 1000);
  return {
    phase: waiting ? PHASES.queued : activity === null ? "At work" : PHASES[activity.phase],
    waiting,
    tally: calls > 0 ? `${toolCalls(calls)}, ${elapsed}` : elapsed,
    headline: activity?.headline ?? activity?.notes?.at(-1) ?? "",
    marks,
    counted: counted(calls, marks),
    calls: listed,
    work: (activity?.work?.items ?? []).map((item) => workLine(item, now)),
    notes: notesOf(activity),
  };
}
