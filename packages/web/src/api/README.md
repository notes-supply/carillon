# The browser's contract with the web provider

This folder is what the page and the provider agree on. The provider's Rust types serialize to exactly these shapes, and the page refuses a message with a field they lack. The check is `stream.sample.json`. The provider's test `a_snapshot_of_each_kind_of_turn_is_the_page_s_sample_message`, in `crates/carillon-web/src/stream/tests.rs`, asserts that its snapshot of each kind of turn serializes to that file, and `stream.test.ts` asserts that `readStreamMessage` reads the file as that snapshot. Only a file link's expiry and signature, which change with the day and the installation, are left out of the comparison.

- `stream.ts`: the stream's messages as TypeBox schemas, the types derived from them, `readStreamMessage` and `streamMessages`.
- `requests.ts`: the view, each request's path and body, each answer's schema, and what a status means.
- `stream.sample.json`: one snapshot message with a turn of each kind, as the provider writes it.
- `stream.test.ts`, `requests.test.ts`: what both promise, run by `vitest`.

The files have no comments, because the page's code has none. This file holds what a comment would have said.

## The stream

`GET /api/stream?conversations=<n>[&agent_id=<agent>&audience_id=<audience>[&turns=<n>]]` with `Authorization: Bearer <token>` answers `text/event-stream`. `streamPath(view)` writes the query, which is the one core reads. `conversations` and `turns` are 1 to 500. `turns` needs a conversation, and a conversation needs both of its IDs. Anything else is a 400 with a plain-text body. A token that is not the current one is a 401.

Each event is one `data` line of JSON, one of three messages:

| `kind`        | Meaning                                                   | What follows     |
| ------------- | --------------------------------------------------------- | ---------------- |
| `snapshot`    | Everything the view shows. The page replaces what it had. | More messages.   |
| `unavailable` | The provider is here and core is not.                     | The stream ends. |
| `signed_out`  | The password changed. The token is void.                  | The stream ends. |

The first event is a `snapshot`, `unavailable` or `signed_out`, and a stream whose UTC day ends before its first snapshot is ready ends with none. The provider sends a snapshot only when it differs from the last one it sent on that stream. A comment line every 10 s keeps the connection open. The provider ends every stream at UTC midnight, so the next one carries file links that work for another day.

`streamMessages(response.body)` yields each message as `Received`, which adds `{ kind: "unreadable" }` for an event the schema refuses. To stop reading, abort the request.

### `ChatSnapshot`

| Field           | Meaning                                                                                       |
| --------------- | --------------------------------------------------------------------------------------------- |
| `agents`        | The configured agents, `{ id, name }`, in registry order. A new conversation takes the first. |
| `limits`        | Core's bounds on one message's files: `files`, `file_bytes`, `message_bytes`.                 |
| `custom_site`   | The page served is the owner's copy.                                                          |
| `conversations` | The latest active first, at most as many as the view asked for.                               |
| `more`          | The interface has more conversations than are listed.                                         |
| `open`          | `null` when the view opened none. Otherwise `unrecorded` or `recorded`.                       |

A conversation is `{ key, title, started_at, active_at, activity, at_work }`. `key` is `{ agent_id, audience_id }`. `title` is empty when the first message had only files. `activity` rises with every message, every turn the runtime opens and every turn's end, and no two conversations share one. `at_work` counts the turns queued or running. Times are Unix seconds.

`open` is one of:

- `{ kind: "unrecorded", key }`: core has no conversation under this key. Its first admitted message records it.
- `{ kind: "recorded", conversation, turns, earlier }`: the newest turns, oldest first, at most as many as the view asked for. `earlier` says older turns exist.

A turn is `{ turn_id, source, accepted_at, progress }`.

`source` is who began it:

- `{ kind: "message", event_id, text, command, files }`: a person's message. `text` is shown as text, never as markup. `command` says the provider took it for one of the runtime's commands. A control names the turn by `event_id`.
- `{ kind: "opened", label? }`: the runtime opened the turn, for a schedule or a subagent's report.

`progress` is how far it has come:

| `state`   | Also present                       | Meaning                                                                                                                                                                                                                               |
| --------- | ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `queued`  |                                    | Core has not handed it to its runtime.                                                                                                                                                                                                |
| `running` | `activity`, `controls`             | Its runtime has it. `activity` is `null` when the runtime gave no answer. A turn its runtime holds behind another has `activity.phase` `queued`. `controls` is what a person may do with it now: any of `steer`, `interrupt`, `stop`. |
| `ended`   | `finished_at`, `ending`, `report?` | It will not change again.                                                                                                                                                                                                             |

`ending` is one of `{ kind: "answered", html, files }`, `{ kind: "interrupted", html }`, `{ kind: "steered" }` and `{ kind: "stopped" }`. `html` is Markdown the provider rendered: insert it as it is, in one place.

`Activity` is the runtime's own words for a turn, the same shape for a running turn's `activity` and an ended turn's `report`: `phase`, `headline?`, `running`, `recent`, `tool_calls`, `work?`, `notes?`, `agent?`, `partial?`. Show every string in it as text.

A `FileLink` is `{ name, size, type, url }`. `type` is `image/png`, `image/jpeg`, `image/gif` or `image/webp` for a file the link answers as an image, and `application/octet-stream` for one that downloads. `url` is opaque, needs no token, and works until the end of the next UTC day.

## The requests

| Request                         | Body                                                                          | Answers                                            |
| ------------------------------- | ----------------------------------------------------------------------------- | -------------------------------------------------- |
| `POST /api/sign-in`             | `signInBody(password)`                                                        | 200 `SignedInSchema`, 401 with the reason, 429     |
| `GET streamPath(view)`          |                                                                               | 200 event stream, 400, 401                         |
| `PUT messagePath(key, eventId)` | `messageForm(text, files)`: one `text` part, then a `file` part for each file | 200, 400, 413, 422, 503, 507                       |
| `POST controlPath(message)`     | `controlBody(action)`                                                         | 204, 404, 409, 503                                 |
| `GET commandsPath(key)`         |                                                                               | 200 `CommandCatalogSchema`, 422, 503               |
| `GET <FileLink.url>`            |                                                                               | The file's bytes, 403 for a link that is void, 404 |

Every request but the sign-in and a file link carries `Authorization: bearer(token)`. A JSON body goes with `Content-Type: application/json`, and the provider answers 415 to one without it.

The provider checks one sign-in at a time. A sign-in sent while another is checked is answered 429 at once and is not checked. The same password, sent again a moment later, is checked then.

- `mintEventId()` names a message before its first attempt. Every retry sends the same ID, and core takes a repeat as the first message, so a retry never makes a second turn. A 2xx is the message's receipt, and the page does not read its body.
- `mintAudienceId()` names a new conversation.
- A send reports upload progress only through `XMLHttpRequest.upload`. `fetch` cannot.
- `readAnswer(schema, body)` reads a 2xx body, and is `undefined` for a body the schema refuses.
- `reading(status)` says what a status means: `ok` for 2xx, `away` for 503 and for a request that got no answer (status 0), `signed_out` for 401, `refused` for the rest.
- `refusalWords(status, body)` is the one rule for showing an answer that is not 2xx: the provider's `error` when the body has one, and otherwise a sentence for the status.
- `retryDelayMs(failures)` is how long to wait before the next attempt.
