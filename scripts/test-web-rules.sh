#!/bin/sh
# Proves what `ast-grep test` cannot: that the page's rules reach the files they
# name. A rule test gives a rule one snippet with no path, so it says nothing
# of a rule that is only a path, of the list of modules a rule lets through, or
# of a TypeScript rule reading a component's script. This scans a small page
# under the repository's own sgconfig.yml and rules, and compares every finding
# with the list below.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TEST_ROOT=$(mktemp -d)
trap 'rm -rf -- "$TEST_ROOT"' EXIT
trap 'exit 1' HUP INT TERM
cp "$ROOT/sgconfig.yml" "$TEST_ROOT/"
mkdir "$TEST_ROOT/lint"
cp -R "$ROOT/lint/ast-grep" "$TEST_ROOT/lint/"
SRC=$TEST_ROOT/packages/web/src

page() {
  mkdir -p "$(dirname -- "$SRC/$1")"
  cat >"$SRC/$1"
}

page main.ts <<'FILE'
import { mount } from "svelte";
import App from "./App.svelte";

mount(App, { target: document.querySelector("main") ?? document.body });
FILE
page rail/Rail.svelte <<'FILE'
<script lang="ts">
  let { titles }: { titles: string[] } = $props();
  const count = $derived(titles.length);
</script>

<nav aria-label="Conversations">{count}</nav>
FILE
page session/route.svelte.ts <<'FILE'
export function followView(open: () => void): void {
  $effect(open);
}
FILE
page session/focus.svelte.ts <<'FILE'
export function focused(field: HTMLInputElement): void {
  field.setAttribute("autofocus", "");
}
FILE
page transcript/Prose.svelte <<'FILE'
<script lang="ts">
  let { html }: { html: string } = $props();
</script>

<div class="prose">{@html html}</div>
FILE

for ending in js mjs cjs jsx; do
  page "rail/legacy.$ending" <<'FILE'
export const double = (count) => count * 2;
FILE
done

page widgets/Thing.svelte <<'FILE'
<script lang="ts">
  let { count }: { count: number } = $props();
</script>

<p>{count}</p>
FILE
page widgets/thing.ts <<'FILE'
export const limit = 3;
FILE
page rail/helpers.ts <<'FILE'
export const limit = 3;
FILE
page rail/shared/Badge.svelte <<'FILE'
<script lang="ts">
  let { count }: { count: number } = $props();
</script>

<p>{count}</p>
FILE

page rail/Unread.svelte <<'FILE'
<script lang="ts">
  let { count }: { count: number } = $props();
  let doubled = $state(0);

  // Twice the count.
  $effect(() => {
    doubled = count * 2;
  });
</script>

<p>{doubled}</p>
FILE

page rail/unread.svelte.ts <<'FILE'
export function followView(open: () => void): void {
  $effect(open);
}
FILE
page rail/draw.ts <<'FILE'
export function focused(field: HTMLInputElement): void {
  field.setAttribute("autofocus", "");
}
FILE
page rail/Title.svelte <<'FILE'
<script lang="ts">
  let { html }: { html: string } = $props();
</script>

<div class="prose">{@html html}</div>
FILE

cat >"$TEST_ROOT/expected" <<'FINDINGS'
rail/Title.svelte svelte-raw-html
rail/Unread.svelte svelte-effect
rail/Unread.svelte web-no-comments
rail/draw.ts web-dom-write
rail/helpers.ts web-junk-drawer
rail/legacy.cjs web-typescript-only-javascript
rail/legacy.js web-typescript-only
rail/legacy.jsx web-typescript-only-javascript
rail/legacy.mjs web-typescript-only
rail/shared/Badge.svelte web-junk-drawer
rail/shared/Badge.svelte web-junk-drawer-svelte
rail/unread.svelte.ts svelte-effect
widgets/Thing.svelte web-domain-directory
widgets/Thing.svelte web-domain-directory-svelte
widgets/thing.ts web-domain-directory
FINDINGS

# `file:line:column: severity[rule]: message`, one finding a line.
(cd "$TEST_ROOT" && ast-grep scan --report-style short 2>/dev/null) |
  sed -n 's|^packages/web/src/\([^:]*\):[0-9]*:[0-9]*: [a-z]*\[\([a-z-]*\)\]: .*$|\1 \2|p' |
  LC_ALL=C sort -u >"$TEST_ROOT/found"
if ! diff -u "$TEST_ROOT/expected" "$TEST_ROOT/found"; then
  echo "the page's rules do not report what scripts/test-web-rules.sh lists (-), or report more (+)" >&2
  exit 1
fi
printf '%s\n' "the page's rules reach the files they name"
