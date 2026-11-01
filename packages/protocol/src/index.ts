/** Shared wire schemas. Runtime-specific settings and their values remain runtime-owned. */
import { Type, type Static } from "typebox";

/** Header naming the configured provider on provider-to-core requests. */
export const PROVIDER_HEADER = "x-carillon-provider";

/** An agent's conversation, as core relays requests about it, such as its commands. */
export const ConversationQuerySchema = Type.Object({
  interface_id: Type.String({ minLength: 1 }),
  agent_id: Type.String({ minLength: 1 }),
  audience_id: Type.Optional(Type.String({ minLength: 1 })),
});
export type ConversationQuery = Static<typeof ConversationQuerySchema>;

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
/**
 * What a person may do with a message that waits behind a running turn:
 * hand it to the running turn now, end the running turn and run it next, or
 * not run it at all.
 */
export const ControlActionSchema = Type.Union([
  Type.Literal("steer"),
  Type.Literal("interrupt"),
  Type.Literal("stop"),
]);
export type ControlAction = Static<typeof ControlActionSchema>;
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
  /** What a person may do with the turn while it waits, each once. */
  controls: Type.Optional(Type.Array(ControlActionSchema, { maxItems: 3, uniqueItems: true })),
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

/** A turn core hands a runtime to hold and run. */
export const RuntimeTurnRequestSchema = Type.Object({
  correlation_id: Type.String(),
  interface_id: ProviderIdentitySchema,
  agent_id: Type.String(),
  audience_id: Type.String(),
  /** The channel core recorded for the conversation. */
  channel_id: Type.String({ minLength: 1 }),
  input: Type.String(),
  /** The captured resource snapshot, read in place. */
  resource_root: Type.String(),
  /** Interface-owned metadata captured with the accepted event. */
  native: Type.Optional(Type.Unknown()),
  /** Where every captured attachment of the installation is now. */
  attachment_root: Type.String({ minLength: 1 }),
  /** This turn's captured files, in message order. */
  attachments: Type.Optional(Type.Array(TurnAttachmentSchema, { maxItems: 100 })),
});
export type RuntimeTurnRequest = Static<typeof RuntimeTurnRequestSchema>;

/**
 * How a runtime says a turn it holds ended. `steered` and `stopped`
 * send nothing to the conversation; a failure's `explanation` is shown as
 * given.
 */
export const TurnOutcomeSchema = Type.Union([
  Type.Object(
    {
      outcome: Type.Literal("answered"),
      output: Type.String({ pattern: "\\S" }),
      activity: Type.Optional(TurnActivitySchema),
    },
    { additionalProperties: false },
  ),
  Type.Object(
    {
      outcome: Type.Literal("failed"),
      explanation: Type.Optional(Type.String({ minLength: 1 })),
      activity: Type.Optional(TurnActivitySchema),
    },
    { additionalProperties: false },
  ),
  Type.Object({ outcome: Type.Literal("steered") }, { additionalProperties: false }),
  Type.Object({ outcome: Type.Literal("stopped") }, { additionalProperties: false }),
]);
export type TurnOutcome = Static<typeof TurnOutcomeSchema>;

/**
 * A turn a runtime opens in an existing conversation of its own, for
 * a schedule or a subagent's report. The same `key` opens one turn.
 */
export const TurnOpeningSchema = Type.Object(
  {
    interface_id: ProviderIdentitySchema,
    agent_id: Type.String({ minLength: 1 }),
    audience_id: Type.String({ minLength: 1 }),
    key: Type.String({ minLength: 1 }),
    /** What the turn is for, recorded as its input. */
    label: Type.Optional(Type.String({ minLength: 1 })),
  },
  { additionalProperties: false },
);
export type TurnOpening = Static<typeof TurnOpeningSchema>;

/** What core recorded of a turn a runtime opened, with the channel core recorded for its conversation. */
export const OpenedTurnSchema = Type.Object({
  event_id: Type.String(),
  turn_id: Type.String({ minLength: 1 }),
  state: Type.Union([
    Type.Literal("queued"),
    Type.Literal("running"),
    Type.Literal("completed"),
    Type.Literal("interrupted"),
  ]),
  duplicate: Type.Boolean(),
  channel_id: Type.String({ minLength: 1 }),
});
export type OpenedTurn = Static<typeof OpenedTurnSchema>;

