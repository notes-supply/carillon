/** Shared wire schemas. Runtime-specific option values remain runtime-owned. */
import { Type, type Static } from "typebox";

export const SelectionSchema = Type.Object({
  provider: Type.String({ minLength: 1 }),
  model: Type.String({ minLength: 1 }),
  options: Type.Optional(Type.Record(Type.String(), Type.String())),
});
export type Selection = Static<typeof SelectionSchema>;

/** A setting the model supports; `default` applies when no scope chose a supported value. */
export const ModelOptionSchema = Type.Object({
  id: Type.String(),
  name: Type.String(),
  choices: Type.Array(Type.String()),
  default: Type.String(),
});
export type ModelOption = Static<typeof ModelOptionSchema>;
/** A provider of the runtime's models, as the runtime names it; `id` is what models' `provider` names. */
export const ModelProviderSchema = Type.Object({ id: Type.String(), name: Type.String() });
export type ModelProvider = Static<typeof ModelProviderSchema>;
export const AvailableModelSchema = Type.Object({
  provider: Type.String(),
  model: Type.String(),
  name: Type.String(),
  options: Type.Array(ModelOptionSchema),
});
export type AvailableModel = Static<typeof AvailableModelSchema>;
export const ScopeStateSchema = Type.Object({
  selection: Type.Union([SelectionSchema, Type.Null()]),
  overridden: Type.Boolean(),
  /** The runtime does not change this scope through the model API. */
  read_only: Type.Optional(Type.Boolean()),
});
export type ScopeState = Static<typeof ScopeStateSchema>;
export const ModelStateSchema = Type.Object({
  providers: Type.Array(ModelProviderSchema),
  available: Type.Array(AvailableModelSchema),
  agent: ScopeStateSchema,
  conversation: Type.Optional(ScopeStateSchema),
});
export type ModelState = Static<typeof ModelStateSchema>;

export const ModelQuerySchema = Type.Object({
  interface_id: Type.String({ minLength: 1 }),
  agent_id: Type.String({ minLength: 1 }),
  audience_id: Type.Optional(Type.String({ minLength: 1 })),
});
export type ModelQuery = Static<typeof ModelQuerySchema>;

export const ModelUpdateSchema = Type.Object({
  ...ModelQuerySchema.properties,
  scope: Type.Union([Type.Literal("agent"), Type.Literal("conversation")]),
  selection: Type.Union([SelectionSchema, Type.Null()]),
  expected: Type.Optional(SelectionSchema),
});
export type ModelUpdate = Static<typeof ModelUpdateSchema>;

/**
 * One of the runtime's own commands (or user-invocable skills), as its
 * engine names and describes it. `hint` describes the arguments the engine
 * expects, when it says.
 */
export const RuntimeCommandSchema = Type.Object({
  name: Type.String({ minLength: 1, maxLength: 100 }),
  description: Type.Optional(Type.String({ minLength: 1 })),
  hint: Type.Optional(Type.String({ minLength: 1 })),
});
export type RuntimeCommand = Static<typeof RuntimeCommandSchema>;
/** The commands a person may run in a conversation, in the runtime's order. */
export const CommandCatalogSchema = Type.Object({
  commands: Type.Array(RuntimeCommandSchema),
});
export type CommandCatalog = Static<typeof CommandCatalogSchema>;

/**
 * A turn's `native` metadata belongs to the interface that submitted it,
 * apart from one neutral key: `intent: "command"`, set by an interface
 * whose person invoked one of the runtime's own commands rather than
 * writing a message.
 */
