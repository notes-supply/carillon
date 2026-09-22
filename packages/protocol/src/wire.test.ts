import { expect, it } from "vitest";
import { Check, Parse } from "typebox/value";
import fixtures from "../fixtures/models.json" with { type: "json" };
import { ModelStateSchema, ModelQuerySchema, ModelUpdateSchema } from "./index.ts";

it("preserves the shared Rust/TypeScript model wire fixtures", () => {
  for (const value of fixtures.states) expect(Parse(ModelStateSchema, value)).toEqual(value);
  for (const value of fixtures.queries) expect(Parse(ModelQuerySchema, value)).toEqual(value);
  for (const value of fixtures.updates) expect(Parse(ModelUpdateSchema, value)).toEqual(value);
  for (const value of fixtures.invalid_queries) expect(Check(ModelQuerySchema, value)).toBe(false);
  for (const value of fixtures.invalid_states) expect(Check(ModelStateSchema, value)).toBe(false);
  for (const value of fixtures.accepted_updates) expect(Check(ModelUpdateSchema, value)).toBe(true);
  for (const value of fixtures.invalid_updates) expect(Check(ModelUpdateSchema, value)).toBe(false);
});
