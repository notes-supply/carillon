import { Check } from "typebox/value";
import { ConversationKeySchema, type ConversationKey, type Opened } from "../api/stream.ts";

const ADDRESS = /^(?<agent>[^/]+)\/(?<audience>[^/]+)$/u;

function decoded(part: string): string {
  try {
    return decodeURIComponent(part);
  } catch {
    return "";
  }
}

export function addressOf(key: ConversationKey): string {
  return `${encodeURIComponent(key.agent_id)}/${key.audience_id}`;
}

export function keyAt(address: string): ConversationKey | null {
  const named = ADDRESS.exec(address)?.groups;
  const key = { agent_id: decoded(named?.["agent"] ?? ""), audience_id: named?.["audience"] };
  return Check(ConversationKeySchema, key) ? key : null;
}

export function openAddress(open: Opened | null): string {
  if (open === null) return "";
  return addressOf(open.kind === "recorded" ? open.conversation.key : open.key);
}

export function openedAt(open: Opened | null, address: string): Opened | null {
  return openAddress(open) === address ? open : null;
}
