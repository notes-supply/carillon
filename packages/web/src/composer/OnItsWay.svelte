<script lang="ts">
  import { size } from "../transcript/files.ts";
  import type { OnItsWay } from "./outbox.svelte.ts";

  let { message }: { message: OnItsWay } = $props();

  const stage = $derived(message.stage);
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
    <div class="said-foot" role="status">
      {#if stage.kind === "receiving"}
        Carillon is receiving it.
      {:else if stage.kind === "retrying"}
        Carillon is not answering. This message will be sent when it does.
      {:else if message.files.length === 0}
        Sending
      {:else}
        Sending{#if stage.total > 0}, {Math.floor((stage.sent / stage.total) * 100)}%{/if}
        <progress max={Math.max(stage.total, 1)} value={stage.sent}></progress>
      {/if}
    </div>
  </div>
</li>
