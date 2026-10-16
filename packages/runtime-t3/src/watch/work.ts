import type { WorkItem } from "@carillon/protocol";
import type { Subagent } from "../contracts.ts";

export const TASK_STATE: Readonly<Record<Subagent["status"], WorkItem["state"]>> = {
  idle: "queued",
  pending: "queued",
  running: "running",
  waiting: "waiting",
  completed: "succeeded",
  failed: "failed",
  cancelled: "cancelled",
  interrupted: "cancelled",
};

/** What a transfer's native status confirms: prepared is not consumed. */
export function transferState(status: string): WorkItem["state"] {
  switch (status) {
    case "pending":
      return "running";
    case "resolved_native":
    case "resolved_portable":
      return "waiting";
    case "consumed":
      return "succeeded";
    case "failed":
      return "failed";
    case "superseded":
      return "cancelled";
    default:
      return "unknown";
  }
}
