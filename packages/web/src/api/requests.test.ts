import { expect, test } from "vitest";
import {
  AWAY,
  CommandCatalogSchema,
  SIGNED_OUT,
  SignedInSchema,
  controlBody,
  controlPath,
  commandsPath,
  messageForm,
  messagePath,
  mintAudienceId,
  mintEventId,
  readAnswer,
  reading,
  refusalWords,
  retryDelayMs,
  signInBody,
  streamPath,
  widened,
} from "./requests.ts";
import { AUDIENCE_ID_PATTERN, EVENT_ID_PATTERN, type MessageSource } from "./stream.ts";

const KEY = { agent_id: "assistant", audience_id: "0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b" };

test("a view travels as the query core reads", () => {
  expect(streamPath({ conversations: 50, open: null })).toBe("/api/stream?conversations=50");
  expect(streamPath({ conversations: 100, open: { key: KEY, turns: 150 } })).toBe(
    "/api/stream?conversations=100&agent_id=assistant&audience_id=0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b&turns=150",
  );
});

test("a window grows by a page and stops at core's limit", () => {
  expect(widened(50)).toBe(100);
  expect(widened(450)).toBe(500);
  expect(widened(500)).toBe(500);
});

test("a conversation's paths carry its agent, escaped", () => {
  const odd = { agent_id: "night shift/2", audience_id: KEY.audience_id };
  expect(messagePath(odd, "m-0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa")).toBe(
    "/api/conversations/night%20shift%2F2/0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b/messages/m-0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa",
  );
  expect(commandsPath(KEY)).toBe(
    "/api/conversations/assistant/0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b/commands",
  );
});

test("a control names its turn by the message that began it", () => {
  const message: MessageSource = {
    kind: "message",
    event_id: "m-0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa",
    text: "",
    command: false,
    files: [],
  };
  expect(controlPath(message)).toBe("/api/messages/m-0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa/control");
  expect(controlBody("interrupt")).toBe('{"action":"interrupt"}');
});

test("minted IDs are the ones the provider admits", () => {
  expect(mintAudienceId()).toMatch(new RegExp(AUDIENCE_ID_PATTERN, "u"));
  expect(mintEventId()).toMatch(new RegExp(EVENT_ID_PATTERN, "u"));
  expect(mintEventId()).not.toBe(mintEventId());
});

test("a message's form has its text, then each file under the same name", () => {
  const files = [new File(["a"], "one.txt"), new File(["bb"], "two.txt")];
  const form = messageForm("Read these.", files);
  expect([...form.keys()]).toStrictEqual(["text", "file", "file"]);
  expect(form.get("text")).toBe("Read these.");
  expect(
    form.getAll("file").map((part) => (part instanceof File ? part.name : part)),
  ).toStrictEqual(["one.txt", "two.txt"]);
  expect([...messageForm("Only text.", []).keys()]).toStrictEqual(["text"]);
});

test("a sign-in's body is the password as typed", () => {
  expect(signInBody(' a "quoted" one ')).toBe('{"password":" a \\"quoted\\" one "}');
});

test("an answer is read only in the shape its route gives", () => {
  expect(readAnswer(SignedInSchema, '{"token":"9f2c"}')).toStrictEqual({ token: "9f2c" });
  expect(readAnswer(SignedInSchema, '{"token":""}')).toBe(undefined);
  expect(
    readAnswer(CommandCatalogSchema, '{"commands":[{"name":"status","hint":"[job]"}]}'),
  ).toStrictEqual({ commands: [{ name: "status", hint: "[job]" }] });
  expect(readAnswer(CommandCatalogSchema, '{"commands":[{"name":""}]}')).toBe(undefined);
});

test("a status says whether to go on, wait, sign in or show a refusal", () => {
  expect(reading(200)).toBe("ok");
  expect(reading(204)).toBe("ok");
  expect(reading(0)).toBe("away");
  expect(reading(503)).toBe("away");
  expect(reading(401)).toBe("signed_out");
  for (const status of [400, 404, 409, 413, 422, 429, 500, 507])
    expect(reading(status)).toBe("refused");
});

test("a refusal shows the provider's words, or a sentence for its status", () => {
  expect(refusalWords(413, '{"error":"a message carries at most 10 files."}')).toBe(
    "A message carries at most 10 files.",
  );
  expect(refusalWords(401, '{"error":"That is not the web chat\'s password."}')).toBe(
    "That is not the web chat's password.",
  );
  expect(
    refusalWords(
      429,
      '{"error":"Another sign-in is being checked. Wait a moment, then try again."}',
    ),
  ).toBe("Another sign-in is being checked. Wait a moment, then try again.");
  expect(refusalWords(401, "")).toBe(SIGNED_OUT);
  expect(refusalWords(400, "a conversation is a lowercase UUID")).toBe(
    "Carillon refused the request with status 400.",
  );
  expect(refusalWords(404, "")).toBe("Carillon refused the request with status 404.");
  expect(refusalWords(422, '{"error":""}')).toBe("Carillon refused the request with status 422.");
  expect(
    refusalWords(503, '{"error":"Carillon is not answering right now. Try again in a moment."}'),
  ).toBe(AWAY);
  expect(refusalWords(0, "")).toBe(AWAY);
});

test("a retry follows a clean end at once, then waits longer, up to ten seconds", () => {
  expect([0, 1, 2, 3, 4, 5, 6, 40].map(retryDelayMs)).toStrictEqual([
    0, 1000, 2000, 4000, 8000, 10_000, 10_000, 10_000,
  ]);
});
