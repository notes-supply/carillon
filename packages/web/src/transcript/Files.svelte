<script lang="ts">
  import { ORIGIN } from "../api/origin.ts";
  import type { FileLink } from "../api/stream.ts";
  import { size } from "./files.ts";

  let { files, pictures = false }: { files: readonly FileLink[]; pictures?: boolean } = $props();
</script>

<ul class="files">
  {#each files as file (file.url)}
    {@const url = ORIGIN + file.url}
    {#if pictures && file.type !== "application/octet-stream"}
      <li class="picture">
        <figure>
          <a href={url} target="_blank" rel="noopener">
            <img src={url} alt={file.name} loading="lazy" />
          </a>
          <figcaption>{file.name}, {size(file.size)}</figcaption>
        </figure>
      </li>
    {:else}
      <li>
        <a class="file" href={url} download={file.name}>
          <span>{file.name}</span>
          <span class="file-size">{size(file.size)}</span>
        </a>
      </li>
    {/if}
  {/each}
</ul>
