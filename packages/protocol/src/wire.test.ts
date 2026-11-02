import { expect, it } from "vitest";
import { Check, Parse } from "typebox/value";
import activity from "../fixtures/activity.json" with { type: "json" };
import commands from "../fixtures/commands.json" with { type: "json" };
import held from "../fixtures/held.json" with { type: "json" };
import settings from "../fixtures/settings.json" with { type: "json" };
import turns from "../fixtures/turns.json" with { type: "json" };
import {
  CommandCatalogSchema,
  OpenedTurnSchema,
  RunningTurnSchema,
  RuntimeTurnControlSchema,
  RuntimeSettingsSchema,
  RuntimeTurnRequestSchema,
  SettingsRequestSchema,
  SettingValueSchema,
  SubjectSchema,
  TurnActivitySchema,
  TurnOpeningSchema,
  TurnOutcomeSchema,
} from "./index.ts";

it("preserves the shared Rust/TypeScript settings wire fixtures", () => {
  for (const value of settings.subjects) expect(Parse(SubjectSchema, value)).toEqual(value);
  for (const value of settings.invalid_subjects) expect(Check(SubjectSchema, value)).toBe(false);
  for (const value of settings.values) expect(Parse(SettingValueSchema, value)).toEqual(value);
  for (const value of settings.invalid_values) expect(Check(SettingValueSchema, value)).toBe(false);
  for (const value of settings.requests) expect(Parse(SettingsRequestSchema, value)).toEqual(value);
  for (const value of settings.invalid_requests)
    expect(Check(SettingsRequestSchema, value)).toBe(false);
  for (const value of settings.settings) expect(Parse(RuntimeSettingsSchema, value)).toEqual(value);
  for (const value of settings.invalid_settings)
    expect(Check(RuntimeSettingsSchema, value)).toBe(false);
});

it("refuses every subject with a property its kind does not have", () => {
  for (const subject of settings.subjects)
    expect(Check(SubjectSchema, { ...subject, audience_id: "discord-100-300" })).toBe(false);
});

it("refuses a turn request or an opened turn without its channel", () => {
  const opened = {
    event_id: "runtime:runtime-pi:schedule:1",
    turn_id: "turn-1",
    state: "running",
    duplicate: false,
    channel_id: "300",
  };
  expect(Check(OpenedTurnSchema, opened)).toBe(true);
  const { channel_id: _opened, ...withoutChannel } = opened;
  expect(Check(OpenedTurnSchema, withoutChannel)).toBe(false);
  const [request] = turns.requests;
  const { channel_id: _request, ...channelless } = request ?? {};
  expect(Check(RuntimeTurnRequestSchema, channelless)).toBe(false);
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
  for (const value of held.running) expect(Parse(RunningTurnSchema, value)).toEqual(value);
  for (const value of held.invalid_running) expect(Check(RunningTurnSchema, value)).toBe(false);
});
