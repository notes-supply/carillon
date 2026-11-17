import type { Conversation } from "../api/stream.ts";
import { addressOf } from "../session/address.ts";

export type Seen = ReadonlyMap<string, number>;

export function seenAfter(seen: Seen, listed: readonly Conversation[], open: string): Seen {
  const next = new Map(seen);
  for (const conversation of listed) {
    const address = addressOf(conversation.key);
    if (address === open || !next.has(address)) next.set(address, conversation.activity);
  }
  return next;
}

export function unread(seen: Seen, conversation: Conversation): boolean {
  return conversation.activity > (seen.get(addressOf(conversation.key)) ?? conversation.activity);
}
