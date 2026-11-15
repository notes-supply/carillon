<script lang="ts">
  import { clock } from "../time/clock.svelte.ts";
  import Calls from "./Calls.svelte";
  import { LEGEND, ledger, type Working } from "./ledger.ts";

  let { progress, acceptedAt }: { progress: Working; acceptedAt: number } = $props();

  const shown = $derived(ledger(progress, acceptedAt, clock.now));
</script>

<section class="ledger" aria-label="This turn is at work">
  <div class="ledger-head">
    <span class="row" role="img" aria-label={shown.counted} title={LEGEND}>
      {#each shown.marks as mark}
        <i class="mark {mark}"></i>
      {/each}
      <i class="mark now" class:sounding={!shown.waiting}></i>
    </span>
    <span class="phase">{shown.phase}</span>
    <span class="tally">{shown.tally}</span>
  </div>
  {#if shown.headline !== ""}
    <p class="headline">{shown.headline}</p>
  {/if}
  {#if shown.calls.length > 0}
    <Calls lines={shown.calls} />
  {/if}
  {#if shown.work.length > 0}
    <Calls lines={shown.work} />
  {/if}
  {#each shown.notes as note}
    <p class="ledger-note">{note}</p>
  {/each}
</section>
