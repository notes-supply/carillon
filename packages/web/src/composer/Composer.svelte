<script lang="ts">
  import type { Command } from "../api/requests.ts";
  import type { Agent, ConversationKey, Limits } from "../api/stream.ts";
  import { addressOf } from "../session/address.ts";
  import { focusedWhen } from "../session/focus.svelte.ts";
  import { openConversation } from "../session/route.svelte.ts";
  import { session } from "../session/session.svelte.ts";
  import Words from "../session/Words.svelte";
  import Attached from "./Attached.svelte";
  import Commands from "./Commands.svelte";
  import { loadCommands, offered } from "./commands.svelte.ts";
  import { onDesktop } from "./desktop.ts";
  import { takesDrops } from "./drop.svelte.ts";
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
  let pointed = $state.raw({ address: "", choice: 0 });
  let dismissedAt = $state("");

  const address = $derived(addressOf(key));
  const sent = $derived(sentTo(address));
  const files = $derived(attachedTo(address));
  const busy = $derived(awaitingReceipt(address) !== undefined);
  const text = $derived(draftOf(address));
  const offers = $derived(dismissedAt === address ? [] : offered(address, text));
  const choice = $derived(
    pointed.address === address ? Math.min(pointed.choice, Math.max(offers.length - 1, 0)) : 0,
  );
  const refusal = $derived(
    overLimit(files, limits) ?? (sent?.kind === "refused" ? sent.words : ""),
  );

  function wrote(written: string): void {
    dismissedAt = "";
    write(address, written);
    if (written.startsWith("/")) void loadCommands(key, session.token);
  }

  function take(command: Command): void {
    wrote(`/${command.name} `);
  }

  function pressed(event: KeyboardEvent & { currentTarget: HTMLTextAreaElement }): void {
    if (event.isComposing) return;
    const offer = offers[choice];
    const enter = event.key === "Enter" && !event.shiftKey;
    if (offer !== undefined && (event.key === "ArrowDown" || event.key === "ArrowUp")) {
      event.preventDefault();
      const step = event.key === "ArrowDown" ? 1 : offers.length - 1;
      pointed = { address, choice: (choice + step) % offers.length };
    } else if (offer !== undefined && event.key === "Escape") {
      dismissedAt = address;
    } else if (
      offer !== undefined &&
      (event.key === "Tab" || (enter && text !== `/${offer.name}`))
    ) {
      event.preventDefault();
      take(offer);
    } else if (enter) {
      event.preventDefault();
      event.currentTarget.form?.requestSubmit();
    }
  }

  function submitted(event: SubmitEvent): void {
    event.preventDefault();
    void send(key, session.token, limits);
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
</script>

<div class="composer-edge" {@attach takesDrops((dropped) => attach(address, dropped))}>
  <div class="column">
    <p class="error" role="alert" hidden={refusal === ""}>
      <Words text={refusal} />
    </p>
    <Commands {offers} {choice} {take} />
    <form class="composer" onsubmit={submitted}>
      <Attached {files} remove={(file) => detach(address, file)} after={field} />
      {#key key.audience_id}
        <textarea
          rows="1"
          aria-label="Message"
          placeholder="Write to Carillon"
          value={text}
          bind:this={field}
          oninput={(event) => wrote(event.currentTarget.value)}
          onkeydown={pressed}
          onpaste={pasted}
          {@attach focusedWhen(onDesktop())}
        ></textarea>
      {/key}
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
