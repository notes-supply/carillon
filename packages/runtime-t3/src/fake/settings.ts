import { Type, type Static } from "typebox";

const InstanceSchema = Type.Object({
  driver: Type.String(),
  displayName: Type.Optional(Type.String()),
  enabled: Type.Optional(Type.Boolean()),
  config: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
  environment: Type.Optional(
    Type.Array(
      Type.Object({
        name: Type.String(),
        value: Type.String(),
        sensitive: Type.Optional(Type.Boolean()),
      }),
    ),
  ),
});

/** An instance as the fake reports it, which a test may seed with values T3 never accepts on create. */
export type FakeInstance = Omit<Static<typeof InstanceSchema>, "environment"> & {
  readonly environment?: readonly {
    readonly name: string;
    readonly value: unknown;
    readonly sensitive?: boolean;
  }[];
};

export const InstanceCreateSchema = Type.Object({
  providerInstanceMutation: Type.Object({
    operation: Type.Literal("create"),
    instanceId: Type.String(),
    instance: InstanceSchema,
  }),
});

export function reportedInstances(
  instances: ReadonlyMap<string, FakeInstance>,
): Record<string, unknown> {
  return Object.fromEntries(
    [...instances].map(([instanceId, instance]) => [
      instanceId,
      instance.environment === undefined
        ? instance
        : {
            ...instance,
            environment: instance.environment.map((variable) =>
              variable.sensitive === true && variable.value !== ""
                ? { ...variable, value: "", valueRedacted: true }
                : variable,
            ),
          },
    ]),
  );
}
