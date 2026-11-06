# Carillon operations

This guide covers installing, running, configuring, updating and backing up
Carillon.

## Requirements

- x86_64 Linux with glibc 2.39 or newer and a systemd user manager.
- GNU coreutils 9.5 or newer for `carillon update`, which swaps the
  application directory atomically with `mv --exchange` on a filesystem that
  supports it (ext4, Btrfs and XFS do). Without it, an update is refused
  before anything stops.
- A Discord application with a bot in one server (guild), invited with the
  `applications.commands` scope.
- A model account Pi can use, or a [T3 Code](../../packages/runtime-t3/README.md)
  server.

Everything runs as the installing Unix user, with that user's files, groups,
programs and sudo rights. Carillon adds no permission system of its own; the
agent can do what you can.

Carillon is a public beta. Each release reads only its own state and
configuration format. When a format changes, `carillon update` refuses before
stopping anything and the release notes say how to move forward. There are no
migrations.

## Install

The [README](../../README.md#install)'s command downloads the newest release
and runs its installer as you, the user who will own Carillon (not root).
Without Discord options, the installer asks at the terminal:

1. **The runtimes.** Pi, a T3 Code server already running on this machine,
   or both (`--runtime pi` and `--runtime t3` answer it). Pi runs Carillon's
   assistant, the agent with Discord's context and Carillon's resources. T3
   runs T3 Code channels.
2. **T3** asks for its address and a token (`t3 auth session issue --label
   Carillon --token-only` prints one), and the projects T3 Code channels
   work in: each becomes an agent that is plain T3 Code through Discord.
   Then it asks each agent's model.
3. **Pi** opens when an agent runs on it: sign in with `/login`, then
   leave with Ctrl+D. Setup checks that Pi is signed in and offers to open
   it again until it is. Then it asks the model of each Pi agent that has
   none yet.
4. **Discord.** The bot token, not shown as you type. Setup learns the bot
   from it, waits until Message Content Intent is on, prints the
   invitation link if the bot is in no server yet, and offers the bot's
   servers and text channels by name (with the ID where names repeat), who
   answers in each channel, and when each agent answers (every message or
   only mentions; a thread per conversation or the channel as one). It
   checks that the bot may answer that way in each channel, and asks again
   until it may. Then who it answers: the bot application's owners, and
   anyone else by Discord user ID. Only those people can talk to the agent.
5. Carillon starts.

Ctrl+C or Esc cancels; run the installer again to finish, and it offers to
keep what's already set up (the Discord bot once Discord confirms its saved
token). Kept T3 settings ask the model of each T3 agent that has none, and
reach T3 only for those agents. The installer refuses while Carillon runs:
stop it with `carillon stop` first, or update it with `carillon update
ARCHIVE`. On an installed release, `carillon setup` asks the same
questions, and `carillon discord setup` asks only Discord's. A rerun that
changes the T3 settings or the bot token takes effect in a running Carillon
at `carillon restart`. Changing an
agent's T3 project makes the first message in each existing conversation open an
empty thread in the current project and receive an explanation. That message
never runs; the next message runs in the new thread. Commands are refused
without creating anything until that thread exists, then use it. Returning to the original project resumes its
original thread. A conversation without a T3 thread yet starts in the current
project. Work already running in another project continues there.

An explicit `--runtime` adds that runtime to an existing installation.
Pi sign-in is skipped when no agent runs on Pi.

