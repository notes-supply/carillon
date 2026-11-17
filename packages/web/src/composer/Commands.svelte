<script lang="ts">
  import type { Command } from "../api/requests.ts";

  let {
    offers,
    choice,
    take,
  }: {
    offers: readonly Command[];
    choice: number;
    take: (command: Command) => void;
  } = $props();

  function pressed(event: MouseEvent, command: Command): void {
    event.preventDefault();
    take(command);
  }
</script>

<ul class="commands" role="listbox" aria-label="Commands" hidden={offers.length === 0}>
  {#each offers as command, index}
    <li
      class="command"
      role="option"
      aria-selected={index === choice}
      onmousedown={(event) => pressed(event, command)}
    >
      <strong>/{command.name}</strong>
      {[command.description, command.hint].filter((part) => part !== undefined).join(" ")}
    </li>
  {/each}
</ul>
