import { WINDOW_START, mintAudienceId, widened, type View } from "../api/requests.ts";
import type { ChatSnapshot, ConversationKey } from "../api/stream.ts";
import { watch } from "../api/watch.ts";
import { addressOf, keyAt } from "./address.ts";
import { hear, see, session } from "./session.svelte.ts";

type EarlierRows = { state: "asked"; after: number } | { state: "listed"; first: string };

let hash = $state(location.hash);
let earlier = $state.raw({ address: "", turns: WINDOW_START });
let rows = $state(WINDOW_START);
let pane = $state<"room" | "conversations">("room");
let earlierRows = $state.raw<EarlierRows | null>(null);

const key = $derived(keyAt(hash.slice(1)));
const address = $derived(key === null ? "" : addressOf(key));
const turns = $derived(earlier.address === address ? earlier.turns : WINDOW_START);
const view = $derived<View>({
  conversations: rows,
  open: key === null ? null : { key, turns },
});

function readAddress(): void {
  const first = session.snapshot?.agents[0];
  if (keyAt(location.hash.slice(1)) === null && first !== undefined)
    location.replace(`#${addressOf({ agent_id: first.id, audience_id: mintAudienceId() })}`);
  hash = location.hash;
  see(address);
}

window.addEventListener("hashchange", readAddress);

export const route = {
  get key(): ConversationKey | null {
    return key;
  },
  get address(): string {
    return address;
  },
  get turns(): number {
    return turns;
  },
  get rows(): number {
    return rows;
  },
  get pane(): "room" | "conversations" {
    return pane;
  },
  get firstEarlierRow(): string {
    return earlierRows?.state === "listed" ? earlierRows.first : "";
  },
};

export function openConversation(next: ConversationKey): void {
  location.replace(`#${addressOf(next)}`);
}

export function showEarlierTurns(): void {
  earlier = { address, turns: widened(turns) };
}

export function showRoom(): void {
  pane = "room";
}

export function showConversations(): void {
  pane = "conversations";
}

export function showEarlierConversations(): void {
  earlierRows = { state: "asked", after: session.snapshot?.conversations.length ?? 0 };
  rows = widened(rows);
}

function listEarlierRows(asked: View, snapshot: ChatSnapshot): void {
  if (earlierRows?.state !== "asked" || asked.conversations <= earlierRows.after) return;
  const first = snapshot.conversations[earlierRows.after];
  earlierRows = first === undefined ? null : { state: "listed", first: addressOf(first.key) };
}

async function follow(asked: View, token: string, signal: AbortSignal): Promise<void> {
  for await (const said of watch(asked, token, signal)) {
    if (signal.aborted) return;
    hear(said);
    if (said.kind !== "snapshot") continue;
    listEarlierRows(asked, said.snapshot);
    if (asked.open === null) readAddress();
  }
}

export function followView(): void {
  $effect(() => {
    const token = session.token;
    if (token === null) return undefined;
    const stream = new AbortController();
    void follow(view, token, stream.signal);
    return () => stream.abort();
  });
}
