<script lang="ts">
  let { html, explanation = false }: { html: string; explanation?: boolean } = $props();
</script>

<div class={explanation ? "explanation" : "prose"}>{@html html}</div>
