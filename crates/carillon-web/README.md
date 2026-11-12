# Web provider

`carillon-web` is the interface provider behind Carillon's web chat, a page
in your own browser on the machine that runs Carillon. It is built on the
[shared interface boundary](../carillon-interface/README.md). It keeps no
conversation: core has every turn, file and answer, and the provider reads
them back. Its only durable file is the browser key,
`state/components/<id>/browser.key`.

A registry entry with `program = "web"` runs it. No release registers one
yet, so write the entry yourself:

```toml
[[providers]]
id = "web"
role = "interface"
program = "web"
version = "0.1.0-beta.9"
url = "http://127.0.0.1:45404"
token_hash = "<SHA-256 of credentials/web.token>"
enabled = true
max_upload_mib = 1024
delivery = "retained"
```

`delivery = "retained"` matters: a web answer is delivered when core records
it, and the provider claims no delivery.

## Commands

```sh
carillon web serve --config carillon.toml --provider web --token-file credentials/web.token
carillon web open            # sign this machine's browser in and open the chat
carillon web open --print    # print the link instead
carillon web sign-out        # sign every browser out and void every file link
```

`open` and `sign-out` work on the browser key file and send the provider
no request. `open` only checks that something listens at the provider's
address, and says so when nothing does. `sign-out` works whether or not the
provider runs. Both take `--provider ID` when the registry has more than one
web entry.

## Who may use it

Any page your browser opens can send requests to loopback, so the provider
trusts nothing a browser attaches on its own.

- The browser key is the root secret. Its file has mode 0600, so only the
  installing user reads it. The provider reads it again for every request
  that presents a token, a sign-in code or a file link, and keeps no key
  between requests. A replaced file takes effect on the next request.
- `carillon web open` reads the key and makes a sign-in code: a time two
  minutes on, a name of its own and the key's signature of both. It opens
  `<address>/#sign-in=<code>`. The provider takes a code that the current
  key signed, that has not expired, that was made since the provider
  started and that no request traded before. It remembers a traded code
  only while it runs and until the code expires, so it takes no code from
  before its start. The page trades the code for a token and keeps the
  token in `localStorage`.
- Every `/api` request carries `Authorization: Bearer <token>`. No cookie
  carries authority.
- `Host` must be the address the provider listens on, as a browser writes
  it, or the answer is 421. `localhost` is another name and is refused.
- An `Origin` other than the provider's own is 403, a preflight's included.
  No answer has a CORS header.
- The page runs under a content security policy that allows only its own
  scripts, styles and images.
- The token, a sign-in code and every file link are HMACs of the browser
  key. `carillon web sign-out` replaces the key, which voids them all.

The page's address belongs to whichever local program holds the provider's
port. While the provider is stopped, another program of this machine can
take the port and serve a page at the chat's own origin, which can read what
the real page stored there. Carillon trusts the machine's local programs
here, as its other providers do. `carillon web sign-out` voids what such a
page could have taken. `carillon web open` refuses to make a link when
nothing listens, but it cannot tell another listener from the provider.

## The page's files

The page is plain files, read from disk on each request, so an edit shows
on the next reload. `serve` finds the release's copy in `lib/web` beside the
directory that holds the program, which is `app/lib/web` in an installation.
In a checkout, name the directory: `--site crates/carillon-web/site`.

A complete copy in `state/components/<id>/site/` is served instead of the
release's. A copy is complete when it has an `index.html`. No file comes
from the release while the copy is served. The directory is flat, and the
provider serves `.html`, `.js`, `.css`, `.svg` and `.png` files from it.

Only a regular file in the folder is served. A symbolic link inside the
folder is answered with 404, because the page's files need no token and such
a link could name any file you can read. The folder itself, and the path to
it, may be reached through links, and whatever folder they lead to is the
page. So a `site` that is a link to a folder with an `index.html` counts as
the custom page, and everything servable in that folder is public to this
machine.

The files in [`site/`](site) only prove sign-in, reading and sending.

## Browser API

Every route but `POST /api/sign-in` needs the token.

A conversation's ID is a UUID the page mints, lowercase with hyphens. The
agent is chosen with the first message and never changes. A message's event
ID is `m-<uuid>`, minted once per message and sent again with every retry.
Times are Unix seconds.

### Refusals

A page needs one rule for an answer that is not 2xx. Read the body as text.
When it is JSON with a string `error`, show that string. Otherwise show a
sentence of your own for the status.

- The provider and core write `{"error": "<words a person can read>"}`.
  Core's words for a refused request pass through as core wrote them, and
  some begin in lowercase.
- A request that cannot be parsed has a plain-text body: 400 for an ID that
  is not a lowercase UUID, a missing query field or broken JSON, 415 for
  JSON under another content type, and 422 for JSON of the wrong shape.
