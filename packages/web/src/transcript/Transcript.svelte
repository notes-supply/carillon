<script lang="ts">
  import { WINDOW_LIMIT, type Opened } from "../api/stream.ts";
  import OnItsWay from "../composer/OnItsWay.svelte";
  import { sentTo } from "../composer/outbox.svelte.ts";
  import { route, showEarlierTurns } from "../session/route.svelte.ts";
  import { followEnd } from "./follow-end.svelte.ts";
  import Turn from "./Turn.svelte";

  let { open, address }: { open: Opened | null; address: string } = $props();

  const turns = $derived(open?.kind === "recorded" ? open.turns : []);
  const sent = $derived(sentTo(address));
</script>

<div class="transcript" {@attach followEnd}>
  <div class="column">
    {#if open?.kind === "recorded" && open.earlier && route.turns < WINDOW_LIMIT}
      <button class="button" id="earlier" type="button" onclick={showEarlierTurns}>
        Show earlier turns
      </button>
    {/if}
    <ol class="turns" aria-label="Turns">
      {#each turns as turn (turn.turn_id)}
        <Turn {turn} />
      {/each}
      {#if sent?.kind === "on_its_way"}
        <OnItsWay message={sent} />
      {/if}
    </ol>
    {#if open !== null && turns.length === 0 && sent?.kind !== "on_its_way"}
      <p class="invitation">
        Write below to start this conversation. Attach files by choosing, dropping or pasting them.
        Type <code>/</code> to run one of the agent's commands.
      </p>
    {/if}
  </div>
</div>
