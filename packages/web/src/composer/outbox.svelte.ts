import { SvelteMap } from "svelte/reactivity";
import type { Credential } from "../api/credential.ts";
import { mintEventId } from "../api/requests.ts";
import { deliver, type Sending } from "../api/send.ts";
import type { ConversationKey, Limits, Opened } from "../api/stream.ts";
import { addressOf } from "../session/address.ts";
import { storeDraft, storedDraft, whenClearedElsewhere } from "./draft.ts";
import { overLimit } from "./limits.ts";

export type OnItsWay = {
  kind: "on_its_way";
  eventId: string;
  text: string;
  files: readonly File[];
  stage: Exclude<Sending, { kind: "refused" | "cancelled" }>;
};
export type Refused = { kind: "refused"; words: string };

const NO_FILES: readonly File[] = [];

const written = new SvelteMap<string, string>();
const attached = new SvelteMap<string, readonly File[]>();
const sent = new SvelteMap<string, OnItsWay | Refused>();
let signedIn = new AbortController();

function joined(first: string, second: string): string {
  return first !== "" && second !== "" ? `${first}\n\n${second}` : first + second;
}

function since(stored: string, text: string): string {
  if (stored === text) return "";
  return text !== "" && stored.startsWith(`${text}\n\n`) ? stored.slice(text.length + 2) : stored;
}

function onItsWay(address: string, eventId: string): OnItsWay | undefined {
  const message = sent.get(address);
  return message?.kind === "on_its_way" && message.eventId === eventId ? message : undefined;
}

function clearSent(address: string, message: OnItsWay): void {
  if (message.stage.kind !== "received")
    storeDraft(address, since(storedDraft(address), message.text));
}

export function awaitingReceipt(address: string): OnItsWay | undefined {
  const message = sent.get(address);
  return message?.kind === "on_its_way" && message.stage.kind !== "received" ? message : undefined;
}

function store(address: string): void {
  storeDraft(address, joined(awaitingReceipt(address)?.text ?? "", written.get(address) ?? ""));
}

function forgetRefusal(address: string): void {
  if (sent.get(address)?.kind === "refused") sent.delete(address);
}

export function draftOf(address: string): string {
  return written.get(address) ?? storedDraft(address);
}

export function write(address: string, text: string): void {
  written.set(address, text);
  store(address);
}

whenClearedElsewhere((address) => {
  if (written.has(address)) store(address);
});

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

export function carry(from: string, into: string): void {
  attached.set(into, attachedTo(from));
  attached.delete(from);
  write(into, draftOf(from));
  write(from, "");
  forgetRefusal(from);
}

export function sentTo(address: string): OnItsWay | Refused | undefined {
  return sent.get(address);
}

export async function send(
  key: ConversationKey,
  credential: Credential | null,
  limits: Limits,
): Promise<void> {
  const address = addressOf(key);
  const text = draftOf(address);
  const files = attachedTo(address);
  if (credential === null || awaitingReceipt(address) !== undefined) return;
  if (text.trim() === "" && files.length === 0) return;
  if (overLimit(files, limits) !== undefined) return;
  const message: OnItsWay = {
    kind: "on_its_way",
    eventId: mintEventId(),
    text,
    files,
    stage: { kind: "uploading", sent: 0, total: 0 },
  };
  const { signal } = signedIn;
  sent.set(address, message);
  attached.delete(address);
  write(address, "");
  for await (const stage of deliver(
    { key, eventId: message.eventId, text, files },
    credential,
    signal,
  )) {
    const waiting = onItsWay(address, message.eventId);
    if (waiting === undefined) return;
    if (stage.kind === "refused" || stage.kind === "cancelled") {
      if (stage.kind === "refused") sent.set(address, stage);
      else sent.delete(address);
      attached.set(address, [...files, ...attachedTo(address)]);
      write(address, joined(text, draftOf(address)));
      return;
    }
    if (stage.kind === "received") clearSent(address, waiting);
    sent.set(address, { ...message, stage });
  }
}

export function cancelSends(): void {
  signedIn.abort();
  signedIn = new AbortController();
}

export function settle(open: Opened | null): void {
  if (open?.kind !== "recorded") return;
  const address = addressOf(open.conversation.key);
  const message = sent.get(address);
  if (message?.kind !== "on_its_way") return;
  const arrived = open.turns.some(
    (turn) => turn.source.kind === "message" && turn.source.event_id === message.eventId,
  );
  if (!arrived) return;
  clearSent(address, message);
  sent.delete(address);
}
