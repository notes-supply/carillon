<script lang="ts">
  import { focusMessage } from "../session/focus.svelte.ts";
  import { size } from "../transcript/files.ts";

  let { files, remove }: { files: readonly File[]; remove: (file: File) => void } = $props();

  let removers = $state<(HTMLButtonElement | null)[]>([]);

  function removed(file: File, index: number): void {
    const next = removers[index + 1];
    if (next) next.focus();
    else focusMessage();
    remove(file);
  }
</script>

<ul class="files" aria-label="Files to send" hidden={files.length === 0}>
  {#each files as file, index (file)}
    <li class="file">
      <span>{file.name}</span>
      <span class="file-size">{size(file.size)}</span>
      <button
        type="button"
        aria-label="Remove {file.name}"
        bind:this={removers[index]}
        onclick={() => removed(file, index)}>×</button
      >
    </li>
  {/each}
</ul>
