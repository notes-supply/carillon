<script lang="ts">
  import type { ConversationKey } from "../api/stream.ts";
  import { addressOf } from "../session/address.ts";
  import Words from "../session/Words.svelte";
  import { draftOf, send, sentTo, write } from "./outbox.svelte.ts";

  let { key }: { key: ConversationKey } = $props();

  const address = $derived(addressOf(key));
  const sent = $derived(sentTo(address));

  function typed(event: Event & { currentTarget: HTMLTextAreaElement }): void {
    write(address, event.currentTarget.value);
  }

  function pressed(event: KeyboardEvent & { currentTarget: HTMLTextAreaElement }): void {
    if (event.key !== "Enter" || event.shiftKey || event.isComposing) return;
    event.preventDefault();
    event.currentTarget.form?.requestSubmit();
  }

  function submitted(event: SubmitEvent): void {
    event.preventDefault();
    void send(key);
  }
</script>

<div class="composer-edge">
  <div class="column">
    <p class="error" role="alert" hidden={sent?.kind !== "refused"}>
      {#if sent?.kind === "refused"}
        <Words text={sent.words} />
      {/if}
    </p>
    <form class="composer" onsubmit={submitted}>
      <textarea
        rows="1"
        aria-label="Message"
        placeholder="Write to Carillon"
        value={draftOf(address)}
        oninput={typed}
        onkeydown={pressed}
      ></textarea>
      <div class="composer-foot">
        <span class="hint">Enter sends. Shift+Enter starts a new line.</span>
        <button class="button primary" disabled={sent?.kind === "on_its_way"}>Send</button>
      </div>
    </form>
  </div>
</div>
