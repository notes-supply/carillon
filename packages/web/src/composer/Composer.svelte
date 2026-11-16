<script lang="ts">
  import type { ConversationKey, Limits } from "../api/stream.ts";
  import { addressOf } from "../session/address.ts";
  import Words from "../session/Words.svelte";
  import Attached from "./Attached.svelte";
  import { overLimit } from "./limits.ts";
  import {
    attach,
    attachedTo,
    awaitingReceipt,
    detach,
    draftOf,
    send,
    sentTo,
    storedElsewhere,
    write,
  } from "./outbox.svelte.ts";

  let { key, limits }: { key: ConversationKey; limits: Limits } = $props();

  let field = $state<HTMLTextAreaElement>();
  let chooser = $state<HTMLInputElement>();

  const address = $derived(addressOf(key));
  const sent = $derived(sentTo(address));
  const files = $derived(attachedTo(address));
  const refusal = $derived(
    overLimit(files, limits) ?? (sent?.kind === "refused" ? sent.words : ""),
  );

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
    void send(key, limits);
  }

  function chosen(event: Event & { currentTarget: HTMLInputElement }): void {
    attach(address, [...(event.currentTarget.files ?? [])]);
    event.currentTarget.value = "";
  }

  function pasted(event: ClipboardEvent): void {
    const carried = [...(event.clipboardData?.files ?? [])];
    if (carried.length > 0) event.preventDefault();
    attach(address, carried);
  }

  function dropped(event: DragEvent): void {
    event.preventDefault();
    attach(address, [...(event.dataTransfer?.files ?? [])]);
  }
</script>

<svelte:window onstorage={storedElsewhere} />
<svelte:document ondragover={(event) => event.preventDefault()} ondrop={dropped} />

<div class="composer-edge">
  <div class="column">
    <p class="error" role="alert" hidden={refusal === ""}>
      <Words text={refusal} />
    </p>
    <form class="composer" onsubmit={submitted}>
      <Attached {files} remove={(file) => detach(address, file)} after={field} />
      <textarea
        rows="1"
        aria-label="Message"
        placeholder="Write to Carillon"
        value={draftOf(address)}
        bind:this={field}
        oninput={typed}
        onkeydown={pressed}
        onpaste={pasted}
      ></textarea>
      <div class="composer-foot">
        <button class="button" type="button" onclick={() => chooser?.click()}>Attach files</button>
        <input type="file" multiple hidden bind:this={chooser} onchange={chosen} />
        <span class="hint">Enter sends. Shift+Enter starts a new line.</span>
        <button class="button primary" disabled={awaitingReceipt(address) !== undefined}>Send</button
        >
      </div>
    </form>
  </div>
</div>
