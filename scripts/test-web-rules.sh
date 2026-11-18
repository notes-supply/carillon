#!/bin/sh
# Proves what `ast-grep test` cannot. A rule test gives a rule one snippet with
# no path, so it says nothing of a rule that is only a path, of the list of
# modules a rule lets through, or of a TypeScript rule reading a component's
# script. This scans a small page, and two files of the web provider's crate,
# under the repository's own sgconfig.yml and rules, and compares every finding
# with the list below. It then scans the real page with the `ignores` of the
# rules in web-outside.yml, svelte-effect.yml and web-dom-write.yml left out,
# so that each module those rules let through is one that needs it, and the
# README's table of them names the same modules.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TEST_ROOT=$(mktemp -d)
trap 'rm -rf -- "$TEST_ROOT"' EXIT
trap 'exit 1' HUP INT TERM
PAGE=$TEST_ROOT/page
REAL=$TEST_ROOT/real
mkdir -p "$PAGE/lint" "$REAL/lint/ast-grep/rules" "$REAL/packages/web"
cp "$ROOT/sgconfig.yml" "$PAGE/"
cp "$ROOT/sgconfig.yml" "$REAL/"
cp -R "$ROOT/lint/ast-grep" "$PAGE/lint/"
cp -R "$ROOT/packages/web/src" "$REAL/packages/web/"
WRAPPERS='web-outside.yml svelte-effect.yml web-dom-write.yml'

# `file rule` for each finding in the tree $1, with a file of the page named
# from packages/web/src, from ast-grep's
# `file:line:column: severity[rule]: message` lines.
findings() {
  (cd "$1" && ast-grep scan --report-style short 2>/dev/null) |
    sed -n 's|^\(packages/web/src/\)\{0,1\}\([^:]*\):[0-9]*:[0-9]*: [a-z]*\[\([a-z-]*\)\]: .*$|\2 \3|p' |
    LC_ALL=C sort -u
}

put() {
  mkdir -p "$(dirname -- "$PAGE/$1")"
  cat >"$PAGE/$1"
}

page() {
  put "packages/web/src/$1"
}

crate() {
  put "crates/carillon-web/src/$1"
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
page time/clock.svelte.ts <<'FILE'
export function ticking(tick: () => void): void {
  $effect(tick);
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
put packages/web/fake/legacy.js <<'FILE'
export const double = (count) => count * 2;
FILE
put packages/web/public/style.css <<'FILE'
/* The page's ink. */
:root {
  --ink: #000;
}
FILE
put packages/web/svelte.config.js <<'FILE'
export default {
  compilerOptions: { runes: true, css: "external" },
};
FILE

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

page rail/reach.ts <<'FILE'
import type { Attachment } from "svelte/attachments";
import { on } from "svelte/events";
import { createSubscriber } from "svelte/reactivity";

export const hover: Attachment<HTMLElement> = (node) => {
  node.ownerDocument.defaultView?.addEventListener("resize", () => undefined);
  new ResizeObserver(() => undefined).observe(node);
};

export const wide = matchMedia("(min-width: 46rem)").matches;
export const late = AbortSignal.timeout(1000);
export const later = new MessageChannel();
export const shown = createSubscriber((update) => on(window, "focus", update));
FILE
page rail/Reach.svelte <<'FILE'
<script lang="ts">
  let { count }: { count: number } = $props();
  let shown = $state(0);

  setTimeout(() => (shown = count), 1000);
  setInterval(() => (shown += 1), 1000);
  requestAnimationFrame(() => (shown = 0));
</script>

<button onclick={() => addEventListener("focus", () => (shown = 0))}>{shown}</button>
FILE

crate markdown/links.rs <<'FILE'
pub fn rewrite(html: String) -> String {
    html.replace("<a ", "<a rel=\"noopener\" ")
}
FILE
crate send.rs <<'FILE'
pub fn unquoted(name: &str) -> String {
    name.replace("%22", "\"")
}
FILE

cat >"$TEST_ROOT/expected" <<'FINDINGS'
crates/carillon-web/src/markdown/links.rs rust-web-markdown-rewrite
main.ts web-dom-write
packages/web/fake/legacy.js web-typescript-only
packages/web/public/style.css web-no-comments-css
rail/Reach.svelte web-deferred
rail/Reach.svelte web-interval
rail/Reach.svelte web-markup-outside
rail/Reach.svelte web-timeout
rail/Title.svelte svelte-raw-html
rail/Unread.svelte svelte-effect
rail/Unread.svelte web-no-comments
rail/draw.ts web-dom-write
rail/helpers.ts web-junk-drawer
rail/legacy.cjs web-typescript-only-javascript
rail/legacy.js web-typescript-only
rail/legacy.jsx web-typescript-only-javascript
rail/legacy.mjs web-typescript-only
rail/reach.ts web-attachment
rail/reach.ts web-deferred
rail/reach.ts web-listener
rail/reach.ts web-media-query
rail/reach.ts web-observer
rail/reach.ts web-subscriber
rail/reach.ts web-svelte-events
rail/reach.ts web-task
rail/shared/Badge.svelte web-junk-drawer
rail/shared/Badge.svelte web-junk-drawer-svelte
rail/unread.svelte.ts svelte-effect
session/focus.svelte.ts web-dom-write
time/clock.svelte.ts svelte-effect
widgets/Thing.svelte web-domain-directory
widgets/Thing.svelte web-domain-directory-svelte
widgets/thing.ts web-domain-directory
FINDINGS

findings "$PAGE" >"$TEST_ROOT/found"
if ! diff -u "$TEST_ROOT/expected" "$TEST_ROOT/found"; then
  echo "the page's rules do not report what scripts/test-web-rules.sh lists (-), or report more (+)" >&2
  exit 1
fi

for rules in $WRAPPERS; do
  awk '/^ignores:/ { skip = 1; next } /^[a-z]/ { skip = 0 } !skip' \
    "$ROOT/lint/ast-grep/rules/$rules" >"$REAL/lint/ast-grep/rules/$rules"
  awk '/^id:/ { id = $2 } /^ignores:/ { listed = 1; next } /^[a-z]/ { listed = 0 }
    listed { sub("^  - packages/web/src/", ""); print $0, id }' "$ROOT/lint/ast-grep/rules/$rules"
done | LC_ALL=C sort -u >"$TEST_ROOT/let-through"
findings "$REAL" >"$TEST_ROOT/needed"
if ! diff -u "$TEST_ROOT/let-through" "$TEST_ROOT/needed"; then
  echo "a rule lets a module through for what it does not do (-), or a module does what its rule refuses (+)" >&2
  exit 1
fi

cut -d ' ' -f 1 "$TEST_ROOT/let-through" | LC_ALL=C sort -u >"$TEST_ROOT/modules"
sed -n '/^## What reaches outside the page.s state$/,/^## /s/^| .\([a-z/.-]*\.ts\). |.*$/\1/p' \
  "$ROOT/packages/web/README.md" | LC_ALL=C sort >"$TEST_ROOT/tabled"
if ! diff -u "$TEST_ROOT/modules" "$TEST_ROOT/tabled"; then
  echo "packages/web/README.md's table leaves out a module the rules let through (-), or names one they do not (+)" >&2
  exit 1
fi
printf '%s\n' "the page's rules reach the files they name, and let through only the modules that need them"
