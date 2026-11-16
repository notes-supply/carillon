import { SvelteMap } from "svelte/reactivity";
import { mintEventId } from "../api/requests.ts";
import { deliver, type Sending } from "../api/send.ts";
import type { ConversationKey, Limits, Opened } from "../api/stream.ts";
import { addressOf } from "../session/address.ts";
import { storedToken } from "../session/token.ts";
import { storeDraft, storedDraft } from "./draft.ts";
import { overLimit } from "./limits.ts";

export type OnItsWay = {
  kind: "on_its_way";
  eventId: string;
  text: string;
  files: readonly File[];
  stage: Exclude<Sending, { kind: "refused" }>;
};
export type Refused = { kind: "refused"; words: string };

const NO_FILES: readonly File[] = [];

const written = new SvelteMap<string, string>();
const attached = new SvelteMap<string, readonly File[]>();
const sent = new SvelteMap<string, OnItsWay | Refused>();

function joined(first: string, second: string): string {
  return first !== "" && second !== "" ? `${first}\n\n${second}` : first + second;
}

function isOnItsWay(address: string, eventId: string): boolean {
  const message = sent.get(address);
  return message?.kind === "on_its_way" && message.eventId === eventId;
}

function forgetRefusal(address: string): void {
  if (sent.get(address)?.kind === "refused") sent.delete(address);
}

export function draftOf(address: string): string {
  return written.get(address) ?? storedDraft(address);
}

export function write(address: string, text: string): void {
  written.set(address, text);
  storeDraft(address, text);
}

export function attachedTo(address: string): readonly File[] {
  return attached.get(address) ?? NO_FILES;
}

export function attach(address: string, files: readonly File[]): void {
  if (files.length === 0) return;
  attached.set(address, [...attachedTo(address), ...files]);
  forgetRefusal(address);
}

export function detach(address: string, file: File): void {
  attached.set(
    address,
    attachedTo(address).filter((other) => other !== file),
  );
  forgetRefusal(address);
}

export function sentTo(address: string): OnItsWay | Refused | undefined {
  return sent.get(address);
}

export async function send(key: ConversationKey, limits: Limits): Promise<void> {
  const address = addressOf(key);
  const text = draftOf(address);
  const files = attachedTo(address);
  const token = storedToken();
  if (token === null || sent.get(address)?.kind === "on_its_way") return;
  if (text.trim() === "" && files.length === 0) return;
  if (overLimit(files, limits) !== undefined) return;
  const message: OnItsWay = {
    kind: "on_its_way",
    eventId: mintEventId(),
    text,
    files,
    stage: { kind: "uploading", sent: 0, total: 0 },
  };
  sent.set(address, message);
  attached.delete(address);
  write(address, "");
  for await (const stage of deliver({ key, eventId: message.eventId, text, files }, token)) {
    if (!isOnItsWay(address, message.eventId)) return;
    if (stage.kind === "refused") {
      sent.set(address, stage);
      attached.set(address, [...files, ...attachedTo(address)]);
      write(address, joined(text, draftOf(address)));
      return;
    }
    sent.set(address, { ...message, stage });
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
