/** Synthetic projection fixtures; shapes follow the live server's protocol 2 answers. */
export const run = (
  status: string,
  userMessageId = "message-1",
  ordinal = 1,
  id = `run-${String(ordinal)}`,
) => ({
  id,
  threadId: "thread",
  ordinal,
  userMessageId,
  providerThreadId: "provider-thread",
  status,
});

export const item = (
  type: string,
  runId: string,
  fields: Record<string, unknown>,
  status = "completed",
) => ({
  id: typeof fields["id"] === "string" ? fields["id"] : `${type}-${runId}`,
  runId,
  type,
  status,
  ordinal: 0,
  title: null,
  startedAt: "2026-10-04T08:00:00Z",
  completedAt: status === "completed" ? "2026-10-04T08:00:01Z" : null,
  updatedAt: "2026-10-04T08:00:01Z",
  ...fields,
});

/** The item T3 opens a provider's wake run with: the notification of its background work. */
export const wakeOpening = (runId: string) =>
  item("notification", runId, {
    source: { kind: "background_task" },
    outcome: "updated",
    summary: "Background activity updated",
  });

export function snapshot(overrides: Record<string, unknown>, sequence = 1) {
  return {
    kind: "snapshot",
    snapshotSequence: sequence,
    projection: {
      thread: {
        id: "thread",
        projectId: "project",
        modelSelection: { instanceId: "claudeAgent", model: "claude-sonnet-5" },
      },
      runs: [],
      runtimeRequests: [],
      providerThreads: [{ id: "provider-thread", providerSessionId: "session" }],
      providerSessions: [{ id: "session", status: "ready" }],
      messages: [],
      turnItems: [],
      subagents: [],
      contextTransfers: [],
      ...overrides,
    },
  };
}

export const event = (sequence: number, type: string, payload: unknown) => ({
  kind: "event",
  sequence,
  event: { type, payload },
});
