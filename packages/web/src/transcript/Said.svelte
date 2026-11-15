<script lang="ts">
  import type { MessageSource } from "../api/stream.ts";
  import Moment from "../time/Moment.svelte";
  import Files from "./Files.svelte";

  let { message, at }: { message: MessageSource; at: number } = $props();
</script>

<div class="said">
  {#if message.text !== ""}
    <p class="said-text">{message.text.replaceAll("\r\n", "\n")}</p>
  {/if}
  {#if message.files.length > 0}
    <Files files={message.files} />
  {/if}
  <div class="said-foot"><Moment {at} /></div>
</div>
