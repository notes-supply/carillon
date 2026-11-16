# Web provider

`carillon-web` is the interface provider behind Carillon's web chat, a page
in your own browser, signed in with a password you choose. It is built on
the [shared interface boundary](../carillon-interface/README.md). It keeps
no conversation: core has every turn, file and answer, and the provider
shows the page core's snapshots of them. Its only durable file is the
password's hash, `state/components/<id>/password.hash`.

A registry entry with `program = "web"` runs it. Setup registers one, when
you choose the web chat at install or run `carillon setup --interface web`
on an installation without it:

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
it, and the provider claims no delivery. The provider refuses to serve an
entry without that line.

## Commands

```sh
carillon web serve --config carillon.toml --provider web --token-file credentials/web.token
carillon web setup           # check the entry, its credential and settings; asks nothing (run by `carillon setup`)
carillon web password        # choose the password, which signs every browser out
```

`password` asks twice at a terminal. Otherwise it reads one line from
standard input, so a script or a password manager can pipe it in. Space at
either end is not part of the password, and it needs 12 characters or more.
It writes the hash and sends the provider no request, so it works whether or
not the provider runs. Running it again, even with the same password, signs
every browser out and voids every file link. Both commands take
`--provider ID` when the registry has more than one web entry.

## Open it from your other devices

Setup has the provider listen on loopback. To reach it from your other
devices, put it on your Tailscale network with `tailscale serve`, as root or
once `sudo tailscale set --operator=$USER` has run. Name the provider's
`url`, which is `http://127.0.0.1:43804` with the default ports, and serve it
at the same port number on the tailnet:

```sh
tailscale serve --bg --https=43804 http://127.0.0.1:43804
```

Without `--https`, Tailscale takes 443, which other services often want and
which Funnel can publish. Funnel uses only 443, 8443 and 10000, so keep the
chat on any other port, even if the provider's own is one of them. The two do
not collide: Tailscale listens on the tailnet, the provider on loopback.

Tailscale asks you to enable HTTPS certificates for the tailnet the first
time. Add the address it prints to the provider's settings file,
`providers/<id>.toml`, and run `carillon restart`:

```toml
# Addresses the chat is opened at besides its own, such as the one `tailscale serve` gives it.
addresses = ["https://<machine>.<tailnet>.ts.net:43804"]
```

Use `serve`, never `funnel`: Funnel puts the chat on the public internet.
Being on the tailnet does not sign a browser in. Each browser signs in with
the password.

`tailscale serve` passes the browser's `Host` on as it is, so the provider
must know the address. An address is `https`, a host and an optional port,
as a browser writes an origin. Plain `http` is refused: a browser gives such
a page of another machine no secure context, where the page cannot work,
and the password would cross the network unencrypted. The provider refuses
to start, and `carillon web setup` refuses the file, when it holds anything
else.

## Who may use it

Whoever knows the password may use the chat, from this machine or any
address you named. Any page your browser opens can send requests to
loopback, so the provider trusts nothing a browser attaches on its own.

- The password's hash is the root secret: Argon2id, in PHC form, with a
  salt of its own. Its file has mode 0600, so only the installing user
  reads it. The provider reads it again for every request that presents a
  password, a token or a file link, and keeps no key between requests. A
  new file takes effect on the next request.
- The page trades the password for a token and keeps the token in
  `localStorage`. The token is the same for every browser and lasts until
  the password changes.
- A file that holds anything but a whole hash, salt and output included,
  signs no one in. Replace a damaged one with `carillon web password`
  rather than editing it.
- The provider checks one password at a time, and a wrong one holds the next
  for a second, so guesses come at about one a second, all browsers
  together. A guess keeps its turn when its request is dropped.
- Every `/api` request carries `Authorization: Bearer <token>`. No cookie
  carries authority.
- `Host` must be the address the provider listens on or one in
  `addresses`, as a browser writes it, or the answer is 421. `localhost` is
  another name and is refused.
- An `Origin` other than one of those addresses is 403, a preflight's
  included. No answer has a CORS header.
- The page runs under a content security policy that allows only its own
  scripts, styles, images and fonts.
- The token and every file link are HMACs of the hash's text. A new
  password writes a new hash, which voids them all.

The page's address belongs to whichever local program holds the provider's
port. While the provider is stopped, another program of this machine can
take the port and serve a page at the chat's origin, the tailnet address
included, since `tailscale serve` forwards to that port. Such a page can
read the token the real page stored and ask for the password. Carillon
trusts the machine's local programs here, as its other providers do, and
remote admission is the boundary. A new password voids what such a page
could have taken.

