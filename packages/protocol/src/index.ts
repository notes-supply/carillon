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
});
export type ScopeState = Static<typeof ScopeStateSchema>;
export const ModelStateSchema = Type.Object({
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
 * What a runtime is doing in one executing turn, for display while it runs.
 * The runtime formats each `detail` from its own tool semantics; consumers
 * show it as given and never receive raw tool arguments.
 */
export const ActivityPhaseSchema = Type.Union([
  Type.Literal("starting"),
  Type.Literal("model"),
  Type.Literal("tools"),
  Type.Literal("retrying"),
  Type.Literal("compacting"),
]);
export type ActivityPhase = Static<typeof ActivityPhaseSchema>;
export const RunningToolSchema = Type.Object({
  tool: Type.String({ minLength: 1 }),
  detail: Type.Optional(Type.String({ minLength: 1 })),
  /** Unix seconds. */
  started_at: Type.Integer(),
});
export type RunningTool = Static<typeof RunningToolSchema>;
export const FinishedToolSchema = Type.Object({
  tool: Type.String({ minLength: 1 }),
  detail: Type.Optional(Type.String({ minLength: 1 })),
  ok: Type.Boolean(),
});
export type FinishedTool = Static<typeof FinishedToolSchema>;
export const TurnActivitySchema = Type.Object({
  phase: ActivityPhaseSchema,
  /** The model's own latest public commentary, when it wrote any. */
  headline: Type.Optional(Type.String({ minLength: 1 })),
  running: Type.Array(RunningToolSchema),
  /** The most recently finished calls, oldest first. */
  recent: Type.Array(FinishedToolSchema),
  /** Every call started in the turn, including those no longer listed. */
  tool_calls: Type.Integer({ minimum: 0 }),
});
export type TurnActivity = Static<typeof TurnActivitySchema>;

/** A configured provider's registry identity. */
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
   * Files of earlier turns in this conversation since its last answered one,
   * oldest first, which the runtime's conversation may not have recorded.
   */
  unanswered: Type.Optional(Type.Array(UnansweredTurnSchema, { maxItems: 100 })),
});
export type RuntimeTurnRequest = Static<typeof RuntimeTurnRequestSchema>;
