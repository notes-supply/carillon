<script lang="ts">
  import type { Conversation } from "../api/stream.ts";
  import { addressOf } from "../session/address.ts";
  import Moment from "../time/Moment.svelte";
  import { titleOf } from "./title.ts";
  import { unread, type Seen } from "./unread.ts";

  let {
    conversations,
    seen,
    open,
  }: { conversations: readonly Conversation[]; seen: Seen; open: string } = $props();
</script>

<nav class="rail" aria-label="Conversations">
  <header class="rail-head">
    <h1 class="wordmark">Carillon</h1>
    <a class="button" href="#new">New conversation</a>
  </header>
  <ol class="chats">
    {#each conversations as conversation (addressOf(conversation.key))}
      {@const address = addressOf(conversation.key)}
      <li>
        <a class="chat" href="#{address}" aria-current={address === open ? "page" : undefined}>
          <span class="chat-title">{titleOf(conversation)}</span>
          <span class="chat-when"><Moment at={conversation.active_at} /></span>
          {#if conversation.at_work > 0}
            <span class="row" role="img" aria-label="At work">
              <i class="mark running"></i>
              <i class="mark running"></i>
              <i class="mark running"></i>
            </span>
          {:else if address !== open && unread(seen, conversation)}
            <i class="unread" role="img" aria-label="New"></i>
          {/if}
        </a>
      </li>
    {/each}
  </ol>
  {#if conversations.length === 0}
    <p class="rail-note">No conversations yet. The ones you start are listed here, latest first.</p>
  {/if}
</nav>
