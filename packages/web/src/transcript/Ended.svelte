<script lang="ts">
  import Calls from "../activity/Calls.svelte";
  import Moment from "../time/Moment.svelte";
  import Files from "./Files.svelte";
  import { outcome, type Ended } from "./outcome.ts";
  import Prose from "./Prose.svelte";

  let { progress, acceptedAt }: { progress: Ended; acceptedAt: number } = $props();

  const ending = $derived(progress.ending);
  const result = $derived(outcome(progress, acceptedAt));
</script>

{#snippet line()}
  {result.label}
  {#if ending.kind !== "steered"}
    <Moment at={progress.finished_at} />{#if result.took !== ""}, {result.took}{/if}
  {/if}
{/snippet}

<div class="turn-body" class:interrupted={ending.kind === "interrupted"}>
  {#if ending.kind === "answered"}
    <Prose html={ending.html} />
    {#if ending.files.length > 0}
      <Files files={ending.files} pictures />
    {/if}
  {/if}
  {#if result.did.length > 0}
    <details class="did">
      <summary class="outcome">{@render line()}</summary>
      <Calls lines={result.did} />
      {#if result.unlisted}
        <p class="ledger-note">The calls before these are not listed.</p>
      {/if}
    </details>
  {:else}
    <p class="outcome">{@render line()}</p>
  {/if}
  {#if ending.kind === "interrupted"}
    <Prose html={ending.html} explanation />
  {/if}
</div>
