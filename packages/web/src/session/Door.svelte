<script lang="ts">
  import { AWAY, SIGNED_OUT } from "../api/requests.ts";
  import { focusedWhen } from "./focus.svelte.ts";
  import { session, signIn } from "./session.svelte.ts";
  import Words from "./Words.svelte";

  let password = $state("");
  let refusal = $state("");
  let asking = $state(false);

  async function submitted(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    asking = true;
    refusal = await signIn(password);
    password = "";
    asking = false;
  }
</script>

<main class="door">
  <h1 class="wordmark">Carillon</h1>
  <p role="status"><Words text={session.token === null ? SIGNED_OUT : AWAY} /></p>
  {#if session.token === null}
    <form class="sign-in" onsubmit={submitted}>
      <input name="username" autocomplete="username" value="carillon" hidden />
      <label for="password">Password</label>
      <input
        id="password"
        type="password"
        autocomplete="current-password"
        required
        bind:value={password}
        {@attach focusedWhen(!asking)}
      />
      <button class="button primary">Sign in</button>
      <p role="alert"><Words text={refusal} /></p>
    </form>
  {/if}
</main>
