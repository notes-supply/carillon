<script lang="ts">
  import { ORIGIN } from "../api/origin.ts";
  import type { FileLink } from "../api/stream.ts";
  import { size } from "./files.ts";

  let { files, pictures = false }: { files: readonly FileLink[]; pictures?: boolean } = $props();
</script>

{#snippet named(file: FileLink)}
  <span>{file.name}</span>
  <span class="file-size">{size(file.size)}</span>
{/snippet}

<ul class="files">
  {#each files as file}
    {@const url = ORIGIN + file.url}
    {@const picture = file.type !== "application/octet-stream"}
    {#if pictures && picture}
      <li class="picture">
        <figure>
          <a href={url} target="_blank" rel="noopener">
            <img src={url} alt={file.name} loading="lazy" />
          </a>
          <figcaption>{file.name}, {size(file.size)}</figcaption>
        </figure>
      </li>
    {:else if picture}
      <li><a class="file" href={url} target="_blank" rel="noopener">{@render named(file)}</a></li>
    {:else}
      <li><a class="file" href={url} download={file.name}>{@render named(file)}</a></li>
    {/if}
  {/each}
</ul>
