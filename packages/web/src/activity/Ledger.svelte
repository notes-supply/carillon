<script lang="ts">
  import type { Control, MessageSource } from "../api/stream.ts";
  import { focusMessage } from "../session/focus.svelte.ts";
  import { session } from "../session/session.svelte.ts";
  import Words from "../session/Words.svelte";
  import { clock } from "../time/clock.svelte.ts";
  import Calls from "./Calls.svelte";
  import { offered, press, type Target } from "./controls.svelte.ts";
  import { LEGEND, ledger, type Working } from "./ledger.ts";

  let {
    progress,
    acceptedAt,
    turnId,
    message,
  }: {
    progress: Working;
    acceptedAt: number;
    turnId: string;
    message: MessageSource | undefined;
  } = $props();

  const shown = $derived(ledger(progress, acceptedAt, clock.now));
  const target = $derived<Target | undefined>(
    message === undefined ? undefined : { turnId, message, waiting: shown.waiting },
  );
  const controls = $derived(
    progress.state === "running" && target !== undefined
      ? offered(target, progress.controls, clock.now)
      : { offers: [], refusal: "" },
  );

  function pressed(action: Control): void {
    if (target !== undefined && session.token !== null)
      void press(target, action, session.token, clock.now);
    focusMessage();
  }
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
    {#each controls.offers as offer (offer.action)}
      <button
        class="button"
        type="button"
        title={offer.title}
        disabled={offer.held}
        onclick={() => pressed(offer.action)}
      >
        {offer.label}
      </button>
    {/each}
  </div>
  {#if controls.refusal !== ""}
    <p class="error" role="alert"><Words text={controls.refusal} /></p>
  {/if}
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
