<script lang="ts">
  import { size } from "../transcript/files.ts";
  import type { OnItsWay } from "./outbox.svelte.ts";

  const STANDS: Record<OnItsWay["stage"], string> = {
    uploading: "Sending",
    receiving: "Carillon is receiving it.",
    retrying: "Carillon is not answering. This message will be sent when it does.",
  };

  let { message }: { message: OnItsWay } = $props();
</script>

<li class="turn">
  <div class="said pending">
    {#if message.text !== ""}
      <p class="said-text">{message.text}</p>
    {/if}
    {#if message.files.length > 0}
      <ul class="files">
        {#each message.files as file (file)}
          <li class="file">
            <span>{file.name}</span>
            <span class="file-size">{size(file.size)}</span>
          </li>
        {/each}
      </ul>
    {/if}
    <div class="said-foot" role="status">{STANDS[message.stage]}</div>
  </div>
</li>