- 404 for a path that is no route, and 405 for a method a route does not
  take, have no body.

These statuses mean the same on every route:

| Status | Meaning | What the page does |
| --- | --- | --- |
| 401 | The token is not the current key's: never signed in, or signed out. | Drop the token and ask the person to run `carillon web open`. |
| 403, 421 | The request did not come from the page's own origin, or named another host. | Nothing. A page loaded from the provider never sees these. |
| 503 | Carillon cannot answer now: core is away, as it is for a moment at every restart, or it failed, or the provider is stopping. | Keep what is on screen and try again. The body is always the same sentence. |

A `fetch` that fails with no status means the provider is not running.
Treat it like a 503: keep the draft and any pending message, and try again.

The provider logs what core said when it answers 503 for core. The page is
never sent it.

### Sign in

```http
POST /api/sign-in
{"code": "<the fragment's code>"}

200 {"token": "9f2c…"}
```

Take the code from the fragment and remove the fragment from the address
before the request. A code that was traded already, has expired, was made
before the provider started or was not signed by the current key is 401.

### Read the session

```http
GET /api/session

200 {
  "agents": [{"id": "assistant", "name": "Carillon"}],
  "limits": {"files": 10, "file_bytes": 1073741824, "message_bytes": 2147483648},
  "custom_site": false
}
```

`agents` are the choices for a new conversation. `limits` are core's bounds
on one message's files, for the page to check before it uploads. They count
file bytes only, not the text. `custom_site` says that the page served is
the copy in the provider's state.

### List conversations

```http
GET /api/conversations
GET /api/conversations?before=41

200 {
  "conversations": [{
    "id": "0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b",
    "agent_id": "assistant",
    "title": "Plan the release",
    "started_at": 1794499000,
    "active_at": 1794499146,
    "activity": 57,
    "at_work": 1
  }],
  "next": 41
}
```

The latest active conversation is first, and a page lists at most 50.
`next` is null on the last page. Otherwise pass it as `before`. A title is
the first line of the first message that is not blank, trimmed, at most 80
characters. It is empty when that message had only files.

### Poll

Poll the first page of this list, about once a second while the page is
visible. Stop while it is hidden, and poll at once when it shows again.
After a 503 or a failed fetch, wait longer each time, up to about ten
seconds.

Compare each conversation's `activity` and `at_work` with the values you
saw last. `activity` rises with every message, every turn the runtime opens
and every turn's end. `at_work` counts the turns queued or running.

- While both are unchanged, the conversation has no new turn and no new
  answer. Do not read its turns again.
- When either changed in the open conversation, read its newest page of
  turns again and replace the turns it holds by `id`. A turn that has ended
  does not change, so older pages you loaded stay as they are.
- A conversation that changes moves to the top of the list. An open
  conversation that is not on the first page has therefore not changed. Do
  not take its absence for a deletion.

