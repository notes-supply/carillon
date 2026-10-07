import type { TurnActivity } from "@carillon/protocol";
import { expect, it } from "vitest";
import {
  diagnostic,
  displayableExplanation,
  EXPLANATION_LIMIT,
  ExplainedFailure,
  redactCredentials,
  reportOf,
  RuntimeClosed,
  withReport,
} from "./failure.ts";

const key = `sk-proj-${"Ab1".repeat(20)}`;

// Ordinary usage-limit, rate-limit and authentication failures, in the shapes
// Pi's providers produce them, stay useful.
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
    "Rate limit reached for gpt-5 in organization org-abc on tokens per min (TPM): Limit 30000, Used 29000. Visit https://platform.example/rate-limits to learn more.",
    "Rate limit reached for gpt-5 in organization org-abc on tokens per min (TPM): Limit 30000, Used 29000. Visit [link] to learn more.",
  ],
  [
    '403: {"message":"Your organization must be verified to use this model.","type":"invalid_request_error","code":null}',
    "403: Your organization must be verified to use this model.",
  ],
  ["401 Unauthorized: invalid API key", "401 Unauthorized: invalid API key"],
  [
    `Authentication failed for "openai-codex". Credentials may have expired or network is unavailable. Run '/login openai-codex' to re-authenticate.`,
    `Authentication failed for "openai-codex". Credentials may have expired or network is unavailable. Run '/login openai-codex' to re-authenticate.`,
  ],
  [
    'Provider finish_reason: content_filter\n{"raw":"metadata"}',
    "Provider finish_reason: content_filter",
  ],
  [`Upstream rejected request ${key} for quota`, "Upstream rejected request [redacted] for quota"],
  ["  Request\tfailed:\u0007 upstream   refused  ", "Request failed: upstream refused"],
])("explains %j as %j", (text, explanation) => {
  expect(displayableExplanation(text)).toBe(explanation);
});

// Credential-bearing structures withhold the whole explanation, whatever the
// length of the value; so do raw pages and bodies without a conventional message.
it.each([
  ["an assignment", "Authentication failed: password=short-secret"],
  ["URL user information", "Request failed: https://alice:short-secret@example.invalid"],
  ["a link target", "Gateway error: [click here](https://example.invalid/?token=short-secret)"],
  ["a query", "Request to https://example.invalid/v1?key=short-secret failed"],
  ["a labelled key", "401 Incorrect API key provided: sk-short. Check your key."],
  ["a labelled JSON credential", '400: {"message":"rejected \\"token\\": short-secret"}'],
  ["an authorization scheme", "Proxy refused Bearer short-secret"],
  ["an HTML page", "403: <html><head><title>Forbidden</title></head></html>"],
  ["a JSON body without a message", 'Codex error: {"type":"error","code":"server_is_overloaded"}'],
  [
    "a message nested elsewhere in a JSON body",
    '400 {"detail":"bad","request":{"message":"the user prompt text"}}',
  ],
  [
    "a truncated JSON body",
    '400: {"error":{"message":"Invalid schema for function"... [truncated 812 chars]',
  ],
  ["nothing", " \n text on a later line"],
])("withholds %s", (_, text) => {
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

// Logs keep the diagnostic but not the values the display boundary withholds.
it.each([
  ["Authentication failed: password=short-secret", "Authentication failed: password=[redacted]"],
  [
    "Request failed: https://alice:short-secret@example.invalid",
    "Request failed: https://example.invalid",
  ],
  [
    "Gateway error: [click here](https://example.invalid/?token=short-secret)",
    "Gateway error: [click here](https://example.invalid/?[redacted]",
  ],
  [
    "401 Incorrect API key provided: sk-short. Check your key.",
    "401 Incorrect API key provided: [redacted] Check your key.",
  ],
  ['{"access_token":"short-secret","expires":3600}', '{"access_token":[redacted],"expires":3600}'],
  ["authorization: Basic c2hvcnQ6c2VjcmV0", "authorization: [redacted] [redacted]"],
  ["Proxy refused Bearer short-secret", "Proxy refused Bearer [redacted]"],
  [`upstream rejected ${key}`, "upstream rejected [redacted]"],
])("logs %j as %j", (text, logged) => {
  expect(redactCredentials(text)).toBe(logged);
});

it("logs the stack without credentials or error properties", () => {
  const failure = Object.assign(new Error(`upstream rejected password=short-secret ${key}`), {
    headers: { authorization: "Bearer request-header-secret" },
  });
  const logged = diagnostic(failure);
  expect(logged).toMatch(/^Error: upstream rejected password=\[redacted\] \[redacted\]\n\s+at /u);
  expect(logged).toContain("failure.test.ts");
  for (const secret of [key, "short-secret", "request-header-secret"])
    expect(logged).not.toContain(secret);
});

it("keeps a report with the failure a turn ended with, and a value that is no error as an error caused by it", () => {
  const report: TurnActivity = { phase: "tools", running: [], recent: [], tool_calls: 1 };
  // Objects and functions are the failure itself, with their class and status.
  const explained = new ExplainedFailure(502, "The build broke.");
  const closed = new RuntimeClosed("runtime is closed");
  const thrown = { code: "EPIPE" };
  const callable = () => undefined;
  for (const failure of [explained, closed, thrown, callable]) {
    expect(withReport(failure, report)).toBe(failure);
    expect(reportOf(failure)).toBe(report);
  }
  expect(explained).toMatchObject({ status: 502, message: "The build broke." });
  // Any other value cannot hold a report: an ordinary error whose cause it is does.
  for (const value of ["socket closed", 42, undefined, null, Symbol("gone")]) {
    const carrier = withReport(value, report);
    expect(carrier).toBeInstanceOf(Error);
    expect(carrier).not.toBeInstanceOf(ExplainedFailure);
    expect(carrier).not.toBeInstanceOf(RuntimeClosed);
    expect(carrier).toMatchObject({ cause: value });
    expect(reportOf(carrier)).toBe(report);
    expect(reportOf(value)).toBeUndefined();
  }
});
