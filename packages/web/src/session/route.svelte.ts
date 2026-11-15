import { WINDOW_START, mintAudienceId, widened, type View } from "../api/requests.ts";
import type { ConversationKey } from "../api/stream.ts";
import { watch } from "../api/watch.ts";
import { addressOf, keyAt } from "./address.ts";
import { hear, see, session } from "./session.svelte.ts";

let hash = $state(location.hash);
let earlier = $state.raw({ address: "", turns: WINDOW_START });

const key = $derived(keyAt(hash.slice(1)));
const address = $derived(key === null ? "" : addressOf(key));
const turns = $derived(earlier.address === address ? earlier.turns : WINDOW_START);
const view = $derived<View>({
  conversations: WINDOW_START,
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
};

export function showEarlierTurns(): void {
  earlier = { address, turns: widened(turns) };
}

async function follow(asked: View, token: string, signal: AbortSignal): Promise<void> {
  for await (const said of watch(asked, token, signal)) {
    if (signal.aborted) return;
    hear(said);
    if (said.kind === "snapshot" && asked.open === null) readAddress();
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
