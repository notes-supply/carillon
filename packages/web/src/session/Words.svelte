<script lang="ts">
  let { text }: { text: string } = $props();
</script>

{#each text.split("`") as part, index}{#if index % 2 === 1}<code>{part}</code>{:else}{part}{/if}{/each}