/**
 * A turn core records running on the runtime that asks: the request core
 * handed it, and the key of a turn the runtime opened.
 */
export const RunningTurnSchema = Type.Object(
  {
    request: RuntimeTurnRequestSchema,
    key: Type.Optional(Type.String({ minLength: 1 })),
  },
  { additionalProperties: false },
);
export type RunningTurn = Static<typeof RunningTurnSchema>;

/** Core's relay of a person's control of a turn the runtime holds. */
export const RuntimeTurnControlSchema = Type.Object(
  { interface_id: ProviderIdentitySchema, action: ControlActionSchema },
  { additionalProperties: false },
);
export type RuntimeTurnControl = Static<typeof RuntimeTurnControlSchema>;

/**
 * What a page of settings is about. Every revision covers it, so a change
 * drawn for one agent never applies after the channel moved to another.
 */
export const SubjectSchema = Type.Union([
  Type.Object(
    { kind: Type.Literal("agent"), agent_id: Type.String({ minLength: 1 }) },
    { additionalProperties: false },
  ),
  Type.Object(
    {
      kind: Type.Literal("channel"),
      interface_id: ProviderIdentitySchema,
      channel_id: Type.String({ minLength: 1 }),
      agent_id: Type.String({ minLength: 1 }),
    },
    { additionalProperties: false },
  ),
]);
export type Subject = Static<typeof SubjectSchema>;

/** A setting's value, as choice IDs. */
export const SettingValueSchema = Type.Union([
  Type.Object(
    { kind: Type.Literal("one"), id: Type.String({ minLength: 1 }) },
    { additionalProperties: false },
  ),
  Type.Object(
    { kind: Type.Literal("many"), ids: Type.Array(Type.String()) },
    { additionalProperties: false },
  ),
]);
export type SettingValue = Static<typeof SettingValueSchema>;

/**
 * The owner's digest of everything that decides what a setting means at its
 * subject. A change names the revision it was drawn from, and the owner
 * refuses it once the revision is another.
 */
export const RevisionSchema = Type.String({ pattern: "^[0-9a-f]{64}$" });

export const ChoiceSchema = Type.Object({
  id: Type.String({ minLength: 1 }),
  name: Type.String({ minLength: 1 }),
  /** The heading a renderer lists the choice under, such as its provider. */
  group: Type.Optional(Type.String()),
});
export type Choice = Static<typeof ChoiceSchema>;

/** One line of a page. Without `choices` it is shown and not editable; `value` is null when nothing applies. */
export const SettingSchema = Type.Object({
  id: Type.String({ minLength: 1 }),
  name: Type.String({ minLength: 1 }),
  choices: Type.Array(ChoiceSchema),
  value: Type.Union([SettingValueSchema, Type.Null()]),
  /** The value comes from the agent, not from this subject. */
  inherited: Type.Boolean(),
  revision: RevisionSchema,
});
export type Setting = Static<typeof SettingSchema>;

/** A change to one setting; a null `value` returns it to what it inherits. */
export const ChangeSchema = Type.Object(
  {
    setting: Type.String({ minLength: 1 }),
    value: Type.Union([SettingValueSchema, Type.Null()]),
    expected: RevisionSchema,
  },
  { additionalProperties: false },
);
export type Change = Static<typeof ChangeSchema>;

/** A read of a subject's runtime settings, with a change to apply first. */
export const SettingsRequestSchema = Type.Object(
  { subject: SubjectSchema, change: Type.Optional(ChangeSchema) },
  { additionalProperties: false },
);
export type SettingsRequest = Static<typeof SettingsRequestSchema>;

/** The settings a runtime owns for a subject, in the order it shows them. */
export const RuntimeSettingsSchema = Type.Object({ settings: Type.Array(SettingSchema) });
export type RuntimeSettings = Static<typeof RuntimeSettingsSchema>;
