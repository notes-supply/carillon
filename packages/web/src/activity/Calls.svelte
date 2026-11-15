<script lang="ts">
  import type { Line } from "./ledger.ts";

  let { lines }: { lines: readonly Line[] } = $props();
</script>

<ol class="calls">
  {#each lines as line}
    <li class="call">
      <span class="row" role="img" aria-label={line.mark}><i class="mark {line.mark}"></i></span>
      <span class="call-name">{line.name}</span>
      <span class="call-detail">{line.detail}</span>
      <span class="call-time" class:failed={line.mark === "failed"}>{line.time}</span>
    </li>
  {/each}
</ol>
