import { expect, it } from "vitest";
import { Check, Parse } from "typebox/value";
import activity from "../fixtures/activity.json" with { type: "json" };
import commands from "../fixtures/commands.json" with { type: "json" };
import held from "../fixtures/held.json" with { type: "json" };
import fixtures from "../fixtures/models.json" with { type: "json" };
import turns from "../fixtures/turns.json" with { type: "json" };
import {
  CommandCatalogSchema,
  ModelStateSchema,
  ModelQuerySchema,
  ModelUpdateSchema,
  RuntimeTurnControlSchema,
  RuntimeTurnRequestSchema,
  TurnActivitySchema,
  TurnOpeningSchema,
  TurnOutcomeSchema,
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
  // A runtime lists each known control once; core reads any list it gets.
  for (const { sent, read } of activity.lenient_controls) {
    expect(Check(TurnActivitySchema, sent)).toBe(false);
    expect(Parse(TurnActivitySchema, read)).toEqual(read);
  }
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

it("preserves the shared Rust/TypeScript held turn fixtures", () => {
  for (const value of held.outcomes) expect(Parse(TurnOutcomeSchema, value)).toEqual(value);
  for (const value of held.invalid_outcomes) expect(Check(TurnOutcomeSchema, value)).toBe(false);
  for (const value of held.openings) expect(Parse(TurnOpeningSchema, value)).toEqual(value);
  for (const value of held.invalid_openings) expect(Check(TurnOpeningSchema, value)).toBe(false);
  for (const value of held.controls) expect(Parse(RuntimeTurnControlSchema, value)).toEqual(value);
  for (const value of held.invalid_controls)
    expect(Check(RuntimeTurnControlSchema, value)).toBe(false);
});
