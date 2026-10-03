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

export const ProviderThreadSchema = Type.Object({
  id: Id,
  pendingBackgroundTasks: Type.Optional(Type.Array(BackgroundTaskSchema)),
});
export type ProviderThread = Static<typeof ProviderThreadSchema>;

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

/** A timeline item with the fields any of its kinds may carry. */
export const TurnItemSchema = Type.Object({
  id: Id,
  runId: Nullable(Id),
  type: Type.String(),
  status: ItemStatusSchema,
  ordinal: Type.Integer(),
  title: Nullable(Type.String()),
  startedAt: Nullable(Type.String()),
  completedAt: Nullable(Type.String()),
  updatedAt: Type.String(),
  text: Type.Optional(Type.String()),
  input: Type.Optional(Type.Unknown()),
  toolName: Type.Optional(Nullable(Type.String())),
  fileName: Type.Optional(Type.String()),
  pattern: Type.Optional(Type.String()),
  patterns: Type.Optional(Type.Array(Type.String())),
  requestId: Type.Optional(Type.String()),
  requestKind: Type.Optional(Type.String()),
  questions: Type.Optional(
    Type.Array(Type.Object({ header: Type.String(), question: Type.String() })),
  ),
  failure: Type.Optional(Type.Object({ class: Type.String(), message: Type.String() })),
  retry: Type.Optional(Type.Unknown()),
});
export type TurnItem = Static<typeof TurnItemSchema>;

export const ProjectionSchema = Type.Object({
  thread: Type.Object({ id: Id, projectId: Id }),
  runs: Type.Array(RunSchema),
  runtimeRequests: Type.Array(RuntimeRequestSchema),
  providerThreads: Type.Array(ProviderThreadSchema),
  messages: Type.Array(MessageSchema),
  turnItems: Type.Array(TurnItemSchema),
});
export type Projection = Static<typeof ProjectionSchema>;

/** One item of `orchestration.subscribeThread`; kinds this build does not know are ignored. */
export const ThreadStreamItemSchema = Type.Union([
  Type.Object({
    kind: Type.Literal("snapshot"),
    snapshotSequence: Type.Integer(),
    projection: ProjectionSchema,
  }),
  Type.Object({
    kind: Type.Literal("event"),
    sequence: Type.Integer(),
    event: Type.Object({ type: Type.String(), payload: Type.Unknown() }),
  }),
  Type.Object({ kind: Type.String() }),
]);
export type ThreadStreamItem = Static<typeof ThreadStreamItemSchema>;

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

function nativeSelection(selection: NativeSelection) {
  return {
    instanceId: selection.instanceId,
    model: selection.model,
    options: selection.options.map(({ id, value }) => ({ id, value })),
  };
}
