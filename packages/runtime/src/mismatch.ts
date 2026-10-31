import type { TSchema } from "typebox";
import { Errors } from "typebox/value";

/**
 * The reason to report why `value` does not match `schema`. An unknown key is
 * reported first, naming the keys: TypeBox lists it after the error it causes,
 * a misspelled required key or a union's earlier branch, and before that
 * lists each unknown key as a `false` schema. Otherwise the first error is
 * reported, passing over a union branch whose type the value does not have
 * while another branch found errors inside it: the value was meant for that one.
 */
export function mismatch(schema: TSchema, value: unknown) {
  const errors = Errors(schema, value);
  const unknown = errors.find((each) => each.keyword === "additionalProperties");
  if (unknown !== undefined)
    return {
      ...unknown,
      message: `${unknown.message}: ${unknown.params.additionalProperties.join(", ")}`,
    };
  const reported = errors.filter((each) => each.keyword !== "boolean");
  const beneath = (path: string) =>
    reported.some((other) => other.instancePath.startsWith(`${path}/`));
  return (
    reported.find((each) => each.keyword !== "type" || !beneath(each.instancePath)) ?? errors[0]
  );
}
