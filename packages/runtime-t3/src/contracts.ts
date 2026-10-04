/**
 * The part of T3's orchestration protocol 2 this runtime consumes, checked
 * at the boundary: a response outside these shapes is a malformed answer,
 * never a guess. T3's own contracts are the authority for the full objects.
 */
import { createHash } from "node:crypto";
import { Type, type Static } from "typebox";

/** The orchestration protocol version this runtime speaks and requires. */
export const ORCHESTRATION_PROTOCOL = 2;

export const RPC = {
  dispatchCommand: "orchestration.dispatchCommand",
  subscribeThread: "orchestration.subscribeThread",
} as const;

const Id = Type.String({ minLength: 1 });
const Nullable = <T extends ReturnType<typeof Type.String>>(schema: T) =>
  Type.Union([schema, Type.Null()]);

export const RunStatusSchema = Type.Union([
  Type.Literal("preparing"),
  Type.Literal("queued"),
  Type.Literal("starting"),
  Type.Literal("running"),
  Type.Literal("waiting"),
  Type.Literal("completed"),
  Type.Literal("interrupted"),
  Type.Literal("failed"),
  Type.Literal("cancelled"),
  Type.Literal("rolled_back"),
]);
export type RunStatus = Static<typeof RunStatusSchema>;

export const RunSchema = Type.Object({
  id: Id,
  threadId: Id,
  ordinal: Type.Integer(),
  userMessageId: Id,
  providerThreadId: Nullable(Id),
  status: RunStatusSchema,
});
export type Run = Static<typeof RunSchema>;

export const RuntimeRequestSchema = Type.Object({
  id: Id,
  kind: Type.String(),
  status: Type.Union([
    Type.Literal("pending"),
    Type.Literal("resolved"),
    Type.Literal("expired"),
    Type.Literal("cancelled"),
  ]),
});
export type RuntimeRequest = Static<typeof RuntimeRequestSchema>;

export const BackgroundTaskSchema = Type.Object({
  taskId: Id,
  kind: Type.Optional(Type.String()),
  description: Type.Optional(Type.String()),
});
export type BackgroundTask = Static<typeof BackgroundTaskSchema>;

/** The native conversation a run executes in, with the work its process still owes. */
export const ProviderThreadSchema = Type.Object({
  id: Id,
  providerSessionId: Nullable(Id),
  pendingBackgroundTasks: Type.Optional(Type.Array(BackgroundTaskSchema)),
});
export type ProviderThread = Static<typeof ProviderThreadSchema>;

/** The provider process behind a provider thread; only a live one can report or wake. */
export const ProviderSessionSchema = Type.Object({
  id: Id,
  status: Type.Union([
    Type.Literal("starting"),
    Type.Literal("ready"),
    Type.Literal("running"),
    Type.Literal("waiting"),
    Type.Literal("stopped"),
    Type.Literal("error"),
  ]),
});
export type ProviderSession = Static<typeof ProviderSessionSchema>;

export const MessageSchema = Type.Object({
  id: Id,
  runId: Nullable(Id),
  role: Type.Union([Type.Literal("user"), Type.Literal("assistant"), Type.Literal("system")]),
  text: Type.String(),
  createdAt: Type.String(),
});
export type Message = Static<typeof MessageSchema>;

export const ItemStatusSchema = Type.Union([
  Type.Literal("idle"),
  Type.Literal("pending"),
  Type.Literal("running"),
  Type.Literal("waiting"),
  Type.Literal("completed"),
  Type.Literal("failed"),
  Type.Literal("cancelled"),
  Type.Literal("interrupted"),
]);
export type ItemStatus = Static<typeof ItemStatusSchema>;

const itemBase = {
  id: Id,
  runId: Nullable(Id),
  status: ItemStatusSchema,
  ordinal: Type.Integer(),
  title: Nullable(Type.String()),
  startedAt: Nullable(Type.String()),
  completedAt: Nullable(Type.String()),
  updatedAt: Type.String(),
};

/**
 * The timeline kinds this runtime reads, each with the fields it consumes
 * required. A known kind missing them is malformed; kinds not listed here
 * are ignored, not retained.
 */
export const TurnItemSchemas = {
  command_execution: Type.Object({
    ...itemBase,
    type: Type.Literal("command_execution"),
    input: Type.String(),
  }),
  file_change: Type.Object({
    ...itemBase,
    type: Type.Literal("file_change"),
    fileName: Type.String(),
  }),
  file_search: Type.Object({
    ...itemBase,
    type: Type.Literal("file_search"),
    pattern: Type.Optional(Type.String()),
  }),
  web_search: Type.Object({
    ...itemBase,
    type: Type.Literal("web_search"),
    patterns: Type.Optional(Type.Array(Type.String())),
  }),
  dynamic_tool: Type.Object({
    ...itemBase,
    type: Type.Literal("dynamic_tool"),
    toolName: Nullable(Type.String()),
    input: Type.Unknown(),
  }),
  subagent: Type.Object({ ...itemBase, type: Type.Literal("subagent") }),
  user_input_request: Type.Object({
    ...itemBase,
    type: Type.Literal("user_input_request"),
    requestId: Id,
    questions: Type.Array(Type.Object({ header: Type.String(), question: Type.String() })),
  }),
  approval_request: Type.Object({
    ...itemBase,
    type: Type.Literal("approval_request"),
    requestId: Id,
    requestKind: Type.String(),
  }),
  error: Type.Object({
    ...itemBase,
    type: Type.Literal("error"),
    failure: Type.Object({ class: Type.String(), message: Type.String() }),
    retry: Type.Optional(Type.Unknown()),
  }),
  compaction: Type.Object({ ...itemBase, type: Type.Literal("compaction") }),
} as const;

