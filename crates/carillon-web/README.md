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

`open` and `sign-out` ask the running provider, with the operator's
credential. Both take `--provider ID` when the registry has more than one
web entry.

## Who may use it

Any page your browser opens can send requests to loopback, so the provider
trusts nothing a browser attaches on its own.

- `carillon web open` gets a single-use code from the provider and opens
  `<url>/#sign-in=<code>`. The code works once, within two minutes. The page
  trades it for a token and keeps the token in `localStorage`.
- Every `/api` request carries `Authorization: Bearer <token>`. No cookie
  carries authority.
- `Host` must be the registry URL's own `host:port`, or the answer is 421.
  `localhost` is another name and is refused.
- An `Origin` other than the provider's own is 403, a preflight's included.
  No answer has a CORS header.
- The page runs under a content security policy that allows only its own
  scripts, styles and images.
- The token and every file link are HMACs of the browser key.
  `carillon web sign-out` replaces the key, which voids them all.

## The page's files

The page is plain files, read from disk on each request, so an edit shows
on the next reload. `serve` finds the release's copy in `lib/web` beside the
directory that holds the program, which is `app/lib/web` in an installation.
In a checkout, name the directory: `--site crates/carillon-web/site`.

A complete copy in `state/components/<id>/site/` is served instead of the
release's. A copy is complete when it has an `index.html`. No file comes
from the release while the copy is served. The directory is flat, and the
provider serves `.html`, `.js`, `.css`, `.svg` and `.png` files from it.

The files in [`site/`](site) only prove sign-in, reading and sending.

## Browser API

Every route but `POST /api/sign-in` needs the token. A refusal is
`{"error": "<words a person can read>"}` with its status. A request the
provider cannot parse, such as an ID that is not a lowercase UUID, is a 400
with a plain-text body.

A conversation's ID is a UUID the page mints, lowercase with hyphens. The
agent is chosen with the first message and never changes. A message's event
ID is `m-<uuid>`, minted once per message and sent again with every retry.
Times are Unix seconds.

### Sign in

```http
POST /api/sign-in
{"code": "<the fragment's code>"}

200 {"token": "9f2c…"}
```

A code that was used, has expired or was never issued is 401.

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
on one message's files, for the page to check before it uploads.
`custom_site` says that the page served is the copy in the provider's state.

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

The latest active conversation is first. `next` is null on the last page.
Otherwise pass it as `before`. A title is the first line of the first
message, and is empty when that message had only files.

Poll this list. `activity` rises with every message, every turn the runtime
opens and every turn's end, and `at_work` counts the turns queued or
running. While both are unchanged, the conversation has no new turn and no
new answer, so do not read its turns again.

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

The page holds the newest 50 turns, oldest first. `before` is null when no
older turn exists. Otherwise pass it as `before`. A conversation core does
not have is 404.

`from` is who began the turn:

- `{"kind": "person", …}` is a message. `text` is what the person wrote.
  Show it as text, never as markup.
- `{"kind": "runtime", "label": "Scheduled run: morning summary"}` is a turn
  the runtime opened itself, for a schedule or a subagent's report. `label`
  can be null.

`state` is how the turn stands:

| `state` | Meaning | Also present |
| --- | --- | --- |
| `queued` | It waits for its runtime. | |
| `running` | The runtime has it. | |
| `answered` | It has its answer. | `html`, `files` |
| `interrupted` | It did not complete. `html` is the explanation. | `html` |
| `steered` | The message went into a turn already running. | |
| `stopped` | Someone stopped it. | |

A steered message has no answer of its own. The answer is on the newest
message that took part, which may not be the one above, so label it
neutrally.

`html` is the answer's Markdown, rendered by the provider. Insert it as it
is. Raw HTML in an answer is shown as text. A link stays a link only to
`http`, `https` or `mailto`, and opens in a new tab. An image becomes a link
to it.

`report` is an ended turn's final activity snapshot, in the same shape as
`activity` in a [live read](#read-live-turns), or null.

### Files

A file has a `name`, a `size` in bytes, a `type` and a `url`. The `url`
needs no token, so it works in `<img>` and as a download link. It opens
that one file until the end of the next day (UTC), and stops working at a
sign-out. Read the turns again for a new link. A link is the same all day,
so the browser keeps the images it fetched.

`type` is the type the link answers with. A PNG, JPEG, GIF or WebP file,
by its bytes, has its image type and is shown in place. Every other file,
SVG and HTML included, is `application/octet-stream` and downloads.

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
- A retry under the same event ID submits nothing new and answers the first
  receipt with `"duplicate": true`.

The answer comes once core has the message, files included. Core copies
the files first, which can take minutes for large ones.

| Status | Meaning |
| --- | --- |
| 400 | The body is not a readable multipart form, or has neither text nor a file. |
| 404 | The conversation is new and names no agent. |
| 409 | The conversation has another agent. |
| 413 | Too many files, or a file or the message is over core's limits. |
| 422 | Carillon has no agent of that name. |
| 503 | The provider is stopping. Send the message again. |
| 507 | Core's file store is full. |

While core still copies the files of an earlier message of the
conversation, a later message waits in the provider until core takes it.
Show it as waiting behind the earlier one.

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

A turn going from queued to running, and its progress, change nothing the
conversation list shows. Ask here for the unfinished turns on screen, at
most 32 at once. A turn core does not have for this provider is left out.

`state` is `queued`, `running` or `ended`. A turn its runtime holds behind
another is `queued`. When a turn is `ended`, read its conversation's turns.

`activity` is the runtime's latest snapshot, or null. The provider passes
it through as the runtime gives it:
[`TurnActivity`](../carillon-protocol/src/model.rs) names its fields.
`activity.controls` lists what a person may do with the turn now. Offer a
control only while it is listed. A turn the runtime opened lists none.

### Send a control

```http
POST /api/messages/m-0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa/control
{"action": "stop"}

204
```

`action` is `steer` (hand the message to the running turn), `interrupt` (end
the running turn and run this message next) or `stop` (run it not at all). A
control names the turn by its message's event ID. The runtime's refusal, for
example of a turn that no longer waits, comes back with its status and
words. The outcome shows in the conversation's turns.

### List the runtime's commands

```http
GET /api/conversations/<conversation>/commands
GET /api/conversations/<conversation>/commands?agent=assistant

200 {"commands": [{"name": "skill:carillon-status", "description": "Report Carillon's status.", "hint": null}]}
```

The list is what the conversation's runtime offers its agent, in the
runtime's order. Pass `agent` for a conversation that has no message yet.
To invoke a command, send a message whose text starts with the command's
name, with `command=true`.

## Operator API

These routes need the operator's credential as a bearer. The interface's own
credential and the browser's token are refused with 403.

| Route | Answer |
| --- | --- |
| `POST /v1/sign-in` | `{"code": "…"}`, the code `carillon web open` puts in its link. |
| `POST /v1/sign-out` | 204. The browser key is replaced. |
| `GET /v1/channels` | `[]`. |
| `GET` or `PUT /v1/channels/{id}/settings` | 404. |

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
- [`api.rs`](src/api.rs): the browser API's reads and its view types.
  [`send.rs`](src/send.rs): a message and its files.
- [`files.rs`](src/files.rs): signed file links. [`markdown.rs`](src/markdown.rs):
  answers as HTML.
- [`site.rs`](src/site.rs): the page's files. [`channels.rs`](src/channels.rs):
  the operator API.
