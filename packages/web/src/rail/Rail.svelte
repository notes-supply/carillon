<script lang="ts">
  import { WINDOW_LIMIT, type Conversation } from "../api/stream.ts";
  import { addressOf } from "../session/address.ts";
  import { focusedWhen, inViewWhen } from "../session/focus.svelte.ts";
  import { route, showEarlierConversations } from "../session/route.svelte.ts";
  import Moment from "../time/Moment.svelte";
  import { titleOf } from "./title.ts";
  import { unread, type Seen } from "./unread.ts";

  let {
    conversations,
    seen,
    open,
    more,
    custom,
  }: {
    conversations: readonly Conversation[];
    seen: Seen;
    open: string;
    more: boolean;
    custom: boolean;
  } = $props();
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
        <a
          class="chat"
          href="#{address}"
          aria-current={address === open ? "page" : undefined}
          {@attach inViewWhen(address === open)}
          {@attach focusedWhen(address === route.firstEarlierRow)}
        >
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
  {#if more && route.rows < WINDOW_LIMIT}
    <button class="button" id="more-chats" type="button" onclick={showEarlierConversations}>
      Show earlier conversations
    </button>
  {/if}
  {#if custom}
    <footer class="rail-note">
      This page is your own copy, served from the web provider's <code>site</code> folder in place of
      the release's.
    </footer>
  {/if}
</nav>