A turn going from queued to running, and its progress, change neither
number. For those, [read the live turns](#read-live-turns) on screen.

### Read a conversation's turns

```http
GET /api/conversations/0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b/turns
GET /api/conversations/0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b/turns?before=<turn id>

200 {
  "turns": [{
    "id": "019a…",
    "from": {
      "kind": "person",
      "event_id": "m-0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa",
      "text": "Chart last week's numbers.",
      "command": false,
      "files": [{"name": "numbers.csv", "size": 812, "type": "application/octet-stream",
                 "url": "/files/…/0?name=numbers.csv&exp=1794614400&sig=…"}]
    },
    "accepted_at": 1794499100,
    "finished_at": 1794499146,
    "state": "answered",
    "html": "<p>Here is the <strong>chart</strong>.</p>\n",
    "files": [{"name": "chart.png", "size": 20417, "type": "image/png",
               "url": "/files/…/0?name=chart.png&exp=1794614400&sig=…"}],
    "report": null
  }],
  "before": null
}
```

A page holds the newest 50 turns, oldest first. `before` is null when no
older turn exists. Otherwise pass it as `before`. A `before` that names no
turn of the conversation reads as 200 with no turns. A conversation core
does not have is 404.

`from` is who began the turn:

- `{"kind": "person", …}` is a message. `text` is what the person wrote.
  Show it as text, never as markup. A browser's form data sends line ends
  as CRLF, and they are kept.
- `{"kind": "runtime", "label": "Scheduled run: morning summary"}` is a turn
  the runtime opened itself, for a schedule or a subagent's report. `label`
  can be null. Such a turn has no event ID, so no control can name it.

`state` is how the turn stands:

| `state` | Meaning | Also present |
| --- | --- | --- |
| `queued` | It is not finished. Core has not handed it to its runtime yet. | |
| `running` | It is not finished. Its runtime has it. | |
| `answered` | It has its answer. | `html`, `files` |
| `interrupted` | It did not complete. `html` is core's or the runtime's explanation. | `html` |
| `steered` | The message went into a turn already running. | |
| `stopped` | Someone stopped it. | |

The turns tell you that a turn is unfinished and not whether it waits. A
message that its runtime holds behind a running answer reads `running`
here. Whether an unfinished turn is queued or running comes from the
[live read](#read-live-turns) alone.

A steered message has no answer of its own. The answer is on the newest
message that took part, which may not be the one above, so label it
neutrally.

`html` is the answer's Markdown, rendered by the provider. Insert it as it
is. Raw HTML in an answer is shown as text. A link stays a link only to
`http`, `https` or `mailto`, and opens in a new tab. An image becomes a link
to it.

`report` is an ended turn's final [activity snapshot](#the-activity-snapshot),
or null. Its `phase` is the last phase the turn was in while it ran. Draw an
ended turn from `state`, never from `report.phase`.

### Files

A file has a `name`, a `size` in bytes, a `type` and a `url`.

- Treat the `url` as opaque. It needs no token, so it works in `<img>` and
  as a download link.
- A link is the same for a whole UTC day, so the browser keeps the images it
  fetched. It works until the end of the next UTC day. After that it is 403.
  Read the turns again for a new link.
- A sign-out voids every link at once: the next request for one is 403. The
  bytes a browser already cached or saved remain with that browser.
- A file core no longer has is 404.
- A `Range` request is answered with the whole file.

`type` is the type the link answers with. A PNG, JPEG, GIF or WebP file,
by its bytes, has its image type and is shown in place. Every other file,
SVG and HTML included, is `application/octet-stream` and downloads. Show a
`name` as text.

### Send a message

```http
PUT /api/conversations/<conversation>/messages/<event id>?agent=assistant
Content-Type: multipart/form-data; boundary=…

text=Chart last week's numbers.
file=<numbers.csv>

200 {"turn_id": "019a…", "duplicate": false}
```

The body has one `text` part and any number of `file` parts. A message needs
text or a file. Leave out a `file` part that has no file: an HTML form sends
an empty one when nothing is chosen.

- `agent` names the agent of a conversation's first message. Later messages
  need none. Naming another agent than the conversation's is 409.
- `command=true` says that the text invokes one of the
  [runtime's commands](#list-the-runtimes-commands).

The 200 is core's receipt: core has the message, files included.

| Status | Meaning |
| --- | --- |
| 400 | The body is not a readable multipart form, or has neither text nor a file. |
| 404 | The conversation is new and names no agent. |
| 409 | The conversation has another agent. |
| 413 | Too many files, or a file or the message is over core's limits. |
| 422 | Carillon has no agent of that name. |
| 503 | Carillon cannot take it now. Send it again, unchanged. |
| 507 | Core's file store is full. Sending again does not help. |

#### A message that is not yet received

Until the receipt, the page holds the message as pending.

- Mint the event ID before the first attempt and keep it with the message.
- Do not clear the draft before the receipt.
- After a 503, or a fetch that failed with no status, the outcome is not
  known: core may have the message. Send the same request again, with the
  same conversation, event ID, agent, command and body. Core answers a
  repeat with the first receipt and `"duplicate": true`, whatever the
  repeat contains, and submits nothing twice.
- The page keeps no store of pending messages, so a reload loses the event
  ID. After a reload, read the turns before you send a draft again.

A request can stay open for minutes, and it does not say why. The causes:

- The upload is still going. `fetch` reports no upload progress.
  `XMLHttpRequest.upload` does.
- Core is copying the message's files, which takes up to five minutes for
  large ones.
- Another message of the same conversation is not received yet. The provider
  takes a conversation's messages one at a time, so they arrive in order.
  The page can tell this cause only from its own earlier pending message.
- Core is away. The provider waits for it, as it does at every restart.

If the browser leaves after its upload finished, the provider still submits
the message. It sets no deadline of its own: a submission waits for core
until core answers or the provider stops.

### Read live turns

```http
GET /api/turns?ids=<turn id>,<turn id>

200 {
  "turns": [{
    "id": "019a…",
    "state": "queued",
    "activity": {"phase": "queued", "running": [], "recent": [], "tool_calls": 0,
                 "controls": ["interrupt", "stop"]}
  }]
}
```

Ask here for the unfinished turns on screen, at most 32 at once. More is a
400. A turn core does not have for this provider is left out.

`state` is `queued`, `running` or `ended`. A turn its runtime holds behind
another is `queued`. When a turn is `ended`, read its conversation's turns
for how it ended.

`activity` is the runtime's latest snapshot, or null when the runtime gave
none.

### The activity snapshot

The provider passes a snapshot on as the runtime gave it. Show every string
in it as text. A field marked optional is left out when it has no value.

```json
{
  "phase": "tools",
  "headline": "Reading the release notes.",
  "running": [{"tool": "read", "detail": "docs/RELEASE.md", "started_at": 1794499120}],
  "recent": [{"tool": "bash", "detail": "git log -5", "ok": true, "duration_ms": 41}],
  "tool_calls": 2,
  "notes": ["The changelog is out of date."],
  "agent": {"model": "gpt-6.1", "effort": "high", "context": {"used": 18250, "window": 400000}},
  "controls": ["stop"]
}
```

| Field | Meaning |
| --- | --- |
| `phase` | `starting`, `queued`, `model`, `tools`, `waiting`, `retrying` or `compacting`. |
| `headline` | Optional. The model's latest public commentary. |
| `running` | The tool calls under way. Each has `tool`, `started_at`, an optional `detail` and an optional `estimated: true` when the start is the runtime's guess. |
| `recent` | The calls finished last, oldest first. Each has `tool`, `ok`, an optional `detail` and an optional `duration_ms`. |
| `tool_calls` | Every call started in the turn, those no longer listed included. |
| `work` | Optional. Delegated and transferred work: `items`, `partial`, which is true when the runtime could not observe all of it, and an optional `omitted` count. |
| `notes` | Optional. At most 8 excerpts of the reasoning the provider published, oldest first. |
| `agent` | Optional. `model`, `effort` and `context` with `used` and `window`, each optional. |
| `partial` | Optional. `true` when part of the snapshot was left out for size. |
| `controls` | Optional. What a person may do with the turn now: any of `steer`, `interrupt`, `stop`. |

An item of `work` has an `id`, a `kind` (`task`, `delegation`, `handoff` or
`fork`), a `label` and a `state` (`queued`, `running`, `waiting`,
`succeeded`, `failed`, `cancelled` or `unknown`). `parent`, `actor`,
`started_at`, `estimated` and `duration_ms` are optional.

### Send a control

```http
POST /api/messages/m-0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa/control
{"action": "stop"}

204
```

A control names a turn by its message's event ID. Show a control only while
the live read's `activity.controls` lists it. The runtime decides what it
lists. On Pi, a message that waits behind a running answer lists those of
`steer`, `interrupt` and `stop` that apply to it, and a running answer
lists `stop`. A turn the runtime opened lists none.

| `action` | On | What it does | How the turns end |
| --- | --- | --- | --- |
| `steer` | A waiting message | Hands it to the running turn. | The message's turn is `steered`. The answer comes on the newest message that took part. |
| `interrupt` | A waiting message | Ends the running turn and runs the message next. | The earlier turn is `stopped`. The message's turn runs and is `answered`. |
| `stop` | A waiting message | Does not run it. | The message's turn is `stopped`. The running turn goes on. |
| `stop` | A running answer | Ends that answer, and none that follows it. | Its turn is `stopped`, and so is the turn of every message that was steered into it. |

Neither the provider nor core checks that a control is listed. The runtime
refuses one that does not apply, or takes it and changes nothing.

A refusal comes back with core's or the runtime's status and words: 404
when core has no such message, 409 when its turn is no longer running. After
a control or its refusal, read the live turns and the conversation's turns
again.

### List the runtime's commands

```http
GET /api/conversations/<conversation>/commands
GET /api/conversations/<conversation>/commands?agent=assistant

200 {"commands": [{"name": "skill:carillon-status", "description": "Report Carillon's status."}]}
```

The list is what the conversation's runtime offers its agent, in the
runtime's order. `description` and `hint`, the arguments a command expects,
are optional. Pass `agent` for a conversation that has no message yet. To
invoke a command, send a message whose text starts with the command's name,
with `command=true`.

The refusals are those of a send's `agent`: 404 for a new conversation with
no agent named, 422 for an agent Carillon does not have, and 409 for another
agent than the conversation's. A runtime that is disabled, unreachable or
late is a 503.

## Operator API

`GET /v1/channels` needs the operator's credential as a bearer and answers
`[]`. The interface's own credential and the browser's token are refused
with 403. With the operator's credential, every other path under `/v1` is
404.

A web chat is its own channel in core, but it has no settings page: it runs
its agent's model. Change that with `carillon settings --agent`.

`GET /health` answers what core checks at start, with no credential.

## Where things live

- [`serve.rs`](src/serve.rs): the process, the router and the gate every
  request passes.
- [`browser.rs`](src/browser.rs): the browser key, sign-in codes, the token
  and signatures.
- [`identity.rs`](src/identity.rs): how a conversation and a message map
  onto core's identities.
- [`api.rs`](src/api.rs): the browser API's reads, its view types and what
  the page is told when core fails. [`send.rs`](src/send.rs): a message and
  its files.
- [`files.rs`](src/files.rs): signed file links. [`markdown.rs`](src/markdown.rs):
  answers as HTML.
- [`site.rs`](src/site.rs): the page's files.
