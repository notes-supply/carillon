<script lang="ts">
  import Composer from "./composer/Composer.svelte";
  import Rail from "./rail/Rail.svelte";
  import { titleOf } from "./rail/title.ts";
  import { addressOf, openedAt } from "./session/address.ts";
  import Door from "./session/Door.svelte";
  import { followView, route } from "./session/route.svelte.ts";
  import { session } from "./session/session.svelte.ts";
  import Words from "./session/Words.svelte";
  import Transcript from "./transcript/Transcript.svelte";

  followView();

  const snapshot = $derived(session.snapshot);
  const open = $derived(openedAt(snapshot?.open ?? null, route.address));
  const conversation = $derived(
    open?.kind === "recorded"
      ? open.conversation
      : snapshot?.conversations.find((listed) => addressOf(listed.key) === route.address),
  );
</script>

<svelte:head>
  <title>{conversation === undefined ? "Carillon" : titleOf(conversation)}</title>
</svelte:head>

{#if snapshot === null}
  <Door />
{:else}
  <div class="app in-room">
    <Rail conversations={snapshot.conversations} seen={session.seen} open={route.address} />
    <main class="room">
      <header class="room-head">
        <h2>{conversation === undefined ? "New conversation" : titleOf(conversation)}</h2>
      </header>
      <p class="notice" role="status" hidden={session.notice === ""}>
        <Words text={session.notice} />
      </p>
      {#if route.key !== null}
        {#key route.address}
          <Transcript {open} address={route.address} />
        {/key}
        <Composer key={route.key} limits={snapshot.limits} />
      {/if}
    </main>
  </div>
{/if}
