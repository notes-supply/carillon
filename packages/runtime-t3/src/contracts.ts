/**
 * The part of T3's orchestration protocol 2 this runtime consumes, checked
 * at the boundary: a response outside these shapes is a malformed answer,
 * never a guess. T3's own contracts are the authority for the full objects.
 */
import { createHash } from "node:crypto";
import { Type, type Static } from "typebox";

export const ORCHESTRATION_PROTOCOL = 2;

export const RPC = {
  dispatchCommand: "orchestration.dispatchCommand",
  subscribeThread: "orchestration.subscribeThread",
  getConfig: "server.getConfig",
  refreshProviders: "server.refreshProviders",
  createUploadUrl: "attachments.createUploadUrl",
  getSettings: "server.getSettings",
  updateSettings: "server.updateSettings",
} as const;

const Id = Type.String({ minLength: 1 });
const Nullable = <T extends ReturnType<typeof Type.String>>(schema: T) =>
  Type.Union([schema, Type.Null()]);

const RunStatusSchema = Type.Union([
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

/** A native model selection: a provider instance's model, with option values by ID. */
export const NativeSelectionSchema = Type.Object({
  instanceId: Id,
  model: Id,
  options: Type.Optional(
    Type.Array(
      Type.Object({ id: Id, value: Type.Union([Type.String({ minLength: 1 }), Type.Boolean()]) }),
    ),
  ),
});
export type NativeSelection = Static<typeof NativeSelectionSchema>;

export const RunSchema = Type.Object({
  id: Id,
  threadId: Id,
  /** T3 numbers a thread's runs from one. */
  ordinal: Type.Integer({ minimum: 1 }),
  userMessageId: Id,
  providerThreadId: Nullable(Id),
  status: RunStatusSchema,
  /** The selection the run runs with, which a later change of the thread's does not alter. */
  modelSelection: Type.Optional(NativeSelectionSchema),
  /**
   * The results of tasks this run delegated: T3 names the message of the
   * continuation it offers next (`delivery`), before it starts that run.
   */
  delegatedCompletion: Type.Optional(
    Type.Object({
      disposition: Type.String(),
      delivery: Type.Union([Type.Object({ messageId: Id }), Type.Null()]),
    }),
  ),
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

const BackgroundTaskSchema = Type.Object({
  taskId: Id,
  kind: Type.Optional(Type.String()),
  description: Type.Optional(Type.String()),
  /** A background subagent's own thread, when it has one. */
  childThreadId: Type.Optional(Id),
});
export type BackgroundTask = Static<typeof BackgroundTaskSchema>;

/** The native conversation a run executes in, with the work its process still owes. */
export const ProviderThreadSchema = Type.Object({
  id: Id,
  providerSessionId: Nullable(Id),
  pendingBackgroundTasks: Type.Optional(Type.Array(BackgroundTaskSchema)),
  /** The provider's latest report of how much of the context window the conversation fills. */
  contextUsage: Type.Optional(
    Type.Union([
      Type.Object({
        usedTokens: Type.Integer({ minimum: 0 }),
        maxTokens: Type.Optional(Type.Integer({ minimum: 1 })),
      }),
      Type.Null(),
    ]),
  ),
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

/** The `provider-session.detached` event: the only field this runtime reads is the session it names. */
export const ProviderSessionDetachedSchema = Type.Object({ providerSessionId: Id });

export const MessageSchema = Type.Object({
  id: Id,
  runId: Nullable(Id),
  role: Type.Union([Type.Literal("user"), Type.Literal("assistant"), Type.Literal("system")]),
  text: Type.String(),
  createdAt: Type.String(),
  /** A continuation T3 dispatched to report delegated results of `parentRunId`. */
  delegatedCompletion: Type.Optional(Type.Object({ parentRunId: Id })),
});
export type Message = Static<typeof MessageSchema>;

/** A child agent: T3's own delegated task (`app_owned`) or the provider's (`provider_native`). */
export const SubagentSchema = Type.Object({
  id: Id,
  /** The run that started it. */
  runId: Nullable(Id),
  parentNodeId: Id,
  origin: Type.String(),
  driver: Type.String(),
  status: Type.Union([
    Type.Literal("idle"),
    Type.Literal("pending"),
    Type.Literal("running"),
    Type.Literal("waiting"),
    Type.Literal("completed"),
    Type.Literal("failed"),
    Type.Literal("cancelled"),
    Type.Literal("interrupted"),
  ]),
  title: Nullable(Type.String()),
  model: Nullable(Type.String()),
  childThreadId: Nullable(Id),
  startedAt: Nullable(Type.String()),
  completedAt: Type.Optional(Nullable(Type.String())),
  /** Whether T3 offers the parent a continuation with the result. */
  completionWake: Type.Optional(Type.String()),
  /**
   * Where the result's delivery to the parent stands, in T3's closed set:
   * `pending` and `claimed` still owe a continuation, `delivered` gave it to
   * the parent, `acknowledged` means an agent read it through T3's task tool
   * (no continuation follows) and `disposed` dropped it. `observedByRunId`
   * names the parent thread's run that was active when the result was
   * acknowledged, if one of the same provider was; it may be another
   * message's run, and is null when none was.
   */
  completionDelivery: Type.Optional(
    Type.Object({
      state: Type.Union([
        Type.Literal("pending"),
        Type.Literal("claimed"),
        Type.Literal("acknowledged"),
        Type.Literal("delivered"),
        Type.Literal("disposed"),
      ]),
      observedByRunId: Type.Optional(Nullable(Id)),
    }),
  ),
});
export type Subagent = Static<typeof SubagentSchema>;

export const ContextTransferSchema = Type.Object({
  id: Id,
  type: Type.String(),
  status: Type.String(),
  sourceThreadId: Id,
  targetThreadId: Id,
  sourcePoint: Type.Object({ threadId: Id, runId: Type.Optional(Id) }),
  targetRunId: Nullable(Id),
  targetProviderInstanceId: Nullable(Id),
});
export type ContextTransfer = Static<typeof ContextTransferSchema>;

const ItemStatusSchema = Type.Union([
  Type.Literal("idle"),
  Type.Literal("pending"),
  Type.Literal("running"),
  Type.Literal("waiting"),
  Type.Literal("completed"),
  Type.Literal("failed"),
  Type.Literal("cancelled"),
  Type.Literal("interrupted"),
]);

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
  subagent: Type.Object({
    ...itemBase,
    type: Type.Literal("subagent"),
    /** The child agent record this item shows, when T3 tracks one. */
    subagentId: Type.Optional(Id),
  }),
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
  /** Reasoning the provider published for display, such as Claude's summarised thinking. */
  reasoning: Type.Object({ ...itemBase, type: Type.Literal("reasoning"), text: Type.String() }),
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
  TurnItemSchemas.reasoning,
]);
export type TurnItem = Static<typeof TurnItemSchema>;
export type TurnItemOf<K extends TurnItem["type"]> = Extract<TurnItem, { type: K }>;

/**
 * The notification a continuation run starts with: the delegated tasks whose
 * results it reports. Unlike the continuation's message, it is part of the
 * thread's timeline, so a snapshot shows it while the run is in the window.
 */
export const DelegatedNotificationSchema = Type.Object({
  type: Type.Literal("notification"),
  runId: Id,
  source: Type.Object({ kind: Type.Literal("delegated_task"), taskIds: Type.Array(Id) }),
});

/**
 * The notification a provider's wake run starts with: T3 dispatches the
 * wake as a message carrying the kind of background work that reported, in
 * its wire form (`background_command` for a command, `background_task` for
 * a subagent, with `work: "subagent"`, or for work it cannot name, and
 * `monitor`), or decoded (`command`, `subagent`). It never names the work.
 */
export const WakeNotificationSchema = Type.Object({
  type: Type.Literal("notification"),
  runId: Id,
  source: Type.Object({
    kind: Type.Union([
      Type.Literal("background_task"),
      Type.Literal("background_command"),
      Type.Literal("command"),
      Type.Literal("subagent"),
      Type.Literal("monitor"),
    ]),
  }),
});

/** A thread snapshot. Timeline items are checked one by one as they are folded. */
const ProjectionSchema = Type.Object({
  /** The thread's own record; its selection is what every run of the thread uses. */
  thread: Type.Object({ id: Id, modelSelection: NativeSelectionSchema }),
  runs: Type.Array(RunSchema),
  runtimeRequests: Type.Array(RuntimeRequestSchema),
  providerThreads: Type.Array(ProviderThreadSchema),
  providerSessions: Type.Array(ProviderSessionSchema),
  messages: Type.Array(MessageSchema),
  turnItems: Type.Array(Type.Unknown()),
  subagents: Type.Array(SubagentSchema),
  contextTransfers: Type.Array(ContextTransferSchema),
});
export type Projection = Static<typeof ProjectionSchema>;

/**
 * The first item of a fresh thread subscription. `hasMoreHistory` says a
 * bounded snapshot left older history out. T3 may also set
 * `payloadBudgetExceeded`, when the complete turns it keeps exceed its byte
 * budget; it leaves nothing out for that, so it is not read.
 */
export const ThreadSnapshotSchema = Type.Object({
  kind: Type.Literal("snapshot"),
  snapshotSequence: Type.Integer(),
  projection: ProjectionSchema,
  hasMoreHistory: Type.Optional(Type.Boolean()),
});

export const DispatchResultSchema = Type.Object({ sequence: Type.Integer() });

/**
 * A setting a model offers, as T3 describes it: a select among choices, or a
 * boolean. Other kinds are not offered through Carillon.
 */
export const OptionDescriptorSchema = Type.Union([
  Type.Object({
    id: Id,
    label: Type.String({ minLength: 1 }),
    type: Type.Literal("select"),
    options: Type.Array(
      Type.Object({
        id: Id,
        label: Type.String({ minLength: 1 }),
        isDefault: Type.Optional(Type.Boolean()),
      }),
    ),
    currentValue: Type.Optional(Id),
  }),
  Type.Object({
    id: Id,
    label: Type.String({ minLength: 1 }),
    type: Type.Literal("boolean"),
    currentValue: Type.Optional(Type.Boolean()),
  }),
]);
export type OptionDescriptor = Static<typeof OptionDescriptorSchema>;

/**
 * One provider instance in T3's server configuration. T3 grows its list
 * forward-compatibly, so an entry outside this shape is skipped, as T3's own
 * clients do.
 */
export const ProviderSchema = Type.Object({
  instanceId: Id,
  displayName: Type.Optional(Type.String({ minLength: 1 })),
  enabled: Type.Boolean(),
  installed: Type.Boolean(),
  status: Type.String(),
  availability: Type.Optional(Type.String()),
  models: Type.Array(
    Type.Object({
      slug: Id,
      name: Type.String({ minLength: 1 }),
      capabilities: Type.Optional(
        Type.Union([
          Type.Object({ optionDescriptors: Type.Optional(Type.Array(Type.Unknown())) }),
          Type.Null(),
        ]),
      ),
    }),
  ),
});
export type Provider = Static<typeof ProviderSchema>;

/** T3's answer to `server.getConfig` and `server.refreshProviders`: its provider instances. */
export const ServerConfigSchema = Type.Object({ providers: Type.Array(Type.Unknown()) });

/**
 * What a provider instance offers a person to invoke, machine-wide and per
 * workspace. Entries outside the shapes below are skipped.
 */
export const ProviderCommandsSchema = Type.Object({
  instanceId: Id,
  slashCommands: Type.Optional(Type.Array(Type.Unknown())),
  skills: Type.Optional(Type.Array(Type.Unknown())),
  workspaceSnapshots: Type.Optional(
    Type.Array(
      Type.Object({
        cwd: Type.String(),
        slashCommands: Type.Array(Type.Unknown()),
        skills: Type.Array(Type.Unknown()),
      }),
    ),
  ),
});

export const SlashCommandSchema = Type.Object({
  name: Id,
  description: Type.Optional(Type.String()),
  input: Type.Optional(Type.Object({ hint: Type.String() })),
});

/** A skill; `userInvocable: false` keeps it out of the person's commands. */
export const SkillSchema = Type.Object({
  name: Id,
  description: Type.Optional(Type.String()),
  enabled: Type.Boolean(),
  userInvocable: Type.Optional(Type.Boolean()),
});

/** Asks T3 for a provider instance's commands and skills in a workspace, scanning it once if T3 has not. */
export function refreshProviders(instanceId: string, cwd: string) {
  return { instanceId, cwd } as const;
}

/**
 * A stable identifier for one Carillon fact. The same facts always name the
 * same T3 thread, message or command, which is what lets a lost answer be
 * read back or replayed without a second execution.
 */
export function derivedId(...facts: readonly string[]): string {
  // The facts are encoded as a JSON array, so no fact can run into the
  // next: ("a", "b\0c") and ("a\0b", "c") hash differently, as do an
  // empty fact and an absent one.
  const hex = createHash("sha256").update(JSON.stringify(facts)).digest("hex");
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

/** An image T3 holds for a message, uploaded before the dispatch that references it. */
export interface NativeImage {
  readonly type: "image";
  readonly id: string;
  readonly name: string;
  readonly mimeType: string;
  readonly sizeBytes: number;
}

/** What one Carillon turn sends T3 as its message. */
export interface MessageContent {
  readonly text: string;
  readonly images: readonly NativeImage[];
  /**
   * The selection the message's run takes, which T3 records with the run
   * and makes the thread's own: the conversation's agent offers it.
   */
  readonly selection: NativeSelection;
  /** The person's words T3 titles a thread's first message from, as its own composer does. */
  readonly titleSeed?: string;
}

/**
 * One Carillon turn as one T3 message: the same turn always sends the same
 * command, its run on the selection it names; behind active work it waits
 * in T3's queue.
 */
export function messageDispatch(
  threadId: string,
  correlationId: string,
  content: MessageContent,
  queued: boolean,
) {
  return {
    type: "message.dispatch",
    createdBy: "user",
    creationSource: "web",
    commandId: derivedId("dispatch", correlationId),
    threadId,
    messageId: derivedId("message", correlationId),
    text: content.text,
    attachments: content.images.map(({ type, id, name, mimeType, sizeBytes }) => ({
      type,
      id,
      name,
      mimeType,
      sizeBytes,
    })),
    modelSelection: nativeSelection(content.selection),
    ...(content.titleSeed === undefined ? {} : { titleSeed: content.titleSeed }),
    dispatchMode: { type: queued ? "queue_after_active" : "start_immediately" },
  } as const;
}

/** Sets the thread's model selection, which every later run of the thread uses. */
export function modelSelectionSet(threadId: string, selection: NativeSelection) {
  return {
    type: "thread.model-selection.set",
    commandId: crypto.randomUUID(),
    threadId,
    modelSelection: nativeSelection(selection),
  } as const;
}

/** Withdraws a run still waiting in T3's queue; a queued run has nothing to interrupt yet. */
export function queuedRunCancel(threadId: string, runId: string) {
  return {
    type: "queued-run.cancel",
    commandId: crypto.randomUUID(),
    threadId,
    runId,
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
    options: (selection.options ?? []).map(({ id, value }) => ({ id, value })),
  };
}

/** The part of T3's settings a profile's instance is checked against. */
export const ServerSettingsSchema = Type.Object({
  providerInstances: Type.Optional(
    Type.Record(
      Type.String(),
      Type.Object({
        driver: Type.String(),
        config: Type.Optional(Type.Object({ homePath: Type.Optional(Type.String()) })),
      }),
    ),
  ),
});

/** A provider instance T3 creates, as its settings name it. */
export interface InstanceDefinition {
  readonly driver: string;
  readonly displayName: string;
  readonly homePath: string;
  /** The harness's environment; T3 keeps a sensitive value, such as an API key, in its secret store. */
  readonly environment: readonly {
    readonly name: string;
    readonly value: string;
    readonly sensitive: boolean;
  }[];
}

/** Creates a provider instance in T3's settings, which T3 starts without a restart. */
export function instanceCreate(instanceId: string, instance: InstanceDefinition) {
  return {
    patch: {},
    providerInstanceMutation: {
      operation: "create",
      instanceId,
      instance: {
        driver: instance.driver,
        displayName: instance.displayName,
        enabled: true,
        config: { homePath: instance.homePath },
        ...(instance.environment.length === 0
          ? {}
          : {
              environment: instance.environment.map(({ name, value, sensitive }) => ({
                name,
                value,
                sensitive,
              })),
            }),
      },
    },
  } as const;
}
