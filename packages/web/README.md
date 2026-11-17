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
| The token | `localStorage`, read through `session/token.ts`, for every tab. `session/session.svelte.ts` holds it as state, and hands it to each request as a `Credential` whose `refused` is its sign-out. | A sign-in stores it and loads the page again, in this tab and in every tab at the door. `signed_out` on the stream, or a request answered 401, drops it only when the refused token is still the stored one. Otherwise the tab goes on with the stored one. |
| What the server knows | `session.snapshot`, one `$state.raw` value | A snapshot arrives and replaces it |
| The notice | `session.notice` | `unavailable` or `unreadable` sets it. The next snapshot empties it |
| What was seen of each conversation | `session.seen`, folded by `rail/unread.ts` | A snapshot arrives, or the address changes |
| The open conversation | The address. `session/route.svelte.ts` reads and writes it | `hashchange` |
| The window of turns | `session/route.svelte.ts`, for the conversation it was widened in | "Show earlier turns" |
| The window of conversations, and the first row a wider one added | `session/route.svelte.ts` | "Show earlier conversations", and the snapshot that answers it |
| Which pane a narrow window shows | `session/route.svelte.ts` | Conversations, or a conversation chosen in the list |
| Whether the tab is shown | The document. `session/route.svelte.ts` reads it | The browser hides or shows the tab |
| What is being written | `composer/outbox.svelte.ts`, in memory for each conversation. `composer/draft.ts` keeps a copy in `localStorage`, which also holds a message on its way until its receipt | Typing, a send, its receipt, a refusal, another tab clearing the copy |
| The files to send | `composer/outbox.svelte.ts`, in memory for each conversation | Attaching, removing, a send, a refusal |
| A message on its way, or its refusal | `composer/outbox.svelte.ts`, one for each conversation | A send, its stages, the snapshot that has its turn, and a sign-out, which cancels it |
| The agent's commands | `composer/commands.svelte.ts`, for each conversation | The first `/` written there asks the provider, once |
| The command chosen in the menu, and whether Escape hid it | `composer/Composer.svelte`, for the conversation it was set in | The arrows, Escape, writing |
| A control a person pressed, or why it was not taken | `activity/controls.svelte.ts`, one for each turn | A press and its answer |
| The time | `time/clock.svelte.ts` | Each second, and `clock.today` each day |

Everything a component draws is a `$derived` of these, or a call to one of the display functions: `ledger`, `outcome`, `size`, `span` and `when`.

## The rules

The page's rules are lints, in `lint/ast-grep/rules/web-*.yml` and `svelte-*.yml` and in the `overrides` of `.oxlintrc.json`. `mise run check:lint` proves them twice: `lint/ast-grep/tests/` gives each rule code it must accept and code it must refuse, and `scripts/test-web-rules.sh` scans a small page to show that each rule reaches the files it names, which a test of one snippet cannot.

Which module may use which browser global, such as `localStorage`, `fetch` or `JSON`, is in the `overrides` of `.oxlintrc.json`.

## What reaches outside the page's state

A listener, Svelte's `on`, a subscription, an observer, a timer, a media query and an attachment are each refused by a rule in `lint/ast-grep/rules/web-outside.yml`, except in the modules that rule's `ignores` name. Each rule lets through only the modules that use its form today. `web-markup-outside` refuses the same calls in a component's markup, and `svelte-effect` refuses an `$effect` outside the modules its `ignores` name. `scripts/test-web-rules.sh` fails when a module there no longer uses the form it is let through for, and when the table below and those `ignores` name different modules. A component reaches the window, the document or the body only through these modules, because `svelte-page-element` refuses `<svelte:window>`, `<svelte:document>` and `<svelte:body>`.

