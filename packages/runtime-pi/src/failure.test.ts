import { expect, it } from "vitest";
import { diagnostic, displayableExplanation, EXPLANATION_LIMIT } from "./failure.ts";

const key = `sk-proj-${"Ab1".repeat(20)}`;

// Shapes Pi's providers produce: a provider's own sentence, an SDK status with
// the provider's JSON body, gateway pages, appended metadata and echoed requests.
it.each([
  [
    "Codex error: The usage limit has been reached",
    "Codex error: The usage limit has been reached",
  ],
  [
    "You have hit your ChatGPT usage limit (plus plan). Try again in ~12 min.",
    "You have hit your ChatGPT usage limit (plus plan). Try again in ~12 min.",
  ],
  [
    '429 {"type":"error","error":{"type":"rate_limit_error","message":"Number of request tokens has exceeded your per-minute rate limit"}}',
    "429 Number of request tokens has exceeded your per-minute rate limit",
  ],
  [
    '403: {"message":"Your organization must be verified to use this model.","type":"invalid_request_error","code":null}',
    "403: Your organization must be verified to use this model.",
  ],
  [
    'Provider finish_reason: content_filter\n{"raw":"metadata"}',
    "Provider finish_reason: content_filter",
  ],
  [
    `401 Incorrect API key provided: ${key}. You can find your API key in your account.`,
    "401 Incorrect API key provided: [redacted]. You can find your API key in your account.",
  ],
  ["  Request\tfailed:\u0007 upstream   refused  ", "Request failed: upstream refused"],
])("explains %j as %j", (text, explanation) => {
  expect(displayableExplanation(text)).toBe(explanation);
});

it.each([
  ["an HTML page from a proxy", "403: <html><head><title>Forbidden</title></head></html>"],
  ["a JSON body without a message", 'Codex error: {"type":"error","code":"server_is_overloaded"}'],
  [
    "a truncated JSON body",
    '400: {"error":{"message":"Invalid schema for function"... [truncated 812 chars]',
  ],
  ["nothing", " \n text on a later line"],
])("has no explanation for %s", (_, text) => {
  expect(displayableExplanation(text)).toBeUndefined();
});

it("bounds an explanation at a word, whatever its length", () => {
  const explanation = displayableExplanation(`Quota exceeded: ${"try again later ".repeat(60)}`);
  expect(explanation?.length).toBeLessThanOrEqual(EXPLANATION_LIMIT);
  expect(explanation).toMatch(/^Quota exceeded: (?:(?:try|again|later) )+(?:try|again|later)…$/u);
  // Without a word boundary the cut still falls between whole graphemes.
  const family = "👨‍👩‍👧";
  const unbroken = displayableExplanation(family.repeat(EXPLANATION_LIMIT)) ?? "";
  expect(unbroken.length).toBeLessThanOrEqual(EXPLANATION_LIMIT);
  expect(unbroken.endsWith("…")).toBe(true);
  const kept = unbroken.slice(0, -1);
  expect(kept.length).toBeGreaterThan(EXPLANATION_LIMIT - family.length * 2);
  expect(kept).toBe(family.repeat(kept.length / family.length));
});

it("logs the stack without credentials or error properties", () => {
  const failure = Object.assign(new Error(`upstream rejected ${key}`), {
    headers: { authorization: "Bearer request-header-secret" },
  });
  const logged = diagnostic(failure);
  expect(logged).toMatch(/^Error: upstream rejected \[redacted\]\n\s+at /u);
  expect(logged).toContain("failure.test.ts");
  expect(logged).not.toContain(key);
  expect(logged).not.toContain("request-header-secret");
});
