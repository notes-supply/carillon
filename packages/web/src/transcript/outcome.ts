import { finishedLine, toolCalls, type Line } from "../activity/ledger.ts";
import type { Ending, Progress } from "../api/stream.ts";
import { span } from "../time/words.ts";

export type Ended = Extract<Progress, { state: "ended" }>;
export type Outcome = { label: string; took: string; did: Line[]; unlisted: boolean };

const LABELS: Record<Ending["kind"], string> = {
  answered: "Answered",
  interrupted: "Did not finish",
  stopped: "Stopped",
  steered:
    "A later message joined this one. Their answer is below, on the newest message that took part.",
};

export function outcome(progress: Ended, acceptedAt: number): Outcome {
  const label = LABELS[progress.ending.kind];
  if (progress.ending.kind === "steered") return { label, took: "", did: [], unlisted: false };
  const calls = progress.report?.tool_calls ?? 0;
  const did = (progress.report?.recent ?? []).map(finishedLine);
  const seconds = progress.finished_at - acceptedAt;
  const took = [
    ...(seconds > 0 ? [span(seconds * 1000)] : []),
    ...(calls > 0 ? [toolCalls(calls)] : []),
  ].join(", ");
  return { label, took, did, unlisted: did.length > 0 && calls > did.length };
}
