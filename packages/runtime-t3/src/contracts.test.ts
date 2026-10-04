import { expect, it } from "vitest";
import { derivedId } from "./contracts.ts";

it("derives one identity per sequence of facts, with no fact running into the next", () => {
  // Adjacent facts that would concatenate to the same bytes stay distinct.
  expect(derivedId("a", "b\0c")).not.toBe(derivedId("a\0b", "c"));
  expect(derivedId("ab", "c")).not.toBe(derivedId("a", "bc"));
  // An empty fact is a fact; an absent one is not.
  expect(derivedId("a", "")).not.toBe(derivedId("a"));
  expect(derivedId("", "a")).not.toBe(derivedId("a", ""));
  // Legitimate text, including Unicode, derives a stable UUID-shaped identity.
  const unicode = derivedId("thread", "projet-été", "discord", "🙂", "conversation-1");
  expect(unicode).toBe(derivedId("thread", "projet-été", "discord", "🙂", "conversation-1"));
  expect(unicode).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/u);
  expect(derivedId("a", "b")).not.toBe(derivedId("b", "a"));
});
