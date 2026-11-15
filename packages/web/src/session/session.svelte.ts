import { AWAY, UNREADABLE } from "../api/requests.ts";
import { requestSignIn } from "../api/sign-in.ts";
import type { ChatSnapshot, Received } from "../api/stream.ts";
import { settle } from "../composer/outbox.svelte.ts";
import { seenAfter, type Seen } from "../rail/unread.ts";
import { openAddress } from "./address.ts";
import { dropToken, storeToken, storedToken } from "./token.ts";

let token = $state(storedToken());
let snapshot = $state.raw<ChatSnapshot | null>(null);
let notice = $state("");
let seen = $state.raw<Seen>(new Map());

export const session = {
  get token(): string | null {
    return token;
  },
  get snapshot(): ChatSnapshot | null {
    return snapshot;
  },
  get notice(): string {
    return notice;
  },
  get seen(): Seen {
    return seen;
  },
};

export async function signIn(password: string): Promise<string> {
  const answer = await requestSignIn(password);
  if (answer.kind === "refused") return answer.words;
  storeToken(answer.token);
  token = answer.token;
  return "";
}

function signOut(): void {
  dropToken();
  token = null;
  snapshot = null;
  notice = "";
  seen = new Map();
}

export function see(open: string): void {
  if (snapshot !== null) seen = seenAfter(seen, snapshot.conversations, open);
}

export function hear(said: Received): void {
  switch (said.kind) {
    case "snapshot":
      snapshot = said.snapshot;
      notice = "";
      see(openAddress(said.snapshot.open));
      settle(said.snapshot.open);
      return;
    case "unavailable":
      notice = AWAY;
      return;
    case "unreadable":
      notice = UNREADABLE;
      return;
    case "signed_out":
      signOut();
      return;
  }
}