export function turnIntent(native: unknown): "command" | undefined {
  return isRecord(native) && native["intent"] === "command" ? "command" : undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * What a runtime is doing in one executing turn, for display while it runs.
 * The runtime formats each `detail` from its own tool semantics; consumers
 * show it as given and never receive raw tool arguments.
 */
export const ActivityPhaseSchema = Type.Union([
  Type.Literal("starting"),
  /** The runtime accepted the turn, which waits behind other work on its native conversation. */
  Type.Literal("queued"),
  Type.Literal("model"),
  Type.Literal("tools"),
  /** The turn's native work waits on something outside the model, such as a native request. */
  Type.Literal("waiting"),
  Type.Literal("retrying"),
  Type.Literal("compacting"),
]);
export type ActivityPhase = Static<typeof ActivityPhaseSchema>;
export const RunningToolSchema = Type.Object({
  tool: Type.String({ minLength: 1 }),
  detail: Type.Optional(Type.String({ minLength: 1 })),
  /** Unix seconds. */
  started_at: Type.Integer(),
  /** `started_at` is when the runtime first saw the call: the engine gave no start of its own. */
  estimated: Type.Optional(Type.Boolean()),
});
export type RunningTool = Static<typeof RunningToolSchema>;
export const FinishedToolSchema = Type.Object({
  tool: Type.String({ minLength: 1 }),
  detail: Type.Optional(Type.String({ minLength: 1 })),
  ok: Type.Boolean(),
  /** How long the call ran, when the engine reported its start and end. */
  duration_ms: Type.Optional(Type.Integer({ minimum: 0 })),
});
export type FinishedTool = Static<typeof FinishedToolSchema>;
/** The most work items one snapshot lists. */
export const WORK_ITEM_LIMIT = 16;
/**
 * One unit of native work related to a turn that is not a tool call: a
 * child agent the turn delegated to (`delegation`), a transfer of the
 * conversation's context to another model or provider (`handoff`), a branch
 * into another conversation (`fork`), or other tracked work (`task`). The
 * runtime keeps `id` stable while it observes the work and states only what
 * the native engine last confirmed; `unknown` when it can no longer confirm.
 * `label` and `actor` are bounded display text, never prompts, results or
 * arguments; `parent` names another listed item the work was observed under.
 */
export const WorkItemSchema = Type.Object({
  id: Type.String({ minLength: 1, maxLength: 256 }),
  kind: Type.Union([
    Type.Literal("task"),
    Type.Literal("delegation"),
    Type.Literal("handoff"),
    Type.Literal("fork"),
  ]),
  label: Type.String({ minLength: 1, maxLength: 200 }),
  state: Type.Union([
    Type.Literal("queued"),
    Type.Literal("running"),
    Type.Literal("waiting"),
    Type.Literal("succeeded"),
    Type.Literal("failed"),
    Type.Literal("cancelled"),
    Type.Literal("unknown"),
  ]),
  parent: Type.Optional(Type.String({ minLength: 1, maxLength: 256 })),
  actor: Type.Optional(Type.String({ minLength: 1, maxLength: 200 })),
  /** Unix seconds. */
  started_at: Type.Optional(Type.Integer()),
  /** `started_at` is when the runtime first saw the work running: the engine gave no start of its own. */
  estimated: Type.Optional(Type.Boolean()),
  /** How long finished work ran, when the engine reported its start and end. */
  duration_ms: Type.Optional(Type.Integer({ minimum: 0 })),
});
export type WorkItem = Static<typeof WorkItemSchema>;
/**
 * The turn's related work, bounded. `partial` says the runtime could not
 * observe all of it (more than it retains, or coverage it cannot confirm);
 * `omitted` is an exact count of further current items when it knows one.
 */
export const WorkSummarySchema = Type.Object({
  items: Type.Array(WorkItemSchema, { maxItems: WORK_ITEM_LIMIT }),
  partial: Type.Boolean(),
  omitted: Type.Optional(Type.Integer({ minimum: 1 })),
});
export type WorkSummary = Static<typeof WorkSummarySchema>;
/** The most reasoning notes one snapshot carries. */
export const NOTE_LIMIT = 8;
/** The longest note, in UTF-16 code units. */
export const NOTE_LENGTH = 500;
/** The most bytes a turn's final report takes, serialized as JSON. */
export const ACTIVITY_REPORT_LIMIT = 16 * 1024;
/** How much of its context window an agent's conversation fills, as the engine last estimated it. */
export const ContextUseSchema = Type.Object({
  /** Tokens the conversation occupies. */
  used: Type.Integer({ minimum: 0 }),
  /** The window's size, when the engine says. */
  window: Type.Optional(Type.Integer({ minimum: 0 })),
});
export type ContextUse = Static<typeof ContextUseSchema>;
/** What a native engine reports about an agent; each part is absent when the engine does not say. */
export const AgentInfoSchema = Type.Object({
  /** The model, as the engine names it. */
  model: Type.Optional(Type.String({ minLength: 1, maxLength: 100 })),
  /** The reasoning effort or thinking level, as the engine names it. */
  effort: Type.Optional(Type.String({ minLength: 1, maxLength: 100 })),
  context: Type.Optional(ContextUseSchema),
});
export type AgentInfo = Static<typeof AgentInfoSchema>;
export const TurnActivitySchema = Type.Object({
  phase: ActivityPhaseSchema,
  /** The model's own latest public commentary, when it wrote any. */
  headline: Type.Optional(Type.String({ minLength: 1 })),
  running: Type.Array(RunningToolSchema),
  /** The most recently finished calls, oldest first. */
  recent: Type.Array(FinishedToolSchema),
  /** Every call started in the turn, including those no longer listed. */
  tool_calls: Type.Integer({ minimum: 0 }),
  /** Delegated and transferred work, when the runtime observes any. */
  work: Type.Optional(WorkSummarySchema),
  /**
   * What the native provider published about its reasoning, as bounded
   * excerpts, oldest first: never the answer, and never reasoning the
   * provider did not expose.
   */
  notes: Type.Optional(
    Type.Array(Type.String({ minLength: 1, maxLength: NOTE_LENGTH }), { maxItems: NOTE_LIMIT }),
  ),
  /** What the native engine reports about the agent running the turn. */
  agent: Type.Optional(AgentInfoSchema),
  /**
   * Part of this report was left out to keep it within
   * {@link ACTIVITY_REPORT_LIMIT}: what it shows is still what the runtime
   * observed, but not all of it.
   */
  partial: Type.Optional(Type.Boolean()),
});
export type TurnActivity = Static<typeof TurnActivitySchema>;

export const ProviderIdentitySchema = Type.String({
  minLength: 1,
  maxLength: 128,
  pattern: "^[A-Za-z0-9_-]+$",
});

/** A file core captured with an accepted turn. */
export const TurnAttachmentSchema = Type.Object({
  /** A capture and ordinal relative to the request's `attachment_root`; never a free path. */
  path: Type.String({
    pattern: "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/(0|[1-9][0-9]{0,2})$",
  }),
  /** The uploader's name for the file: display text, never a path. */
  name: Type.String({ minLength: 1, maxLength: 1024 }),
  size: Type.Integer({ minimum: 0 }),
  /** The media type the platform declared, unverified. */
  content_type: Type.Optional(Type.String({ maxLength: 255 })),
  sha256: Type.String({ pattern: "^[0-9a-f]{64}$" }),
});
export type TurnAttachment = Static<typeof TurnAttachmentSchema>;

/** An earlier turn's captured files, offered again to a later turn. */
export const UnansweredTurnSchema = Type.Object({
  correlation_id: Type.String({ minLength: 1 }),
  attachments: Type.Array(TurnAttachmentSchema, { minItems: 1, maxItems: 100 }),
});
export type UnansweredTurn = Static<typeof UnansweredTurnSchema>;

/** Core's request to run one claimed turn. */
export const RuntimeTurnRequestSchema = Type.Object({
  correlation_id: Type.String(),
  interface_id: ProviderIdentitySchema,
  agent_id: Type.String(),
  audience_id: Type.String(),
  input: Type.String(),
  /** The captured resource snapshot, read in place. */
  resource_root: Type.String(),
  /** Interface-owned metadata captured with the accepted event. */
  native: Type.Optional(Type.Unknown()),
  /** Where every captured attachment of the installation is now. */
  attachment_root: Type.String({ minLength: 1 }),
  /** This turn's captured files, in message order. */
  attachments: Type.Optional(Type.Array(TurnAttachmentSchema, { maxItems: 100 })),
  /**
   * Files of earlier turns in this conversation that no answered turn has
   * carried yet, oldest first, which the runtime's conversation may not hold.
   */
  unanswered: Type.Optional(Type.Array(UnansweredTurnSchema, { maxItems: 100 })),
  /**
   * Present on an observation: a turn core queued itself to carry the report
   * of native work an earlier turn started, with that turn's runtime cursor.
   */
  follow_up: Type.Optional(Type.Unknown()),
});
export type RuntimeTurnRequest = Static<typeof RuntimeTurnRequestSchema>;