An installation updated from 0.1.0-beta.10 or earlier keeps an unused
`browser.key`. Its browsers are signed out until you choose a password.
Reload a page that was open across the update, which still runs the old
script.

## The page's files

The page is plain files, read from disk on each request. `serve` finds the
release's copy in `lib/web` beside the directory that holds the program,
which is `app/lib/web` in an installation. In a checkout, build the page with
`mise run build:web` and name its folder: `--site packages/web/dist`.

A complete copy in `state/components/<id>/site/` is served instead of the
release's. A copy is complete when it has an `index.html`. No file comes
from the release while the copy is served. The directory is flat, and the
provider serves `.html`, `.js`, `.css`, `.svg`, `.png` and `.woff2` files
from it.

Only a regular file in the folder is served. A symbolic link inside the
folder is answered with 404, because the page's files need no token and such
a link could name any file you can read. The folder itself, and the path to
it, may be reached through links, and whatever folder they lead to is the
page. So a `site` that is a link to a folder with an `index.html` counts as
the custom page, and everything servable in that folder is public to this
machine.

### What the page is made of

The page's source is the package [`packages/web`](../../packages/web),
written in Svelte 5 and TypeScript. Vite builds it to one flat folder of
static files, and that folder is what a release ships and the provider
serves. [Its README](../../packages/web/README.md) says where each fact the
page shows lives, how its tests and browser checks are laid out, and which
rules its code is held to.

[`style.css`](../../packages/web/public/style.css) is the page's one
stylesheet. It has both colour schemes as one named palette at its top, and
the phone layout at its end. Every string the page shows goes in as text.
The one exception is the HTML the provider rendered for an answer.

In the browser the page keeps the token and, for each conversation, the
text not yet received by Carillon: what is in the composer and, until its
receipt, the message on its way. It keeps no event ID and no file, so a
reload sends nothing by itself. A receipt clears only the text that was
sent. Tabs share this storage. Two tabs that hold different unsent text for
one conversation store the text typed last, and a tab stores its own again
when another tab's receipt clears the draft.

Answers are set in Literata, by the Literata Project Authors, under the SIL
Open Font License 1.1.
[`literata.woff2`](../../packages/web/public/literata.woff2) and
[`literata-italic.woff2`](../../packages/web/public/literata-italic.woff2)
are subsets of it for Latin text, and
[`literata-OFL.txt`](../../packages/web/public/literata-OFL.txt) is its
license.
A copy of the page without the font files reads in the faces `style.css`
names after it.

## Browser API

The routes under `/api` are the page's own. The provider and the page ship
together, so the API changes with the page, and it is no interface for
another program. Every route but `POST /api/sign-in` needs the token as
`Authorization: Bearer <token>`.

A conversation is named by its agent and a UUID the page mints, lowercase
with hyphens, in every request. A message's event ID is `m-<uuid>`, minted
once per message and sent again with every retry. Times are Unix seconds.

| Request | What it does |
| --- | --- |
| `POST /api/sign-in` | Trades the password for the token. |
| `GET /api/stream?conversations=<n>[&agent_id=<agent>&audience_id=<uuid>[&turns=<n>]]` | Streams what the view shows. |
| `PUT /api/conversations/<agent>/<uuid>/messages/<event id>` | Sends a message with its files. |
| `GET /api/conversations/<agent>/<uuid>/commands` | Lists the commands the agent's runtime offers, for the page to suggest. |
| `POST /api/messages/<event id>/control` | Steers, interrupts or stops the turn of a message. |
| `GET /files/<path>?name=&exp=&sig=` | Reads a file by a link a snapshot gave. It needs no token. |

### The stream

The stream is the page's one source of what the server knows. It is a
stream of server-sent events, each one `data` line of JSON with a `kind`:

- `snapshot` holds everything the view shows, and the page replaces what
  it had with it. A snapshot has the configured agents, core's limits on a
  message's files, whether the page served is your copy, the conversations,
  the latest active first, and the open conversation's newest turns. The
  first event is a snapshot, and another follows whenever what the view
  shows has changed.
- `unavailable` says the provider is here and core is not: away, as it is
  for a moment at every restart, or failing. It ends the stream.
- `signed_out` says the password changed, so the token is void. It ends the
  stream. The provider reads the password again before every message and
  every keep-alive, so an open page learns of a new one within ten seconds.

The provider also ends every stream at UTC midnight, with no last message.
It asks core for a view once for each stream and never again: the page
opens the next stream, at once after a midnight and after a wait otherwise.
A comment every ten seconds keeps the connection open.

