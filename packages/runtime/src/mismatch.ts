import type { TSchema } from "typebox";
import { Errors } from "typebox/value";

/**
 * The reason to report why `value` does not match `schema`. TypeBox lists each
 * unknown key as a `false` schema before the object's own error, so the
 * object's error is reported instead, naming the keys.
 */
export function mismatch(schema: TSchema, value: unknown) {
  const errors = Errors(schema, value);
  const error = errors.find((each) => each.keyword !== "boolean") ?? errors[0];
  return error?.keyword === "additionalProperties"
    ? { ...error, message: `${error.message}: ${error.params.additionalProperties.join(", ")}` }
    : error;
}
