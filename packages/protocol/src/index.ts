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

/** A request the runtime refuses; the message is shown to the person asking. */
