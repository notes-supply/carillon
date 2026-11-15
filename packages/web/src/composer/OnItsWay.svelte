<script lang="ts">
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
    <p class="said-text">{message.text}</p>
    <div class="said-foot" role="status">{STANDS[message.stage]}</div>
  </div>
</li>
