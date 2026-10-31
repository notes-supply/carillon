import type { TSchema } from "typebox";
import { Errors } from "typebox/value";

/**
 * The reason to report why `value` does not match `schema`. An unknown key is
 * reported first, naming the keys: TypeBox lists it after the error it causes,
 * a misspelled required key or a union's earlier branch, and before that
 * lists each unknown key as a `false` schema. Otherwise the deepest error
 * names the value: a union lists the branch the value was not meant for, at
 * the union's own path, before the error inside the branch it was.
 */
export function mismatch(schema: TSchema, value: unknown) {
  const errors = Errors(schema, value);
  const unknown = errors.find((each) => each.keyword === "additionalProperties");
  if (unknown !== undefined)
    return {
      ...unknown,
      message: `${unknown.message}: ${unknown.params.additionalProperties.join(", ")}`,
    };
  const depth = (path: string) => path.split("/").length;
  let deepest = errors.find((each) => each.keyword !== "boolean") ?? errors[0];
  for (const each of errors)
    if (each.keyword !== "boolean" && depth(each.instancePath) > depth(deepest?.instancePath ?? ""))
      deepest = each;
  return deepest;
}
