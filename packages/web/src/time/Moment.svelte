<script lang="ts">
  import { clock } from "./clock.svelte.ts";
  import { when } from "./words.ts";

  let { at }: { at: number } = $props();

  const second = $derived(at);
  const date = $derived(new Date(second * 1000));
  const words = $derived(when(second, clock.today));
</script>

<time datetime={date.toISOString()} title={date.toLocaleString()}>{words}</time>
