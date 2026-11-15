import { Type, type Static, type TSchema } from "typebox";
import { Check } from "typebox/value";
import {
  ControlSchema,
  ConversationKeySchema,
  WINDOW_LIMIT,
  parsedJson,
  type Control,
  type ConversationKey,
  type MessageSource,
} from "./stream.ts";

export const WINDOW_START = 50;
export const WINDOW_STEP = 50;

const Window = Type.Integer({ minimum: 1, maximum: WINDOW_LIMIT });

export const ViewSchema = Type.Object(
  {
    conversations: Window,
    open: Type.Union([
      Type.Object({ key: ConversationKeySchema, turns: Window }, { additionalProperties: false }),
      Type.Null(),
    ]),
  },
  { additionalProperties: false },
);
export type View = Static<typeof ViewSchema>;

export function widened(window: number): number {
  return Math.min(window + WINDOW_STEP, WINDOW_LIMIT);
}

export const SIGN_IN_PATH = "/api/sign-in";
export const STREAM_PATH = "/api/stream";

export function streamPath(view: View): string {
  const query = new URLSearchParams({ conversations: String(view.conversations) });
  if (view.open !== null) {
    query.set("agent_id", view.open.key.agent_id);
    query.set("audience_id", view.open.key.audience_id);
    query.set("turns", String(view.open.turns));
  }
  return `${STREAM_PATH}?${query.toString()}`;
}

function conversationPath(key: ConversationKey): string {
  return `/api/conversations/${encodeURIComponent(key.agent_id)}/${key.audience_id}`;
}

export function messagePath(key: ConversationKey, eventId: string): string {
  return `${conversationPath(key)}/messages/${eventId}`;
}

export function commandsPath(key: ConversationKey): string {
  return `${conversationPath(key)}/commands`;
}

export function controlPath(message: MessageSource): string {
  return `/api/messages/${message.event_id}/control`;
}

export function bearer(token: string): string {
  return `Bearer ${token}`;
}

export function mintAudienceId(): string {
  return crypto.randomUUID();
}

export function mintEventId(): string {
  return `m-${crypto.randomUUID()}`;
}

export const SignInBodySchema = Type.Object(
  { password: Type.String() },
  { additionalProperties: false },
);

export function signInBody(password: string): string {
  return JSON.stringify({ password } satisfies Static<typeof SignInBodySchema>);
}

export const ControlBodySchema = Type.Object(
  { action: ControlSchema },
  { additionalProperties: false },
);

export function controlBody(action: Control): string {
  return JSON.stringify({ action } satisfies Static<typeof ControlBodySchema>);
}

export function messageForm(text: string, files: readonly File[]): FormData {
  const form = new FormData();
  form.set("text", text);
  for (const file of files) form.append("file", file);
  return form;
}

export const SignedInSchema = Type.Object(
  { token: Type.String({ minLength: 1 }) },
  { additionalProperties: false },
);
export type SignedIn = Static<typeof SignedInSchema>;

export const SentSchema = Type.Object(
  { turn_id: Type.String({ minLength: 1 }), duplicate: Type.Boolean() },
  { additionalProperties: false },
);
export type Sent = Static<typeof SentSchema>;

export const CommandSchema = Type.Object(
  {
    name: Type.String({ minLength: 1, maxLength: 100 }),
    description: Type.Optional(Type.String({ minLength: 1 })),
    hint: Type.Optional(Type.String({ minLength: 1 })),
  },
  { additionalProperties: false },
);
export type Command = Static<typeof CommandSchema>;

export const CommandCatalogSchema = Type.Object(
  { commands: Type.Array(CommandSchema) },
  { additionalProperties: false },
);
export type CommandCatalog = Static<typeof CommandCatalogSchema>;

export function readAnswer<Schema extends TSchema>(
  schema: Schema,
  body: string,
): Static<Schema> | undefined {
  const answer = parsedJson(body);
  return Check(schema, answer) ? answer : undefined;
}

export type Reading = "ok" | "away" | "signed_out" | "refused";

export function reading(status: number): Reading {
  if (status >= 200 && status < 300) return "ok";
  if (status === 0 || status === 503) return "away";
  return status === 401 ? "signed_out" : "refused";
}

export const AWAY = "Carillon is not answering. Trying again.";
export const UNREADABLE =
  "This page could not read what Carillon answered. It is trying again. If this stays, reload the page.";
export const SIGNED_OUT =
  "Sign in with the web chat's password. To choose one, run `carillon web password` in a terminal.";

const RefusalSchema = Type.Object({ error: Type.String({ minLength: 1 }) });

export function refusalWords(status: number, body: string): string {
  if (reading(status) === "away") return AWAY;
  const refusal = parsedJson(body);
  if (Check(RefusalSchema, refusal))
    return refusal.error.charAt(0).toUpperCase() + refusal.error.slice(1);
  return status === 401
    ? SIGNED_OUT
    : `Carillon refused the request with status ${String(status)}.`;
}

export function retryDelayMs(failures: number): number {
  return failures === 0 ? 0 : Math.min(1000 * 2 ** (failures - 1), 10_000);
}