| Module | What it reaches, and why |
|---|---|
| `api/retry.ts` | `retryPause` waits with a timeout before the next attempt, and listens for the abort of the signal it was given, which ends the wait. |
| `api/send.ts` | `deliver` sends a message with the page's one `XMLHttpRequest`, the only request that reports its upload. It listens for the request's events, turns them into a stream of stages, and aborts the request when the signal it was given does. |
| `api/watch.ts` | `watch` gives a stream up when it has brought no byte for 30 s, which is three of the keep-alive comments the contract promises every 10 s, and opens the next one as after any other ending. Every chunk counts, a comment too, so the bytes are counted before `streamMessages` parses them. The timeout stops when the stream's signal aborts. |
| `composer/draft.ts` | `whenClearedElsewhere` listens for `storage`, so that what another tab cleared is stored again. `composer/outbox.svelte.ts` calls it once. |
| `composer/drop.svelte.ts` | `takesDrops` is an attachment on the composer. It listens on the composer's document, so it takes a file dropped anywhere on the page, and stops listening when the composer goes. |
| `session/focus.svelte.ts` | `focusedWhen` is an attachment that focuses the password field whenever the door waits for a password, and the first row "Show earlier conversations" added. The composer uses it for the message field, which each conversation gets anew, when `onDesktop`, a media query, says the window is as wide as a desktop's. `inViewWhen` is an attachment that scrolls the open conversation's row into view when it becomes the current one. `messageField` is an attachment on the composer's text field. It holds the field so that `focusMessage` can give it the keyboard after a control is pressed, on a window wide enough to show the rail beside the room. |
| `session/route.svelte.ts` | `followView` is the page's one `$effect`. It keeps one stream open for the view, which is the address and the two windows, and aborts it when one of them changes or the tab is hidden. A tab shown again opens it again. Whether the tab is shown is read through `createSubscriber`, which listens for `visibilitychange` with Svelte's `on` only while the effect reads it. The module also listens for `hashchange`, which changes the address it reads, and for `storage`, which loads the page again when another tab signs in. |
| `time/clock.svelte.ts` | `clock` subscribes to a one-second interval with `createSubscriber`, so the interval runs only while something on the page reads the time. |
| `transcript/follow-end.svelte.ts` | `followEnd` is an attachment on the element that scrolls. A `ResizeObserver` tells it the content changed, and a `scroll` listener tells it whether the person is still at the end and where the oldest turn is drawn. |

## What the tools do not tell you

- The message field grows by the stylesheet's `field-sizing: content`. The browser checks prove it in Chromium only. A browser without it keeps a field of one line that scrolls.
- A snapshot replaces every turn's object, so everything derived from a turn runs again for all of them. That is cheap except for `when()`, which costs about 40 µs a call. `time/Moment.svelte` therefore reads its prop through a `$derived` of the number alone, and reads `clock.today` in place of `clock.now`. Draw every time on the page with `Moment`.
- The effect in `followView` tracks only what it reads before the first `await`. `follow` and `watch` must read no state there.
- `hear` in `session/session.svelte.ts` is the one place a snapshot is taken in. It also calls `see` and `settle`, because both remember something the next snapshot no longer says. `readAddress` calls `see` too, so that a conversation opened and left before its snapshot arrived still counts as seen.
- "New conversation" links to `#new`, an address that names no conversation. Svelte's accessibility check refuses `href="#"`. `readAddress` then gives the address a new conversation to name.
- A pressed control reads as pressed and holds its turn's other controls until the turn ends, or starts or stops waiting its turn. A runtime that still offers the control fifteen seconds later took it and changed nothing, so it is offered again. The press is kept by turn, outside the component, because the transcript is drawn again for each conversation opened.
- A browser that cannot store the token keeps it in `session.token` alone: the person is signed in until the page is loaded again. `storeToken` says whether it stored, and `signIn` loads the page again only when it did, so a full store cannot turn a sign-in into a loop.
- A file's item in `transcript/Files.svelte` has no key. The provider signs every link again each UTC day, so a key of the link would take every picture off the page and draw it again at midnight. Without one the picture stays and only its address changes.
- `followEnd` also holds a person's place when turns are added above it: it remembers where the oldest turn was drawn, and scrolls by as much as that turn was pushed. The browser's own scroll anchoring does not hold it: with the button left out by `overflow-anchor: none`, Chromium still let the turns be pushed down.
- The transcript is inside `{#key route.address}`, so each conversation gets its own scrolling element and starts at its end.