`conversations` and `turns` are windows of 1 to 500. A view that is not one
is a 400 with a plain-text body. A conversation core has no message of is
open all the same and reads as unrecorded.

Only what a person may see crosses. An answer and an interruption's notice
arrive as HTML the provider rendered from Markdown: raw HTML is shown as
text, a link stays one only to `http`, `https` or `mailto`, and an image
becomes a link. A message's text arrives as written and is text, never
markup. A running turn has what its runtime last said it was doing and the
controls a person may use now. An ended turn has its ending and the
runtime's final report. Where a file is on this machine, its hash, and a
conversation's channel stay behind.

### Files

A file has a `name`, a `size` in bytes, a `type` and a `url`.

- The `url` is opaque and needs no token, so it works in `<img>` and as a
  download link.
- A stream mints its links for the UTC day it opened on, so the browser
  keeps the images it fetched. A link works until the end of the next UTC
  day. After that it is 403, and the stream that followed the midnight has
  given the page another.
- A new password voids every link at once: the next request for one is 403.
  The bytes a browser already cached or saved remain with that browser.
- A file core no longer has is 404.
- A `Range` request is answered with the whole file.

`type` is the type the link answers with. A PNG, JPEG, GIF or WebP file,
by its bytes, has its image type and is shown in place. Every other file,
SVG and HTML included, is `application/octet-stream` and downloads.

### Sending

A message is multipart form data: one `text` part and a `file` part for
each file. A message needs text or a file. A browser writes `"`, a carriage
return and a line feed in a file's name as `%22`, `%0D` and `%0A`, and the
provider reads those three back, so a file keeps the name it has on disk.

The 200 is core's receipt, `{"turn_id": "…", "duplicate": false}`: core has
the message, files included. Core answers a repeat of an event ID with the
first receipt and `"duplicate": true`, whatever the repeat contains, and
submits nothing twice. So after a 503, or a request that got no answer, the
page sends the same request again.

The provider decides whether a message invokes one of the runtime's
commands, and the turn's snapshot says what it decided. A text that begins
with `/` asks the runtime for its commands, once, and is a command when its
first word is `/` and one of their names. A runtime that does not answer
within six seconds leaves the text a message, so no message waits for a
list. A text that begins with anything else asks nothing.

| Status | Meaning |
| --- | --- |
| 400 | The body is not a readable multipart form, has a part that is not `text` or `file`, has a second `text` part, or has neither text nor a file. |
| 413 | Too many files, a file or the message's files over core's limits, or text and file names that together make more than core reads in one event, which is about 2 MiB. |
| 422 | Carillon has no agent of that name. |
| 503 | Carillon cannot take it now. |
| 507 | Core's file store is full. Sending again does not help. |

A request can stay open for minutes: while the upload is going, while core
copies the message's files, which takes up to five minutes for large ones,
and while core is away, which the provider waits out as it does at every
restart. If the browser leaves after its upload finished, the provider
still submits the message. A refusal is given once the whole body is read:
a browser that is answered while it uploads reports a broken connection.

### Refusals

An answer that is not 2xx from the provider or core is
`{"error": "<words a person can read>"}`. A request that cannot be parsed
has a plain-text body, and a path that is no route has none.

| Status | Meaning |
| --- | --- |
| 401 | The token is not the current password's: never signed in, or the password changed. |
| 403, 421 | The request did not come from the page's own origin, or named another host. A page loaded from the provider never sees these. |
| 503 | Carillon cannot answer now: core is away or failed, or the provider is stopping. The body is always the same sentence. |

The provider logs what core said when it answers 503 for core, or ends a
stream as `unavailable`. The page is never sent it.

Told to stop, the provider ends every stream as soon as its page reads
again and gives the requests in flight two seconds to be answered. A
message cut off there is not lost: the page sends it again under the same
event ID.

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
- [`browser.rs`](src/browser.rs): the password, its hash and sign-in, the
  token and signatures, and the addresses the gate admits.
- [`identity.rs`](src/identity.rs): how a conversation and a message map
  onto core's identities.
- [`stream.rs`](src/stream.rs): the page's stream, from its first message
  to how it ends. [`snapshot.rs`](src/snapshot.rs): what a snapshot holds,
  made from core's.
- [`api.rs`](src/api.rs): the browser API's other routes, and what the page
  is told when core fails. [`send.rs`](src/send.rs): a message and its
  files, and whether it is a command.
- [`files.rs`](src/files.rs): signed file links. [`markdown.rs`](src/markdown.rs):
  answers as HTML.
- [`site.rs`](src/site.rs): how the page's files are served.
  [`packages/web`](../../packages/web): the page.
