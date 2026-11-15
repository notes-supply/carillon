# Carillon's web chat, in Svelte 5

One slice of the page, built for the framework bakeoff: the door, the rail, the transcript and the composer, against the stream contract in `src/api/stream.ts` and `src/api/requests.ts`.

`vite build` writes a flat folder to `dist/`, which the provider serves as it is. The code has no comments, so this file holds what a comment would have said.

## Where each fact lives

| Fact | Owner | Changes when |
|---|---|---|
| The token | `localStorage`, read through `session/token.ts`. `session/session.svelte.ts` holds it as state. | A sign-in, or `signed_out` on the stream |
| What the server knows | `session.snapshot`, one `$state.raw` value | A snapshot arrives and replaces it |
| The notice | `session.notice` | `unavailable` or `unreadable` sets it. The next snapshot empties it |
| What was seen of each conversation | `session.seen`, folded by `rail/unread.ts` | A snapshot arrives, or the address changes |
| The open conversation | The address. `session/route.svelte.ts` reads and writes it | `hashchange` |
| The window of turns | `session/route.svelte.ts`, for the conversation it was widened in | "Show earlier turns" |
| What is being written | `localStorage`, through `composer/draft.ts`. `composer/outbox.svelte.ts` holds it for the page | Typing, a send, a refusal |
| A message on its way, or its refusal | `composer/outbox.svelte.ts`, one for each conversation | A send, its stages, and the snapshot that has its turn |
| The time | `time/clock.svelte.ts` | Each second, and `clock.today` each day |

Everything a component draws is a `$derived` of these, or a call to one of the given display functions.

## What reaches outside the page's state

- `followView` in `session/route.svelte.ts` is the page's one `$effect`. It keeps one stream open for the view, which is the address and the window of turns, and aborts it when either changes.
- `followEnd` in `transcript/follow-end.svelte.ts` is an attachment on the element that scrolls. A `ResizeObserver` tells it the content changed, and a `scroll` listener tells it whether the person is still at the end.
- `focusedWhen` in `session/focus.svelte.ts` is an attachment that focuses the password field whenever the door waits for a password.
- `clock` subscribes to a one-second interval with `createSubscriber`, so the interval runs only while something on the page reads the time.

## What the tools do not tell you

- A snapshot replaces every turn's object, so everything derived from a turn runs again for all of them. That is cheap except for `when()`, which costs about 40 µs a call. `time/Moment.svelte` therefore reads its prop through a `$derived` of the number alone, and reads `clock.today` in place of `clock.now`. Draw every time on the page with `Moment`.
- The effect in `followView` tracks only what it reads before the first `await`. `follow` and `watch` must read no state there.
- `hear` in `session/session.svelte.ts` is the one place a snapshot is taken in. It also calls `see` and `settle`, because both remember something the next snapshot no longer says. `readAddress` calls `see` too, so that a conversation opened and left before its snapshot arrived still counts as seen.
- "New conversation" links to `#new`, an address that names no conversation. Svelte's accessibility check refuses `href="#"`. `readAddress` then gives the address a new conversation to name.
- The transcript is inside `{#key route.address}`, so each conversation gets its own scrolling element and starts at its end.
