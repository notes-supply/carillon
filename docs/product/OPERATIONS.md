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
token). The installer refuses while Carillon runs: stop it with `carillon
stop` first, or update it with `carillon update ARCHIVE`. On an installed
release, `carillon setup` asks the same questions, and `carillon discord
setup` asks only Discord's. A rerun that changes the T3 settings or the bot
token takes effect in a running Carillon at `carillon restart`. Changing an
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
state, but rewrites the Discord settings from them. Use `carillon update`
for new releases.

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
can run it.

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
agent's until **Reset** on Model. **Reset** on an option such as Thinking
returns it to its model's default and keeps the channel's model. Choosing
another model returns its options to that model's defaults. On T3,
a message keeps the model it started with. On Pi, a conversation runs one
model at a time: the next message moves the whole conversation to the new
model, an answer still in progress there included.

If someone changed the line after the page was drawn, nothing changes and
the page shows `⚠ Changed meanwhile`. If the runtime did not answer in time,
the page shows `⚠ Not confirmed`, and the change may or may not have
applied. Look at the redrawn page before you change it again. Agent,
Listens and Replies come from the Discord settings file and are not
editable here.

The terminal shows the same pages:

```sh
carillon settings                              # one line per routed channel
carillon settings CHANNEL_ID                   # that channel's page
carillon settings CHANNEL_ID thinking=high     # change lines, left to right
carillon settings CHANNEL_ID reset model       # take the agent's model again
carillon settings --agent assistant            # the agent's own values
carillon settings --agent assistant model=openai-codex/gpt-6.1-sol
```

A key names a line, such as `model` or `thinking`. A value is a choice's ID
or name. A value that matches no choice lists the choices. Each change uses
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
for them at the terminal; `configure` takes them as options:

```sh
carillon discord configure --guild-id GUILD_ID --bot-user-id BOT_ID \
  --user-id YOUR_DISCORD_ID --route CHANNEL_ID:always:thread:assistant
carillon discord settings          # what the provider serves vs. the file
carillon discord settings apply    # serve the file's live changes
```

Users, routes, `typing`, `reactions` with their emoji, `activity_markers`
and the [module](#discord-module) apply live. The guild, bot user, bot token file, attachment limits and
`activity` need a restart of the Discord provider. A message keeps the
settings it was admitted with. Setup and `configure` save the file, then
have a running provider serve it; if it refuses, they fail, unless only a
restart applies the file and this release accepts the module it declares.

### Conversations at once

Each runtime holds the turns core hands it and orders them itself, answering
in every conversation at once. T3 runs a conversation's messages in order;
Pi handles a message that arrives while it answers as below.

### Messages while Pi answers

A message that arrives while Pi answers in its conversation steers that
answer by default: Pi reads it after its current tool call and answers both
together. The agent's `settings.json` changes that:

```json
{ "busy": "queue" }
```

`queue` answers it after the running answer, and `interrupt` ends the
running answer and answers the new message, with every waiting one, next.
A waiting message offers reactions to steer it in, interrupt for it, or stop
it.

### What Pi can do besides answer

The assistant's `settings.json` selects more of Pi's built-in extensions.
None of these is selected by default:

```json
{ "extensions": { "add": ["schedules", "subagents", "mcp:files"] } }
```

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
users, in a routed channel or a thread under a thread route.

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
images. Nothing is extracted, converted or executed on arrival.

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

# carillon.toml
[core.attachments]
max_files = 10            # 0 refuses every attachment
max_file_mib = 1024
max_event_mib = 2048
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
meanwhile. Core recognizes messages it already accepted and doesn't run them
twice. Otherwise, messages sent while Discord was down are not replayed.
Messages refused because too many were waiting (64 in all, 16 per
conversation) get a "too busy" reply when possible and are not replayed;
send them again.

### Agent Discord tools

The [Discord tools](../../packages/discord-pi/README.md) let the assistant
read and manage the Discord server with the bot's permissions. On Pi they
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
- **A reply may not have arrived.** Delivery has its own outbox: an answer
  is never generated twice. If Discord crashed mid-send, the message gets the
  uncertain reaction.
- **A runtime crashes.** Its service restarts it. Pi finishes the answers it
  held: a model call runs again, and a tool call it cut off is reported to
  the model as interrupted rather than repeated. A message that keeps
  crashing it is given up after five tries and fails. T3 takes up the turns
  it held again, reading each run back from T3, and settles a stop core
  hadn't recorded instead of running the message.
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
   you have one, and `agents/`.
3. Run the new release's `install.sh` and answer setup. It keeps the
   Discord settings and the Pi sign-in. It asks T3's address, token and
   projects again, because the old T3 settings name a model, and the model
   now lives in the runtime's settings. It asks each agent's model and
   creates a new database.
4. Run `carillon start`.

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

Provider versions in `carillon.toml` are expectations you change yourself
when you mean to run another provider release; `status` shows any mismatch.

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
