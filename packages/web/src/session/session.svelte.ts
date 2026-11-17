import { AWAY, UNREADABLE } from "../api/requests.ts";
import type { Credential } from "../api/credential.ts";
import { requestSignIn } from "../api/sign-in.ts";
import type { ChatSnapshot, Received } from "../api/stream.ts";
import { cancelSends, settle } from "../composer/outbox.svelte.ts";
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
  get credential(): Credential | null {
    const held = token;
    return held === null ? null : { token: held, refused: () => signOut(held) };
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

export type SignedIn = { kind: "refused"; words: string } | { kind: "stored" } | { kind: "held" };

export async function signIn(password: string): Promise<SignedIn> {
  const answer = await requestSignIn(password);
  if (answer.kind === "refused") return answer;
  if (storeToken(answer.token)) return { kind: "stored" };
  token = answer.token;
  return { kind: "held" };
}

function signOut(refused: string): void {
  const stored = storedToken();
  if (stored === refused) dropToken();
  token = stored === refused ? null : stored;
  if (token !== null) return;
  cancelSends();
  snapshot = null;
  notice = "";
  seen = new Map();
}

export function see(open: string): void {
  if (snapshot !== null) seen = seenAfter(seen, snapshot.conversations, open);
}

export function hear(said: Received, carried: string): void {
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
      signOut(carried);
      return;
  }
}
