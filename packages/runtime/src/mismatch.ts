import type { TSchema } from "typebox";
import { Errors } from "typebox/value";

/**
 * The reason to report why `value` does not match `schema`. An unknown key is
 * reported first, naming the keys: TypeBox lists it after the error it causes,
 * a misspelled required key or a union's earlier branch, and before that
 * lists each unknown key as a `false` schema.
 */
export function mismatch(schema: TSchema, value: unknown) {
  const errors = Errors(schema, value);
  const unknown = errors.find((each) => each.keyword === "additionalProperties");
  if (unknown !== undefined)
    return {
      ...unknown,
      message: `${unknown.message}: ${unknown.params.additionalProperties.join(", ")}`,
    };
  return errors.find((each) => each.keyword !== "boolean") ?? errors[0];
}
