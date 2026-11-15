import { SvelteMap } from "svelte/reactivity";
import { mintEventId } from "../api/requests.ts";
import { deliver, type Sending } from "../api/send.ts";
import type { ConversationKey, Opened } from "../api/stream.ts";
import { addressOf } from "../session/address.ts";
import { storedToken } from "../session/token.ts";
import { storeDraft, storedDraft } from "./draft.ts";

export type OnItsWay = {
  kind: "on_its_way";
  eventId: string;
  text: string;
  stage: Exclude<Sending["kind"], "refused">;
};
export type Refused = { kind: "refused"; words: string };

const written = new SvelteMap<string, string>();
const sent = new SvelteMap<string, OnItsWay | Refused>();

function joined(first: string, second: string): string {
  return first !== "" && second !== "" ? `${first}\n\n${second}` : first + second;
}

function isOnItsWay(address: string, eventId: string): boolean {
  const message = sent.get(address);
  return message?.kind === "on_its_way" && message.eventId === eventId;
}

export function draftOf(address: string): string {
  return written.get(address) ?? storedDraft(address);
}

export function write(address: string, text: string): void {
  written.set(address, text);
  storeDraft(address, text);
}

export function sentTo(address: string): OnItsWay | Refused | undefined {
  return sent.get(address);
}

export async function send(key: ConversationKey): Promise<void> {
  const address = addressOf(key);
  const text = draftOf(address);
  const token = storedToken();
  if (token === null || text.trim() === "" || sent.get(address)?.kind === "on_its_way") return;
  const message: OnItsWay = {
    kind: "on_its_way",
    eventId: mintEventId(),
    text,
    stage: "uploading",
  };
  sent.set(address, message);
  write(address, "");
  for await (const stage of deliver(key, message.eventId, text, token)) {
    if (!isOnItsWay(address, message.eventId)) return;
    if (stage.kind === "refused") {
      sent.set(address, stage);
      write(address, joined(text, draftOf(address)));
      return;
    }
    sent.set(address, { ...message, stage: stage.kind });
  }
}

export function settle(open: Opened | null): void {
  if (open?.kind !== "recorded") return;
  const address = addressOf(open.conversation.key);
  const message = sent.get(address);
  if (message?.kind !== "on_its_way") return;
  const arrived = open.turns.some(
    (turn) => turn.source.kind === "message" && turn.source.event_id === message.eventId,
  );
  if (arrived) sent.delete(address);
}
