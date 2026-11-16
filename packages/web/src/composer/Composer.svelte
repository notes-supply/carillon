<script lang="ts">
  import type { Agent, ConversationKey, Limits } from "../api/stream.ts";
  import { addressOf } from "../session/address.ts";
  import { openConversation } from "../session/route.svelte.ts";
  import Words from "../session/Words.svelte";
  import Attached from "./Attached.svelte";
  import { overLimit } from "./limits.ts";
  import {
    attach,
    attachedTo,
    awaitingReceipt,
    carry,
    detach,
    draftOf,
    send,
    sentTo,
    storedElsewhere,
    write,
  } from "./outbox.svelte.ts";

  let {
    key,
    limits,
    agents,
    recorded,
  }: {
    key: ConversationKey;
    limits: Limits;
    agents: readonly Agent[];
    recorded: boolean;
  } = $props();

  let field = $state<HTMLTextAreaElement>();
  let chooser = $state<HTMLInputElement>();

  const address = $derived(addressOf(key));
  const sent = $derived(sentTo(address));
  const files = $derived(attachedTo(address));
  const busy = $derived(awaitingReceipt(address) !== undefined);
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

  function picked(event: Event & { currentTarget: HTMLSelectElement }): void {
    const next = { ...key, agent_id: event.currentTarget.value };
    carry(address, addressOf(next));
    openConversation(next);
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
        {#if !recorded && agents.length > 1}
          <select
            aria-label="Agent for this conversation"
            value={key.agent_id}
            disabled={busy}
            onchange={picked}
          >
            {#each agents as agent (agent.id)}
              <option value={agent.id}>{agent.name}</option>
            {/each}
          </select>
        {/if}
        <span class="hint">Enter sends. Shift+Enter starts a new line.</span>
        <button class="button primary" disabled={busy}>Send</button>
      </div>
    </form>
  </div>
</div>
