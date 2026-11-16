# Carillon's web chat, in Svelte 5

The page: the door, the rail, the transcript and the composer, against [the stream contract](src/api/README.md) in `src/api/stream.ts` and `src/api/requests.ts`.

`mise run build:web` writes a flat folder to `dist/`, which the provider serves as it is. The code has no comments, so this file holds what a comment would have said.

## Where each kind of file lives

| Path | Holds | Checked by |
|---|---|---|
| `src/<domain>/` | The page, by what each module is about: `api`, `session`, `rail`, `transcript`, `activity`, `composer`, `time`. A module's test sits beside it as `<module>.test.ts`. | The page's lint rules, `svelte-check`, and `vitest` |
| `fake/` | A provider that serves the contract and plays named scenarios. [Its README](fake/README.md) lists them. | The repository's Node lint and types, and `fake.test.ts` |
| `browser/` | What a person can see and do, as Playwright checks against the built page served by the fake. | `mise run test:browser` |
| `public/` | The page's one stylesheet, its typeface and its icon. | The formatter |

A test of one module is beside that module, so the lint that says where the page may use `JSON`, `Date` or a timer says the same of its tests. A browser check is about the built page as a whole and not about one module, and the fake is a Node program the page never loads, so both stay outside `src`, which holds only what Vite builds.

## The browser checks

- `browser/fixtures.ts` starts one fake provider for each Playwright worker, on a port of its own, serving `dist/`. The page and the API then share an origin, as with the real provider.
- A check begins with `fake.play(scenario)`, which starts that scenario over on the manual clock. It then acts as a person does: it types the password, clicks, types a message.
- `fake.step(label)` plays the scenario's next step and fails if that step is not the one named. A check never waits for time to pass. It steps the fake and waits for what the page shows.
- The page's clock is set to the scenario's beginning, 2026-11-15T12:00:00Z, and `clockAt(page, seconds)` moves it. Timers in the page still run. The locale is `en-GB` and the time zone UTC.
- Every check also fails when the page logs a refusal by the provider's policy, throws, or opens a dialog.
- `test-results/` holds a trace for each failed check.

To see the page as the provider serves it, run `vite build --watch` beside the fake. Vite's development server injects styles and scripts the provider's policy refuses.

## Where each fact lives

| Fact | Owner | Changes when |
|---|---|---|
| The token | `localStorage`, read through `session/token.ts`. `session/session.svelte.ts` holds it as state. | A sign-in, or `signed_out` on the stream |
| What the server knows | `session.snapshot`, one `$state.raw` value | A snapshot arrives and replaces it |
| The notice | `session.notice` | `unavailable` or `unreadable` sets it. The next snapshot empties it |
| What was seen of each conversation | `session.seen`, folded by `rail/unread.ts` | A snapshot arrives, or the address changes |
| The open conversation | The address. `session/route.svelte.ts` reads and writes it | `hashchange` |
| The window of turns | `session/route.svelte.ts`, for the conversation it was widened in | "Show earlier turns" |
| What is being written | `composer/outbox.svelte.ts`, in memory for each conversation. `composer/draft.ts` keeps a copy in `localStorage`, which also holds a message on its way until its receipt | Typing, a send, its receipt, a refusal, another tab clearing the copy |
| The files to send | `composer/outbox.svelte.ts`, in memory for each conversation | Attaching, removing, a send, a refusal |
| A message on its way, or its refusal | `composer/outbox.svelte.ts`, one for each conversation | A send, its stages, the snapshot that has its turn, and a sign-out, which cancels it |
| The agent's commands | `composer/commands.svelte.ts`, for each conversation | The first `/` written there asks the provider, once |
| The command chosen in the menu, and whether Escape hid it | `composer/Composer.svelte`, for the conversation it was set in | The arrows, Escape, writing |
| The time | `time/clock.svelte.ts` | Each second, and `clock.today` each day |

Everything a component draws is a `$derived` of these, or a call to one of the display functions: `ledger`, `outcome`, `size`, `span` and `when`.

## The rules

The page's rules are lints, in `lint/ast-grep/rules/web-*.yml` and `svelte-*.yml` and in the `overrides` of `.oxlintrc.json`. `mise run check:lint` proves them twice: `lint/ast-grep/tests/` gives each rule code it must accept and code it must refuse, and `scripts/test-web-rules.sh` scans a small page to show that each rule reaches the files it names, which a test of one snippet cannot.

The modules that may reach outside the page's state are a short list, written in three places: the `ignores` of `web-dom-write` and `svelte-effect`, the `.oxlintrc.json` overrides, and the list of findings in `scripts/test-web-rules.sh`. A new one is added to all three on purpose.

## What reaches outside the page's state

- `followView` in `session/route.svelte.ts` is the page's one `$effect`. It keeps one stream open for the view, which is the address and the window of turns, and aborts it when either changes.
- `followEnd` in `transcript/follow-end.svelte.ts` is an attachment on the element that scrolls. A `ResizeObserver` tells it the content changed, and a `scroll` listener tells it whether the person is still at the end.
- `focusedWhen` in `session/focus.svelte.ts` is an attachment that focuses the password field whenever the door waits for a password. The composer uses it for the message field, which each conversation gets anew, when `composer/desktop.ts` says the window is as wide as a desktop's.
- `composer/Composer.svelte` listens on `<svelte:window>` for `storage`, to store again what another tab cleared, and on `<svelte:document>` for a dropped file. Svelte adds and removes those listeners with the component.
- `api/send.ts` sends a message with the page's one `XMLHttpRequest`, the only request that reports its upload. It turns the request's events into a stream of stages, and aborts the request when the signal it was given does.
- `clock` subscribes to a one-second interval with `createSubscriber`, so the interval runs only while something on the page reads the time.

## What the tools do not tell you

- The message field grows by the stylesheet's `field-sizing: content`, which every engine has had since June 2026. An older browser keeps a field of one line that scrolls.
- A check that fills the browser's storage does so for real. One that makes storage refuse replaces `Storage`'s methods after the sign-in, because `session/token.ts` still reads storage unguarded when the page starts.
- A snapshot replaces every turn's object, so everything derived from a turn runs again for all of them. That is cheap except for `when()`, which costs about 40 µs a call. `time/Moment.svelte` therefore reads its prop through a `$derived` of the number alone, and reads `clock.today` in place of `clock.now`. Draw every time on the page with `Moment`.
- The effect in `followView` tracks only what it reads before the first `await`. `follow` and `watch` must read no state there.
- `hear` in `session/session.svelte.ts` is the one place a snapshot is taken in. It also calls `see` and `settle`, because both remember something the next snapshot no longer says. `readAddress` calls `see` too, so that a conversation opened and left before its snapshot arrived still counts as seen.
- "New conversation" links to `#new`, an address that names no conversation. Svelte's accessibility check refuses `href="#"`. `readAddress` then gives the address a new conversation to name.
- The transcript is inside `{#key route.address}`, so each conversation gets its own scrolling element and starts at its end.
