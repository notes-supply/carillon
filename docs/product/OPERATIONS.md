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
   or both (`--runtime pi` and `--runtime t3` answer it).
2. **The assistant.** What runs Carillon's assistant, the agent with
   Discord's context and Carillon's resources: Pi, or T3 with Claude, Codex
   or both, each signed in for Carillon alone. With T3 there may be no
   assistant at all, and only T3 Code channels.
3. **T3** asks for its address and a token (`t3 auth session issue --label
   Carillon --token-only` prints one), the project the assistant works in,
   and the projects T3 Code channels work in: each becomes an agent that
   is plain T3 Code through Discord. For each harness of the assistant it
   makes Carillon's profile, a T3 instance whose home is
   `${XDG_DATA_HOME:-~/.local/share}/carillon/profiles/HARNESS` by default.
   T3's instance `config.homePath` owns the path for an existing profile.
   Setup signs it in with the harness's own login in that home, an API key,
   or a compatible endpoint
   and its token. Then it asks each agent's default model.
4. **Pi** opens when an agent runs on it: sign in with `/login`,
   choose the agent's model with `/model` and press Ctrl+S on it, then
   leave with Ctrl+D. Setup checks that Pi is ready and offers to open it
   again until it is.
5. **Discord.** The bot token, not shown as you type. Setup learns the bot
   from it, waits until Message Content Intent is on, prints the
   invitation link if the bot is in no server yet, and offers the bot's
   servers and text channels by name (with the ID where names repeat), who
   answers in each channel, and when each agent answers (every message or
   only mentions; a thread per conversation or the channel as one). It
   checks that the bot may answer that way in each channel, and asks again
   until it may. Then who it answers: the bot application's owners, and
   anyone else by Discord user ID. Only those people can talk to the agent.
6. Carillon starts.

Ctrl+C or Esc cancels; run the installer again to finish, and it offers to
keep what's already set up (the Discord bot once Discord confirms its saved
token). The installer refuses while Carillon runs: stop it with `carillon
stop` first, or update it with `carillon update ARCHIVE`. On an installed
release, `carillon setup` asks the same questions, and `carillon discord
setup` asks only Discord's. A rerun that changes the T3 settings or the bot
token takes effect in a running Carillon at `carillon restart`. Changing an
agent's T3 project makes the first message in each existing conversation open an
empty thread in the current project and receive an explanation. That message
never runs; the next message runs in the new thread. Model requests, commands,
and follow-up observations are refused without creating anything until that
thread exists, then use it. Returning to the original project resumes its
original thread. A conversation without a T3 thread yet starts in the current
project. Work already running in another project continues there.

An explicit `--runtime` adds that runtime to an existing installation.
Pi sign-in is skipped when no agent runs on Pi.

Each T3 profile keeps its own real `skills/` directory. Carillon links each
`skills/NAME` to the assistant's `resources/skills/NAME`. The instructions
file links to `resources/AGENTS.md`. Serve startup reads each saved profile's
homePath from T3, then adds new links, removes obsolete links, and updates
links after resources move before accepting turns. A saved profile missing
from T3 or without a homePath stops startup. Only directories containing
`SKILL.md` are shared. Harness files and directories stay in the profile.
A conflicting name or a symlink in the source `AGENTS.md` or `skills/` tree stops
startup. This includes linked skill directories, manifests, and nested resources,
which core's admission snapshots also refuse. Each T3 runtime permits at most
one agent with resources.

Profiles belong to the user and are shared across installations. Setup creates
or validates each T3 instance and creates its home for sign-in, leaving links
untouched. `carillon runtime-t3 check` reads settings and the token without
reaching T3 or changing links. Setting up or checking another installation
therefore leaves the serving installation's resources linked. Starting an
installation reconciles the links to its resources. Only one installation's
services run at a time.

`carillon runtime-t3 discover --agent ID --url URL` reads the token on
standard input and returns that agent's model offer. The saved `profiles`
list decides which instances the assistant may use. Plain T3 Code excludes
saved profiles and the reserved `carillon_claude` and `carillon_codex` IDs,
even when they are absent from that list.

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
- `--runtime t3` runs the assistant on T3; set `--workspace` to the T3
  project's folder.
- `--version VERSION`, first, installs that release instead of the newest.

Then put the bot token in `~/.carillon/credentials/discord-bot.token` (mode
0600), sign in to Pi with the printed `carillon-pi` command, or set T3 up
with `carillon setup` (see the [T3 guide](../../packages/runtime-t3/README.md)),
and run `carillon start`.

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

