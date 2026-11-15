<script lang="ts">
  import Ledger from "../activity/Ledger.svelte";
  import type { Turn } from "../api/stream.ts";
  import Moment from "../time/Moment.svelte";
  import Ended from "./Ended.svelte";
  import Said from "./Said.svelte";

  let { turn }: { turn: Turn } = $props();
</script>

<li class="turn">
  {#if turn.source.kind === "message"}
    <Said message={turn.source} at={turn.accepted_at} />
  {:else}
    <p class="opened">
      <strong>{turn.source.label ?? "A turn of its own"}</strong>
      Opened by Carillon, not by a message.
      <Moment at={turn.accepted_at} />
    </p>
  {/if}
  {#if turn.progress.state === "ended"}
    <Ended progress={turn.progress} acceptedAt={turn.accepted_at} />
  {:else}
    <Ledger progress={turn.progress} acceptedAt={turn.accepted_at} />
  {/if}
</li>
