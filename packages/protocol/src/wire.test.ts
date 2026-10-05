import { expect, it } from "vitest";
import { Check, Parse } from "typebox/value";
import activity from "../fixtures/activity.json" with { type: "json" };
import commands from "../fixtures/commands.json" with { type: "json" };
import fixtures from "../fixtures/models.json" with { type: "json" };
import turns from "../fixtures/turns.json" with { type: "json" };
import {
  CommandCatalogSchema,
  ModelStateSchema,
  ModelQuerySchema,
  ModelUpdateSchema,
  RuntimeTurnRequestSchema,
  TurnActivitySchema,
} from "./index.ts";

it("preserves the shared Rust/TypeScript model wire fixtures", () => {
  for (const value of fixtures.states) expect(Parse(ModelStateSchema, value)).toEqual(value);
  for (const value of fixtures.queries) expect(Parse(ModelQuerySchema, value)).toEqual(value);
  for (const value of fixtures.updates) expect(Parse(ModelUpdateSchema, value)).toEqual(value);
  for (const value of fixtures.invalid_queries) expect(Check(ModelQuerySchema, value)).toBe(false);
  for (const value of fixtures.invalid_states) expect(Check(ModelStateSchema, value)).toBe(false);
  for (const value of fixtures.accepted_updates) expect(Check(ModelUpdateSchema, value)).toBe(true);
  for (const value of fixtures.invalid_updates) expect(Check(ModelUpdateSchema, value)).toBe(false);
});

it("preserves the shared Rust/TypeScript activity wire fixtures", () => {
  for (const value of activity.activities) expect(Parse(TurnActivitySchema, value)).toEqual(value);
  for (const value of activity.invalid_activities)
    expect(Check(TurnActivitySchema, value)).toBe(false);
});

it("preserves the shared Rust/TypeScript runtime turn fixtures", () => {
  for (const value of turns.requests) expect(Parse(RuntimeTurnRequestSchema, value)).toEqual(value);
  for (const value of turns.invalid_requests)
    expect(Check(RuntimeTurnRequestSchema, value)).toBe(false);
});

it("preserves the shared Rust/TypeScript command catalogue fixtures", () => {
  for (const value of commands.catalogs) expect(Parse(CommandCatalogSchema, value)).toEqual(value);
  for (const value of commands.invalid_catalogs)
    expect(Check(CommandCatalogSchema, value)).toBe(false);
});