`restart` and `update` pause core's runtime workers and stop the services only
once nothing would be interrupted, waiting up to `--timeout` seconds:

- turns that are running finish normally (none is cancelled or replayed);
- their answers are delivered while Discord still runs;
- every runtime says a restart loses nothing it alone knows.

New messages are accepted meanwhile and wait. If the drain doesn't finish in
time, or core or a runtime doesn't answer, nothing is stopped and the command
says what remained. After a drain that ran out of time, core's paused
workers resume within 15 seconds, when the pause lapses. `--force` restarts anyway: a running message is recorded
as interrupted and not replayed, and native work a runtime started may carry
on. `carillon stop`, and restarting a single unit with `systemctl --user`, do
not drain.

The T3 runtime refuses a restart while it remembers a conversation whose
native work it couldn't confirm stopped; only that process knows, and
restarting forgets it. Use `--force` once you've checked T3 yourself.

A drain needs core and every runtime to answer, so a few changes need a
different path:

- **A runtime that isn't running**, for example one you just enabled:
  `carillon start` starts it and stops nothing.
- **A new `core.listen` or operator credential** in `carillon.toml`: the
  command would ask core at the new address or with the new credential. Make
  the change while Carillon is stopped (`carillon stop` once `carillon status`
  shows no running turns, edit, `carillon start`), or use `restart --force`.

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

A runtime's `enabled` and `parallel_turns` apply live. Disabling a runtime
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

### Parallel conversations

T3 can run several conversations at once: set `parallel_turns` (1 to 16) on
the `runtime-t3` entry and run `carillon registry apply`. Each conversation
still runs one turn at a time, in order.

A runtime marked `durable = true`, as setup registers Pi, holds the turns it
accepts and orders them itself, so it takes no `parallel_turns`: Pi answers
in every conversation at once. Changing `durable` needs `carillon restart`,
and a runtime refuses to start when its entry says otherwise than its
program does.

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

`/model` shows the models the runtime offers and lets an allowlisted user
change the conversation's model and settings, or the agent's default for new
conversations. Every channel routed to the agent shares that default. Outside
a conversation `/model` changes only the default. Its embed shows each
selection and where it comes from. Controls use the runtime's provider,
model and choice names, such as **Speed: Fast**, and name the scope they
change. Reset returns that scope to its default.

`/command` runs one of the runtime's own commands or skills in the
conversation, exactly as written:
`/command invocation:compact keep the decisions` sends `/compact keep the
decisions`. Plain `/command` lists them.

Both commands are admitted like messages: only in the guild, from allowlisted
users, in a routed channel or a thread under a thread route.

### Reactions

The bot reacts to each message it answers: while it works, then with how it
ended. While a message waits behind the agent's running turn, a runtime that
holds its turns may offer controls as reactions: an allowlisted person's
reaction hands the message to the running turn now (steer), ends the running
turn and runs it next (interrupt), or doesn't run it (stop). The emoji are
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
The picker uses these same model badges in its summaries. Custom emotes and
Unicode emoji also appear beside models and providers in its menus. Other
symbols stay in the summary text. A provider gets a menu badge when all its
models share one.

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

On T3 the assistant uses `carillon discord call` as the Discord skill
explains. Setup doesn't install the skill, because the tools can administer
the server; install it into the assistant's resources, which Carillon's T3
profiles share, so T3 Code channels never see it:

```sh
carillon discord tools install
```

Run it again after an update to refresh it.

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
  crashing it is given up after five tries and fails. T3 continues with the
  next message.
- **Core can't save an outcome** (disk full, for example). The turn stays
  marked running until core restarts, which records it as interrupted.
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

Maintenance commands take a lock per installation (`.maintenance.lock`) and
one on the owner's user units, and refuse while another holds them.

## Uninstall

```sh
~/.carillon/app/bin/carillon-uninstall
```

It removes the user services, the `~/.local/bin/carillon` link and the
application. Configuration, credentials, sessions and other data stay in the
installation directory; delete it yourself if you no longer need it.

T3 profile homes under `${XDG_DATA_HOME:-~/.local/share}/carillon/profiles`
also stay, with their sign-ins, sessions, and settings. T3 keeps the
`carillon_claude` and `carillon_codex` instances and their keys in its secret
store. Remove those separately in T3 settings and remove the profile homes
when no longer needed.