The installer creates `~/.carillon` (or `CARILLON_HOME`), registers the user
services (`carillon.target` with core, Discord and both runtimes; a
runtime you didn't choose ends at once) and links `~/.local/bin/carillon`.
It never replaces an unrelated program there. If `~/.local/bin` isn't on
your `PATH`, run `~/.local/bin/carillon` instead of `carillon`, or add that
directory to your `PATH`. With Pi, `--workspace` sets the assistant's
folder (default: your home). `--base-port` moves the loopback ports
(default 43800, up to 43803).

To start Carillon at boot without logging in, an administrator can run
`loginctl enable-linger USER`.

### Without questions

With the Discord options, the installer asks nothing:

```sh
curl -fsSL https://raw.githubusercontent.com/notes-supply/carillon/main/install.sh |
  sh -s -- --guild-id GUILD_ID --bot-user-id BOT_ID \
  --user-id YOUR_DISCORD_ID --route CHANNEL_ID:always:thread:assistant
```

- `--user-id` may repeat; only these people can talk to the agent.
- `--route CHANNEL:ACTIVATION:SCOPE:AGENT` may repeat. `ACTIVATION` is
  `always` or `mention`; `SCOPE` is `thread` (each thread is a conversation)
  or `channel`; `AGENT` is the agent that answers there, `assistant` in a new
  installation.
- Carillon's assistant runs on Pi; set T3 Code channels up afterwards with
  `carillon setup --runtime t3`.
- `--version VERSION`, first, installs that release instead of the newest.

Then put the bot token in `~/.carillon/credentials/discord-bot.token` (mode
0600), sign in to Pi with the printed `carillon-pi` command, and run
`carillon start`. Choose the assistant's model last, with `carillon settings
--agent assistant model=PROVIDER/MODEL`. A value that names no model lists
the models Pi offers.

Rerunning the installer with options keeps the registry, credentials and
state, and writes the guild, bot and people from them. A `carillon.toml`
copied from another installation, without its state, gets a new database
(see [Update](#update)). `--route` only creates the first routes: a rerun
must name the routes the settings already have, and changes a route only
through [Settings](#settings). Use `carillon update` for new releases.

### From the downloaded archive

```sh
version=0.1.0-beta.2
base=https://github.com/notes-supply/carillon/releases/download/v$version
curl -fLO "$base/carillon-$version-linux-x86_64.tar.gz"
curl -fLO "$base/SHA256SUMS"
sha256sum -c SHA256SUMS
tar -xzf "carillon-$version-linux-x86_64.tar.gz"
"carillon-$version/install.sh"    # or with the options above
```

## Everyday commands

```sh
carillon status              # health of core and each provider
carillon start
carillon stop
carillon restart
carillon logs -f core        # or discord, runtime-pi, runtime-t3; without a name, all logs
carillon recent              # recent turns
carillon inspect TURN_ID     # one turn's state, output or failure
carillon check               # does this program accept the installation? (read-only)
carillon settings            # each routed channel's settings, one line each
```

An enabled runtime with no assigned agents is `idle`. Core does not probe it,
and it does not delay `start`, `restart`, or `update` readiness.

`start` and `restart` wait up to `--timeout` seconds (default 60) for every
interface and runtime serving an agent to answer. A provider whose version differs from the one recorded in
`carillon.toml` shows `version_mismatch`; that's informational.

`carillon --home PATH` (or `CARILLON_HOME`) picks an installation; otherwise
the command uses the installation it belongs to, then `~/.carillon`. One
owner can have several installations, but the user services run one at a
time; switch with the other installation's `app/bin/carillon-register-services`.

### Restart and update drain first

`restart` and `update` pause core's runtime dispatchers and stop the services
only once nothing would be lost, waiting up to `--timeout` seconds:

- no turn is being handed to its runtime;
- answers are delivered while Discord still runs;
- every running runtime says a restart loses nothing it alone knows. A
  runtime that isn't running, such as one that is disabled or serves no
  agent, holds nothing.

Turns a runtime holds are not waited for: it takes each up again once it
starts, and none is replayed. New messages are accepted meanwhile and wait. If the drain doesn't finish in
time, or core or a running runtime doesn't answer, nothing is stopped and the command
says what remained. After a drain that ran out of time, core's paused
dispatchers resume within 15 seconds, when the pause lapses. `--force`
restarts anyway: a runtime that was not ready may lose what only it knew,
and native work a runtime started may carry on. `carillon stop`, and restarting a single unit with `systemctl --user`, do
not drain.

The T3 runtime refuses a restart while it remembers a conversation whose
native work it couldn't confirm stopped, or while a person's stop waits for
T3 to confirm it; only that process knows, and restarting forgets it. Use
`--force` once you've checked T3 yourself.

A drain needs core to answer, so a new `core.listen` or operator credential
in `carillon.toml` needs a different path: the command would ask core at the
new address or with the new credential. Make the change while Carillon is
stopped (`carillon stop` once `carillon status` shows no running turns, edit,
`carillon start`), or use `restart --force`.

## Settings

`/settings` in a routed Discord channel, or in one of its threads, shows
that channel's settings. Only allowlisted people in the configured server
can run it. In a channel of that server with no route, it shows one line,
`Agent`; choosing an agent routes the channel to it, answering only when
mentioned, a thread per conversation. Nothing reaches a runtime there before
that.

```text
#home
Agent     assistant
Listens   always
Replies   threads
Model     GPT-6.1 Sol · default
Thinking  Medium · default
```

`· default` marks a value the channel does not set itself: its agent's, or
its model's default. Pick a line, then a value. The change applies at once
and the page redraws. A model change applies to the next message each
conversation in the channel receives. Changing any model line, Thinking
included, gives the channel its own model, which no longer follows the
agent's until **Reset** on Model. An option the channel did not choose
follows the agent's while both run the same model. **Reset** on an option
such as Thinking returns it to the agent's value, or to the model's default
on another model, and keeps the channel's model. Choosing another model
returns its options to that model's defaults. On T3,
a message keeps the model it started with. On Pi, a conversation runs one
model at a time: the next message moves the whole conversation to the new
model, an answer still in progress there included.

If someone changed the line after the page was drawn, nothing changes and
the page shows `⚠ Changed meanwhile`. If the runtime did not answer in time,
the page shows `⚠ Not confirmed`, and the change may or may not have
applied. Look at the redrawn page before you change it again.

Agent, Listens and Replies are the channel's route. Agent moves the channel
to another agent, whose model lines the page then shows. Listens is
`mentions` or `always`, and Replies is `threads` or `channel`. A route
change applies to the next message; a message already admitted keeps the
place its answer goes. The Discord provider writes the change to its
settings file and serves it. If the file has changes nobody applied, the
route is not changed. The page says to run `carillon discord settings
apply` first, or `carillon restart` when one of those changes needs a
restart. `/settings` cannot remove a route.

Direct messages have one page of their own, `dm`, at the terminal only:
Discord offers Carillon's commands in the server, not in direct messages.
Its only route line is Agent, the agent that answers allowlisted people's
direct messages to the bot; its model lines apply to every person's direct
messages. Until an agent is chosen, direct messages are refused and
`carillon settings` doesn't list the page. Setup chooses the assistant when
the installation has it, also when you keep the saved bot and settings.

The terminal shows the same pages:

```sh
carillon settings                              # one line per routed channel
carillon settings CHANNEL_ID                   # that channel's page
carillon settings CHANNEL_ID thinking=high     # change lines, left to right
carillon settings CHANNEL_ID reset model       # take the agent's model again
carillon settings CHANNEL_ID agent=t3-carillon-t3-code listens=mention
carillon settings dm agent=assistant           # who answers direct messages
carillon settings --agent assistant            # the agent's own values
carillon settings --agent assistant model=openai-codex/gpt-6.1-sol
```

A key names a line, such as `model`, `thinking` or `agent`. A value is a
choice's ID or name: `listens=mention` or `listens=always`, `replies=thread`
or `replies=channel`. A value that matches no choice lists the choices. Each change uses
the page the one before it returned, and the command stops at the first
change that is refused, naming its key and the keys already applied.

The agent's value is its channels' only default. An agent without one fails
every message with an explanation that names `carillon settings --agent`.
On T3, a model picked in T3's own UI for a Carillon thread lasts until
Carillon's next turn in that thread.

## Configuration

`carillon.toml` is the registry: core's address, the agent, and each
provider with its role, address, credential hash and expected version.
Unknown fields make the file invalid. Each provider keeps its own settings in
`providers/ID.toml`.

Setup, `carillon init` and `carillon provider` change `carillon.toml` as
Discord changes its settings file, described
[below](#changes-that-apply-while-running): they write only the values a
change alters, each in its place, and match providers and agents by ID. The
file keeps its comments, its order, its quoting and its line endings, except
as listed there. A new provider or agent goes after the others.

### Changes that apply while running

```sh
carillon registry                     # what core serves, and how carillon.toml differs
carillon registry apply               # serve the file's live changes now
carillon provider runtime-t3 disable  # or enable; any runtime's ID
```

A runtime's `enabled` applies live. Disabling a runtime
refuses new messages for its conversations and starts none of its queued
turns; running turns finish and are delivered. Enabling or disabling changes
what core does, not whether the runtime's program runs: `carillon start`
starts an enabled runtime that isn't running. Everything else in
`carillon.toml` (addresses, credentials, the agent, the set of providers)
needs `carillon restart`; an apply that meets such a change refuses the whole
file and says which fields.

Discord's admission settings apply live too. `carillon discord setup` asks
for them at the terminal, or takes them as options and asks nothing:

```sh
carillon discord setup --guild-id GUILD_ID --bot-user-id BOT_ID \
  --user-id YOUR_DISCORD_ID --route CHANNEL_ID:always:thread:assistant
carillon discord settings          # what the provider serves vs. the file
carillon discord settings apply    # serve the file's live changes
```

`--route` only creates the first routes. Once the settings route channels,
setup refuses other routes and leaves the file as it is; change a route
with `carillon settings` or `/settings`. When the settings name no agent
for direct messages, setup names the assistant, if the installation has
it, whether you keep the saved bot and settings or answer every question,
and its last line says who answers direct messages:

```toml
# providers/discord-private.toml
[direct_messages]
agent = "assistant"
```

Users, routes, `direct_messages`, `typing`, `reactions` with their emoji, `activity_markers`
and the [module](#discord-module) apply live. The guild, bot user, bot token file, attachment limits and
`activity` need a restart of the Discord provider. A message keeps the
settings it was admitted with. While the provider runs, it is the only
program that writes its settings file: setup, `carillon discord module` and
route changes send their change to it, and it writes and serves the file.
It writes only the values that a change alters, each in its place. The file
keeps its comments, its order and its line endings. A missing final newline
stays missing, and defaults the file leaves unstated stay unstated.

Routes are matched by channel and users by ID. Each keeps its line and its
comments. A removed one goes with its line and the comments above it. Comments
a blank line sets off from it stay. A removed one that shares its line with
another goes alone. Setup answers that list routes or users in another order
change nothing. New ones are added at the end. Naming an agent for direct
messages adds the `[direct_messages]` table at the end of the file, above
any comment that ends it, and a later change of that agent edits its line.

A changed string keeps its single quotes when the new value fits in them,
which means it holds no single quote and no line break. Other changed values
are written in their plain form. A plain string is in double quotes. It is in
single quotes when it holds a `"` or a backslash but no single quote. It is
in `"""` when it holds a line break.

A change and its undo leave the file byte for byte as it was, except in these
cases:

- A user or route the change removed comes back without its comments, and
  setup adds it at the end of its list. In a list written over several
  lines, a line the removed user shared with another can come back split in
  two.
- The comments a blank line sets off above a `[[routes]]` table go with it
  when a change removes that route and every route after it, and adds none
  after them.
- `carillon discord module disable` keeps the module's path, so a module
  enabled with a path the file lacked, then disabled, leaves `[module]` with
  `enabled = false` and that path.
- Module settings that the undo leaves at their defaults, disabled with no
  path, go unless a comment is on or above them, as a new file leaves them
  unstated.
- A changed string comes back in its plain form, unless it was in single
  quotes and both values fit in them. So `'''x'''` comes back as `'x'` when
  both fit, `"\u0061"` as `"a"`, and `'x'` as `"x"` when the change's value
  held a single quote.
- A user list that names a user twice is written again whole, on one line,
  when a change drops one of them.
- A file with CRLF line endings anywhere comes back with CRLF throughout.

A change whose file is in place but whose folder could not be synced
serves, and is reported as made but not confirmed durable: a system crash
could still bring back the previous file.
It refuses a change while the file has edits nobody applied. A change to
what only a restart applies is written and served at the next restart;
until then, setup can change the file again, and route and module changes
are refused.
While the provider is stopped, setup and `carillon discord module` change
the file themselves. A route change needs the running provider: while it
is stopped, `carillon settings` fails and changes nothing.

### Conversations at once

Each runtime holds the turns core hands it and orders them itself, answering
in every conversation at once. T3 runs a conversation's messages in order;
Pi handles a message that arrives while it answers as below.

### Messages while Pi answers

A message that arrives while Pi answers in its conversation steers that
answer by default: Pi reads it after its current tool call and answers both
together. The agent's Busy setting changes that:

```sh
carillon settings --agent assistant busy=queue
carillon settings --agent assistant reset busy    # steer again
```

`queue` answers it after the running answer, and `interrupt` ends the
running answer and answers the new message, with every waiting one, next.
A waiting message offers reactions to steer it in, interrupt for it, or stop
it. The change applies to the next message Pi receives; a message already
received keeps the mode it arrived with.

### What Pi can do besides answer

Pi can answer from its own history, such as what you discussed last week
about a topic or in a thread, and how many tokens it used this week. With
`coding` selected, as by default, its prompt names the SQLite file it records
conversations in, and the conversation history skill, which a release ships
in `CARILLON_HOME/app/lib/pi-skills/`, tells it how to read that file
read-only with `sqlite3`, or with the SQLite in Pi's own executable where
`sqlite3` is not installed. Token counts are what the provider reported;
the dollar figure is the model catalogue's estimate, not a bill, and a
ChatGPT subscription is not billed per token. A skill of the same name in
the agent's `resources` replaces it.

Pi has no web search of its own. [Add web search](WEB_SEARCH.md) gives it
one as a skill that calls a search provider's API.

The agent's Extensions setting selects more of Pi's built-in extensions.
None of these is selected by default:

```sh
carillon settings --agent assistant
```

```text
assistant
Model       GPT-6.1 Sol
Thinking    Medium · default
Busy        steer · default
Extensions  coding, tool-search, discord · default
```

```sh
carillon settings --agent assistant extensions=coding,tool-search,discord,schedules,subagents
carillon settings --agent assistant reset extensions    # the default again
```

`extensions=` takes the whole list, comma separated, and refuses a name the
agent does not have, listing the ones it has. A selected extension the agent
no longer has, such as a deleted file or an `mcp:` server `mcp.json` no
longer configures, shows as unavailable, and Pi refuses messages until a
change removes it. Pi offers only the files in `extensions/` whose names
hold no comma. Busy and Extensions are kept in `settings.json` in the
agent's `resources` directory, which is `CARILLON_HOME/agents/default/` for
the assistant setup creates. A change rewrites that file keeping everything
else it holds, its mode included; you can also edit that
file by hand, such as `{ "extensions": { "add": ["schedules"] } }`. Either
way the next message Pi receives brings the change, and from then on the
agent's extensions apply to every request it makes, also for an answer
already in progress. An agent without resources shows both lines and cannot
change them, and one whose `settings.json` cannot be used shows them empty
and names the problem when you change one. `/settings` in Discord shows the
channel's page, which has neither line.

- `schedules` lets the assistant run a prompt later in the same
  conversation, once or on a cron expression, such as "every weekday at 9".
  Each run's answer arrives as a message of its own, like any answer.
  Schedules survive restarts; a run missed while Pi was down happens once
  when it starts again, and runs held up while core was unreachable happen
  once when it is back. Remove `schedules` to end each one when it is next
  due.
- `subagents` lets the assistant hand a task to a helper that starts with
  its model and tools but none of the conversation's history. The helper
  works in the background by default, and its result arrives later as a
  message of its own; stopping the answer that started it doesn't stop the
  helper.
- `mcp:<server>` gives the assistant the tools of an MCP server, which it
  finds with tool search. Configure servers in `CARILLON_HOME/pi/mcp.json`,
  then `carillon restart`. It holds their credentials, so make it readable
  only by you, `chmod 600`; Pi refuses to start with one other users may
  read or write, as ssh refuses such a key:

  ```json
  {
    "mcpServers": {
      "files": { "command": "mcp-files", "args": ["--root", "/srv"] },
      "notes": { "url": "https://mcp.example.com/mcp", "headers": { "authorization": "Bearer …" } }
    }
  }
  ```

  A server, named in lowercase words joined by `-`, runs a program
  (`command`, with optional `args`, `env` and `cwd`) or is reached at an
  `http:` or `https:` `url` (with optional `headers`). Pi refuses to start
  with a file it cannot use, and `carillon logs runtime-pi` says why. A
  server Pi loses offers no tools until it reconnects.

## Discord

`/settings` shows and changes the channel's settings, as
[Settings](#settings) describes.

`/command` runs one of the runtime's own commands or skills in the
conversation, exactly as written:
`/command invocation:compact keep the decisions` sends `/compact keep the
decisions`. Plain `/command` lists them.

Both commands are admitted like messages: only in the guild, from allowlisted
users, in a routed channel or a thread under a thread route. `/settings`
also opens in a channel of the guild with no route, to route it. Neither
runs in direct messages.

### Direct messages

An allowlisted person can message the bot directly. Their direct messages
are one rolling conversation with the agent the settings name for direct
messages ([Settings](#settings)); there are no threads. Messages from anyone
else, or while no agent is named, are dropped before core sees them, as in
an unrouted channel. Files, the person's and those the agent attaches to its
answers, and reactions work as in a channel.

An agent can also write to an allowlisted person first with its
[Discord tools](#agent-discord-tools), from any of its conversations, such
as a `#home` thread. The provider keeps what agents send a person this way,
the latest ten, until that person next writes to the bot. Their next direct
message, whether or not it uses Reply, opens with a quote of each, oldest
first, naming the agent that sent it and the channel it was sent from:

```text
You sent them this direct message at 2026-11-05T14:03:00+00:00 from <#1302543210987654321>:
> PR #12 is ready for review.

Merge it.
```

`You` is the agent the direct message goes to. Another agent is named, as
in `Agent "code" sent them this direct message …`, and a message sent with
`carillon discord call` is from `Another agent`. Once core accepts the
direct message, those quotes are done: the next one doesn't repeat them.
What an agent sends from the person's own direct-message conversation is
already part of it and isn't quoted. While no agent is named for direct
messages, the person's answer is dropped like any other direct message.

Discord sends direct messages only to a gateway session that asked for
them, and doesn't replay them later. A direct message sent before the
provider first ran a release with direct messages, such as during the
update that brought them, never reaches Carillon: it gets no reaction and
no answer. Send it again.

### Replies and forwards

A message that replies to another, or forwards one, reaches the agent with
a quote of that message before the person's own words:

```text
In reply to your message, sent 2026-11-05T14:03:00+00:00, with files ["timetable.pdf"]:
> Shall I book the 9:40 train?

Yes, book it.
```

The first line is Carillon's: whose message it is, when it was sent and
its files' names. Every line of the quoted words starts with `>`, so nothing
quoted can pass for Carillon's line, and the quote keeps the message's first
300 characters. A person is named by their display name, as a JSON string,
and their Discord user ID: `a message from "Ada" (Discord user
1302543210987654322)`. A message of the bot is `your message` only when the
agent the turn goes to had the bot send it, as an answer, the messages
that carry its files included, or with its Discord tools; another agent's
is `a message from agent "code"`. The provider records which agent had the
bot send each of its latest 1,000 messages. A reply to an older message of
the bot, or to one it didn't record, such as an activity card or a message
sent with `carillon discord call`, is to `a message from another agent`. A
message an agent sent with its tools from another channel says where:
`sent … from <#1302543210987654321>`.

Discord doesn't say who wrote a forwarded message, so its quote reads
`Forwarding a message`. A forward's files arrive with it, downloaded like
the message's own ([Files](#files)); a reply's quote only names the files
of the message it answers. A reply to a forward quotes the forwarded words
and adds `forwarding a message`. A forward with no words of its own is
answered where a message would be. Only the new message carries the quote:
the conversation before it is unchanged.

### Reactions

The bot reacts to each message it answers: while it works, then with how it
ended. A runtime that holds a message's turn may offer controls as reactions,
which the bot shows while the runtime lists them. While a message waits
behind the agent's running turn, an allowlisted person's reaction hands it to
the running turn now (steer), ends the running turn and runs it next
(interrupt), or doesn't run it (stop). On T3, a running message offers stop
too, which stops its run. The emoji are
settings with these defaults, Unicode or a custom `name:id`, and apply live,
replacing the controls messages show. The controls must differ from each
other and from the rest.

```toml
# providers/discord-private.toml
reactions = true
processing_emoji = "👀"
delivered_emoji = "✅"
failed_emoji = "❌"
uncertain_emoji = "⚠️"
steered_emoji = "↪️"     # handed to the turn before it, which answers both
stopped_emoji = "⏹️"
steer_emoji = "⏩"
interrupt_emoji = "⏭️"
stop_emoji = "🛑"
```

### Files

Allowlisted people can attach any files to a message the agent would answer.
The message is accepted with all its files or not at all; a file that can't be
downloaded or kept gets a short reply saying why. Core keeps accepted files
read-only under `state/attachments` with the turn, and checks each one's
digest before the turn runs. The agent sees every file in its working tools,
and suitable images (PNG, JPEG, WebP, GIF) are also given to the model as
images. A voice message arrives as its audio file, usually
`voice-message.ogg`, and so does a forwarded one: a forward's files count
as the message's own, under the same limits. Nothing is extracted,
converted, transcribed or executed on arrival; an agent that should
transcribe needs a tool for it, with your own transcription key.

The agent can send files with its answer. On Pi it attaches them with its
`attach` tool. T3 answers are text only. A file goes with the answer of the
run that attached it. If that run is stopped, interrupted by a new message or
fails, its files are dropped, and the agent attaches them again with a later
answer.

Before core records the answer, it copies each file Discord can upload into
`state/attachments`, within the limits below, and keeps it with the turn.
Those copies count against `storage_mib` with the files people send. Core
keeps no copy of a file over Discord's upload limit, 20 MiB unless its
`max_upload_mib` says otherwise, or of one that changed or couldn't be read
while core copied it. The answer names each such
file with where the agent had it and why, and the other files are kept.

Discord takes at most 10 files and 25 MiB in one message. The answer's last
message carries the first files, and the files that don't fit follow on more
messages after it, so every file arrives. Delivery sends core's copies, so the
agent's own files may change or go once the answer is recorded, and an answer
that waits for Discord, even across restarts, never runs the model again. To
post files elsewhere, the agent uses its [Discord tools](#agent-discord-tools).

Limits, each applied when its service restarts:

```toml
# providers/discord-private.toml
[attachments]
enabled = true
max_files = 10            # per message (Discord's own maximum)
max_file_mib = 1024
max_message_mib = 2048
concurrent_messages = 2   # messages downloading at once
staging_mib = 2048

# carillon.toml, the Discord provider's [[providers]] entry
max_upload_mib = 20       # the largest file it uploads: 20 when unset, at most 24

# carillon.toml
[core.attachments]
max_files = 10            # per message or answer; 0 refuses every file
max_file_mib = 1024
max_event_mib = 2048      # a message's or an answer's files together
storage_mib = 10240       # all kept files together
free_space_mib = 2048     # space a new file must leave free on the disk
concurrent_captures = 2

# providers/runtime-pi.toml (optional)
[inline_images]
max_count = 4
max_image_mib = 16
max_total_mib = 32
max_megapixels = 40       # decoding a 38-megapixel PNG takes about 0.7 GiB
```

Accepted files are never deleted to make room; new ones are refused instead.

### Activity cards

While the agent works, its conversation shows a card after two seconds: what
it's doing now, recent tool calls with their durations, delegated work, and
the model and context use when the runtime reports them. Bash calls show only
the command's first line; tool output and unpublished reasoning are never
shown. Redaction of credentials in what is shown is best effort. The card is
edited at most every three seconds and stays as a summary when the work ends,
or disappears when the work was only an answer. Turn cards off with
`activity = false` in `providers/discord-private.toml` and restart Discord.

Published reasoning and the live headline render balanced `**bold**`,
`*italic*`, `_italic_` and inline code spans that open and close on one
line. Other Markdown, mentions, links, custom emotes and timestamps in model
text stay literal. A span cut by a length limit stays literal too.

Markers default to:

```toml
[activity_markers]
working = "⏳"
queued = "⏳"
finishing = "⏳"
running = "↳"
succeeded = "✓"
failed = "✗"
uncertain = "⚠️"
stopped = "⏹️"
```

A marker can be a Unicode emoji or symbol, a custom emote (`<:name:id>`, or
`<a:name:id>` for animated), or `""` to omit it; omitted fields keep their
default. Model badges map model-ID prefixes to emotes, longest prefix first:

```toml
[activity_markers.models]
"claude-" = "<:claude:123>"
"gpt-" = "<:openai:456>"
```

Use emote IDs that belong to your bot's application. After editing, run
`carillon discord settings apply`; an invalid marker names the field to fix.
The `/settings` model menu shows these badges beside its models.

### Discord module

Slash commands, their controls and the cards come from a WebAssembly module.
The release bundles one, and an example that colours each card by its job's
state. To try the example:

```sh
carillon discord module enable ~/.carillon/app/lib/examples/card-colours.wasm
carillon discord module            # what serves, and what the settings declare
carillon discord module disable    # back to the bundled module
```

`enable` and `disable` change the `[module]` table of
`providers/discord-private.toml` and apply the settings:

```toml
[module]
enabled = true
path = "/home/you/card-colours.wasm"   # a relative path starts at the installation
```

Applying the settings (`carillon discord settings apply`) rereads the file,
so a module rebuilt at the same path serves after an apply. Discord gets the
module's commands first. A module that can't serve, or whose commands
Discord refuses, leaves the serving module and settings as they were.
Commands already running finish on the module they started with.
`carillon discord module check FILE` checks a module without serving it.

An update checks the enabled module first: the example (or any module inside
`app/`) as the new release ships it, any other file as it is. If the new
release can't serve a module of your own, the update refuses; disable it,
update, then enable one built for the new release. The example's
source is in [examples/card-colours](../../examples/card-colours/README.md).

### Restarts and replay

A graceful stop of the Discord provider records its gateway session, and the
next start within ten minutes resumes it, so Discord replays messages sent
meanwhile. A start of a release that asks Discord for other kinds of events
than the recorded session did, as the first release with direct messages
does, starts a new session instead. Core recognizes messages it already
accepted and doesn't run them twice. Otherwise, messages sent while Discord
was down are not replayed.
Messages refused because too many were waiting (64 in all, 16 per
conversation) get a "too busy" reply when possible and are not replayed;
send them again.

### Agent Discord tools

The [Discord tools](../../packages/discord-pi/README.md) let the assistant
read and manage the Discord server with the bot's permissions, and read and
send direct messages with allowlisted people, so it can, for example, tell
you about work in a `#home` thread directly. On Pi they
are a built-in extension the assistant selects by default; remove it in the
assistant's `settings.json` to turn them off:

```json
{ "extensions": { "remove": ["discord"] } }
```

Your other harnesses (Claude Code, Codex) can use the same operations
through `carillon discord call`, as the skill a release ships in
`app/lib/carillon-discord-SKILL.md` explains. Nothing installs it, because
the tools can administer the server.

## When things fail

- **A turn fails or is interrupted.** Carillon tells the conversation, with
  the runtime's explanation when it has a safe one (a usage limit, for
  example), and never replays it: tools may already have run. Send another
  message to continue. `carillon inspect TURN_ID` and `carillon logs
  runtime-pi` show more.
- **A reply may not have arrived.** Delivery has its own outbox, so an
  answer is never generated twice. Each message of an answer is sent once,
  the messages that carry its extra files included. If Discord refuses one,
  or the Discord provider crashes while it sends text or uploads a file,
  nothing is sent again. The messages that went out stay, and the person's
  message gets the uncertain reaction.
- **A runtime crashes.** Its service restarts it. Pi finishes the answers it
  held: a model call it cut off counts as a failed attempt and is retried,
  and a tool call it cut off is reported to the model as interrupted rather
  than repeated. A message whose answer keeps crashing it fails once Pi's
  retries run out. However often it crashes, every outcome it decided still
  reaches core, and scheduled runs and subagent reports still open their
  turns. T3 takes up the turns it held again, reading each run back from T3,
  and settles a stop core hadn't recorded instead of running the message.
- **Core can't save an outcome** (disk full, for example). The runtime asks
  again until core records it, and the turn shows as running meanwhile.
- **Delegated T3 work** reports later in its own message, even across
  restarts.

## Credentials

The bot token belongs to Discord, model credentials to the runtime (Pi keeps
its own under `~/.carillon/pi`, separate from `~/.pi`). Loopback tokens in
`credentials/` authenticate the services to each other; `operator` authorizes
diagnostics and maintenance. None of them reach prompts, sessions or logs.
Changing a credential takes a restart.

Each provider's settings name its token's file: `bot_token_file` for Discord
(by default `credentials/discord-bot.token`, for a token put there by hand)
and `token_file` for T3. Setup writes a changed token to a new private file
first and only then replaces the settings to name it, keeping the old file.
Until that replacement the previous settings and their token stay in place.
If its durability cannot be confirmed afterwards, a crash may keep either
the old or the new settings, each naming its own durable token.

## Back up and restore

Stop Carillon first:

```sh
carillon stop
carillon backup --output /somewhere/else/carillon.tar.gz
```

The archive is the whole installation, including credentials, sessions and
Pi's sign-in: keep it private and encrypt it before moving it. An external
workspace isn't included. Backup checks the database and every kept file and
captured resource first. To restore (as the same or another non-root user):

```sh
carillon restore --input carillon.tar.gz --target /new/carillon
/new/carillon/app/bin/carillon-register-services
/new/carillon/app/bin/carillon start
```

Restore only accepts a backup of the current format, and doesn't check
provider credentials: check `carillon status` afterwards.

## Update

```sh
carillon update carillon-VERSION-linux-x86_64.tar.gz
```

This release is installed fresh. `carillon update` from an earlier release
refuses its database format and changes nothing. To move to it:

1. Run `carillon stop`, then move `~/.carillon` aside.
2. Create a new `~/.carillon` with mode 700. Copy these from the old one,
   keeping their modes: `carillon.toml`, `providers/*.toml`, `credentials/`,
   `pi/auth.json`, `pi/models.json`, `pi/settings.json`, `pi/mcp.json` if
   you have one, and `agents/`. If the `[module]` table of
   `providers/discord-private.toml` enables a module of your own kept
   inside `~/.carillon`, outside `app/`, copy that file to the same path. A
   relative `path` there starts at `~/.carillon`.
3. Run the new release's `install.sh` the way you installed before.
   - **Guided**, without options. Setup asks no runtime question: it uses
     the runtimes `carillon.toml` registers. With T3, it asks "Keep the T3
     settings and agents?". Yes keeps T3's address, token and projects and
     asks the model of each T3 agent. No asks the address, token, projects
     and models again. With Pi, it says Pi is signed in, then asks the
     model of each Pi agent. Discord checks the saved bot token and asks
     "Keep the saved Discord bot and settings?". Then Carillon starts.
   - **Without questions**, with the Discord options you used before,
     every route included. The bot token and the Pi sign-in are already
     in place, so skip the first two steps it prints. Run `carillon
     start`, then give each agent its model with `carillon settings
     --agent AGENT model=PROVIDER/MODEL`.

   Either way, the installer creates a new database. In `carillon.toml`, it
   sets the `version` of each provider this release bundles
   (`discord-private`, `runtime-pi` and `runtime-t3`) and leaves the rest
   of the file as it was. A provider you installed separately keeps its
   version.

`state/` starts empty, so turn history, conversations, Pi's conversations
and scheduled runs do not carry over. T3 per-thread model choices do not
carry over either. Each channel runs its agent's model until someone picks
one with `/settings`.

Before stopping anything, update unpacks the archive inside the installation,
checks it against its `MANIFEST` and has the new release check your registry,
database, Discord settings and the custom module they enable. It then drains, stops,
swaps the application directory atomically, and starts what was running. It
keeps configuration, credentials, state, sessions and logs. A refusal changes
nothing. If a later step fails, the message says what changed; rerun the
same update until it succeeds before starting services. There's no
downgrade: restore a backup made before the update instead.

After `carillon update`, provider versions in `carillon.toml` are
expectations you change yourself when you mean to run another provider
release; `status` shows any mismatch.

Earlier releases ran Carillon's assistant on T3 through T3 instances of its
own, `carillon_claude` and `carillon_codex`. Nothing uses them now, and T3
offers them to plain T3 Code agents like any other instance, so remove them
in T3's settings.

Maintenance commands take a lock per installation (`.maintenance.lock`) and
one on the owner's user units, and refuse while another holds them.

## Uninstall

```sh
~/.carillon/app/bin/carillon-uninstall
```

It removes the user services, the `~/.local/bin/carillon` link and the
application. Configuration, credentials, sessions and other data stay in the
installation directory; delete it yourself if you no longer need it.