export const TurnItemSchema = Type.Union([
  TurnItemSchemas.command_execution,
  TurnItemSchemas.file_change,
  TurnItemSchemas.file_search,
  TurnItemSchemas.web_search,
  TurnItemSchemas.dynamic_tool,
  TurnItemSchemas.subagent,
  TurnItemSchemas.user_input_request,
  TurnItemSchemas.approval_request,
  TurnItemSchemas.error,
  TurnItemSchemas.compaction,
]);
export type TurnItem = Static<typeof TurnItemSchema>;
export type TurnItemOf<K extends TurnItem["type"]> = Extract<TurnItem, { type: K }>;

/** A thread snapshot. Timeline items are checked one by one as they are folded. */
export const ProjectionSchema = Type.Object({
  thread: Type.Object({ id: Id }),
  runs: Type.Array(RunSchema),
  runtimeRequests: Type.Array(RuntimeRequestSchema),
  providerThreads: Type.Array(ProviderThreadSchema),
  providerSessions: Type.Array(ProviderSessionSchema),
  messages: Type.Array(MessageSchema),
  turnItems: Type.Array(Type.Unknown()),
});
export type Projection = Static<typeof ProjectionSchema>;

export const DispatchResultSchema = Type.Object({ sequence: Type.Integer() });

/** A native model selection as T3 names it, preserved from configuration. */
export interface NativeSelection {
  readonly instanceId: string;
  readonly model: string;
  readonly options: readonly { readonly id: string; readonly value: string }[];
}

/**
 * A stable identifier for one Carillon fact. The same facts always name the
 * same T3 thread, message or command, which is what lets a lost answer be
 * read back or replayed without a second execution.
 */
export function derivedId(...facts: readonly string[]): string {
  const hash = createHash("sha256");
  for (const fact of facts) hash.update(fact).update("\0");
  const hex = hash.digest("hex");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20, 32)}`;
}

export interface ThreadBinding {
  readonly threadId: string;
  readonly projectId: string;
  readonly title: string;
}

/** Opens the conversation's thread; replayed by its receipt when it already exists. */
export function threadCreate(binding: ThreadBinding, selection: NativeSelection) {
  return {
    type: "thread.create",
    createdBy: "user",
    creationSource: "web",
    commandId: derivedId("create", binding.threadId),
    threadId: binding.threadId,
    projectId: binding.projectId,
    title: binding.title,
    modelSelection: nativeSelection(selection),
    runtimeMode: "full-access",
    interactionMode: "default",
    branch: null,
    worktreePath: null,
  } as const;
}

/** One Carillon turn as one T3 message: the same turn always sends the same command. */
export function messageDispatch(
  threadId: string,
  correlationId: string,
  text: string,
  selection: NativeSelection,
) {
  return {
    type: "message.dispatch",
    createdBy: "user",
    creationSource: "web",
    commandId: derivedId("dispatch", correlationId),
    threadId,
    messageId: derivedId("message", correlationId),
    text,
    attachments: [],
    modelSelection: nativeSelection(selection),
    dispatchMode: { type: "start_immediately" },
  } as const;
}

export function runInterrupt(threadId: string, runId: string, reason: string) {
  return {
    type: "run.interrupt",
    commandId: crypto.randomUUID(),
    threadId,
    runId,
    reason,
  } as const;
}

/**
 * Asks T3 to detach the provider session behind a thread's native
 * conversation: the only public stop for a process that owes a wake with
 * nothing running. T3 accepts the request and removes the session's record
 * before the process closes, without confirming the closure or fencing a
 * wake the process may still deliver, so acceptance is a receipt, not a
 * confirmed stop; the runtime reports such a stop as unconfirmed.
 */
export function sessionDetach(threadId: string, providerSessionId: string, reason: string) {
  return {
    type: "provider-session.detach",
    commandId: crypto.randomUUID(),
    threadId,
    providerSessionId,
    reason,
  } as const;
}

/**
 * The thread subscription: a bounded snapshot (T3 windows messages and
 * items to recent turns; runs, requests, provider threads and sessions stay
 * whole), or a replay of the events after a sequence this runtime already
 * folded, each ended by a completion marker.
 */
export function subscribeThread(threadId: string, afterSequence: number | undefined) {
  return {
    threadId,
    acceptBoundedSnapshot: true,
    requestCompletionMarker: true,
    ...(afterSequence === undefined ? {} : { afterSequence }),
  } as const;
}

function nativeSelection(selection: NativeSelection) {
  return {
    instanceId: selection.instanceId,
    model: selection.model,
    options: selection.options.map(({ id, value }) => ({ id, value })),
  };
}
