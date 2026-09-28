# Carillon operations

## Installation model

The release installs four user units:

- `carillon.target`
- `carillon-core.service`
- `carillon-discord.service`
- `carillon-runtime-pi.service`

Run `install.sh` as the non-root Unix user that should own Carillon. The installer rejects root, creates no users or groups, changes no sudo policy, and does not enable lingering. It enables but does not start `carillon.target`.

`CARILLON_HOME` defaults to `~/.carillon`. It contains the registry (`carillon.toml`), provider-owned settings (`providers/`), credentials, agent resources, Pi-private state, SQLite state, logs, and installed application files (`app/bin` for the programs, `app/lib` for the release's Discord command module). An operator-installed command module is Discord provider state, not an application file. The configured workspace is separate and defaults to the owner's home.

The installer links `~/.local/bin/carillon` to `app/bin/carillon`, a path that stays valid across updates. It creates the link only where nothing exists or where a link into a removed installation's `app/bin` remains; it never replaces another program or the entry point of another existing installation, and says so. It does not edit shell profiles: when `~/.local/bin` is not on `PATH` or another `carillon` precedes it, the installer prints the full path to use instead.

Every `carillon` command acts on one installation, chosen in this order: the global `--home PATH` option, then `CARILLON_HOME`, then the installation whose managed `app/bin` contains the running program (following the entry-point link), otherwise `~/.carillon`. The current directory and its ancestors are never searched. `carillon <provider> …` passes the chosen installation to the provider as `CARILLON_HOME`. The services themselves are started with explicit `--config` and credential paths.

Several installations may belong to one owner, but the user units (`carillon.target` and its services) run one of them at a time. `start`, `stop`, `restart` and `update` refuse to act when the registered units run a different installation; stop that one and run the other installation's `app/bin/carillon-register-services` to switch deliberately.

## Authority

Every included service runs as the installing user with that user's groups, filesystem access, executables, applications, and passwordless sudo configuration. Carillon has no separate command or path permission layer.

## Setup

Installation requires:

- the immutable Discord guild ID;
- the bot user ID;
- at least one allowlisted user ID;
- at least one channel route with activation and conversation mode; and
- an existing absolute workspace path.

`carillon init` writes the registry and credentials; the installer then writes the Discord settings with `carillon discord configure`, which owns `providers/discord-private.toml`. Run `carillon discord configure --guild-id … --bot-user-id … --user-id … --route …` to change the guild, users, or routes; it rewrites the file completely, keeping any tuned feedback settings, and leaves it untouched when the input is invalid. Restart the Discord service afterwards.

Discord commands that act on a configured instance (`configure`, `check`, `commands`) use the only interface provider with Discord settings. When there is none or several, they list the choices and require `--provider ID`.

After installation:

1. write the Discord bot token to the credential path printed by the installer;
2. authenticate the packaged Pi installation using the printed `carillon-pi` command, then open `/model` and press Ctrl+S on the desired model to save its startup default;
3. run `carillon start`; and
4. inspect it with `carillon status` and `carillon logs`.

The bot's Discord application needs the `applications.commands` scope in the guild: the Discord provider registers its slash commands there on first start, and again whenever the application, guild, or command definitions differ from what it last recorded as registered, and refuses to serve if registration fails.

An administrator may run `loginctl enable-linger <owner>` when the services must start without an interactive login.

## Services

- **Core** (`carillon core`) owns accepted turns, their state, and delivery intents.
- **Discord** (`carillon discord serve`, the `carillon-discord` program) owns Discord admission, routing, threads, feedback, message delivery, and slash commands.
- **Pi runtime** (`carillon-runtime-pi`) owns Pi sessions, inference, native tools, extensions, skills, model selection, and private engine settings.

`carillon <provider> …` runs the `carillon-<provider>` program installed beside `carillon`, or else the one found on `PATH`; the subcommands after the provider name belong to that provider. A provider that is not part of the release is installed as `carillon-<provider>` on the owner's `PATH`, not under `app/`, which each update replaces wholesale; the user units carry the owner's `PATH` at registration.

Everyday operation:

```sh
carillon status            # core and provider health from the running core
carillon check             # this program accepts the registry and core database, read-only
carillon start             # start carillon.target and wait until providers answer
carillon stop
carillon restart
carillon logs [-f] [-n LINES] [core|discord|runtime-pi]
carillon discord commands  # the command module serving slash commands
carillon update /path/to/carillon-VERSION-linux-ARCH.tar.gz
```

`start`, `stop` and `restart` call `systemctl --user` on `carillon.target`; `logs` runs `tail` on `logs/`. `start` and `restart` report each provider's health; they fail when a provider is still unavailable after `--timeout` seconds (default 60). A version mismatch is reported but is not a failure. Restart an individual provider with:

```sh
systemctl --user restart carillon-discord.service
systemctl --user restart carillon-runtime-pi.service
```

Core is restarted separately. Registry and provider configuration are static until restart.

### Discord commands

`/model` shows the models the runtime offers and lets an allowlisted user change the conversation's selection or the agent default from Discord-native menus. It is admitted like a message: only in the configured guild, from allowlisted users, and in a routed channel or a thread under a thread route. Outside a conversation it changes only the agent default. A change the runtime refuses or does not confirm is shown with its reason above the settings as read again afterwards, with working controls; it is not retried.

The commands and their controls come from a WebAssembly command module. The release ships one at `app/lib/carillon_discord_commands.wasm`. An operator can install another in the running provider; it is kept at `<data_dir>/components/<provider>/command-module.wasm` (by default `state/components/discord-private/command-module.wasm`), outside the release's files, so it keeps serving after restarts and `carillon update`. At startup the provider serves the installed module when there is one, otherwise the release's module, and registers its commands with the guild unless its record shows the same definitions already registered for this application and guild.

```sh
carillon discord commands                 # what serves now
carillon discord commands check FILE      # can this program serve FILE? (offline)
carillon discord commands install FILE    # serve FILE now and after restarts
carillon discord commands reset           # serve the release's module again
```

`check` needs neither an installation nor a running provider; it applies exactly the checks installation and startup apply, including the 16 MiB (16,777,216-byte) size limit, and reports the module's commands or why it is unusable. `install` and `reset` are performed by the running provider, without restarting it or reconnecting its gateway:

1. It takes the installation's maintenance lock, refusing while an update or other maintenance holds it.
2. It validates the module (compilation, exports, its description, contract version and command definitions). An unusable module changes nothing.
3. When Discord's registration, as the provider last recorded it, differs from the module's definitions, it registers them first. It clears that record, durably, before contacting Discord. If Discord rejects the commands, nothing else changes; if Discord does not answer, it may have applied them, the previous module keeps serving, and the next install, reset or provider start registers again.
4. The module serves. Interactions already running finish on the module they started with.
5. It saves the change for later starts: `install` writes the installed module; `reset` removes it. Each change first syncs the provider's state directory and its ancestors, creating the directory when needed, so repeating a change also finishes a directory sync an earlier attempt could not.

A refusal says why, what to do next, and which module still serves. Success is one or two lines naming the source (`installed` or `release`) and the commands. A save has two failure modes, which are reported differently and exit with an error:

- **Not saved**: the module serves now, but the saved state on disk did not change. A restart selects whatever was saved before, which may already be this module; the failure alone does not establish what that state contains or whether it is usable.
- **Not confirmed durable**: the file changed, so a restart serves the new module, but syncing its directory failed and a system crash may still bring the previous state back.

Running the same command again finishes the change in either case, including registration, even when the requested module already serves. `carillon discord commands` also says when a restart would serve something other than what serves now.

`carillon update` keeps an installed module and has the new release check it before any service stops; an update whose provider cannot serve it is refused. Run `carillon discord commands reset` before such an update, then install a module built for the new release. A provider refuses to start with an installed module it cannot use and names the file; remove that file (the provider then serves the release's module), or replace it with a working module.

Each bundled provider reports its own build version, not the `providers[].version` expectation in configuration. Core's status reports the observed version with `version_mismatch` when the provider ID and role match but its version differs. Unreachable or wrong-identity providers are `unavailable`, with the configured identity and expected version instead.

### Activity cards

While a turn runs, Discord shows a temporary message in the conversation, two seconds after the message was accepted: whether it is queued or working, the model's latest public commentary, the calls running now with how long they have run, and the last few finished calls (`✓` or `✗` by default) with a count of earlier ones. Bash calls show the command's first line only, without heredoc or script bodies, substitutions or long quoted arguments; file tools show the path, searches the pattern, and Discord tools the action and IDs it targets. Tool output, file contents and private reasoning are never shown. The card is edited only when its text changes, at most every three seconds, and never notifies anyone. The typing indicator continues while the turn runs.

When the answer of a completed turn is delivered, the card is removed. An interrupted turn leaves the card saying it stopped before finishing, and a delivery that may not have arrived is stated on the card, including when both apply. When core is briefly unreachable the card keeps what it last showed.

Redaction of what cards show is best-effort. Known credential shapes (labelled values, `name=value` assignments, authorization headers, URL user information and queries, values after options such as `--password`, and long opaque tokens) are hidden, but a short secret passed as an ordinary argument can appear. Cards go only to the conversation that started the turn. Turn cards off with `activity = false` in `providers/<id>.toml` and restart the Discord service; `typing = false` turns off typing independently.

Configure card markers in that same provider file. Omitting the `[activity_markers]` table, or any field within it, uses these defaults:

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

`working`, `queued` and `finishing` prefix the card's current status. `running` prefixes running tools, while `succeeded` and `failed` mark recent tool results. `uncertain` marks a completed turn whose reply may not have arrived; `stopped` marks an interrupted turn, including when its notice may not have arrived.

Ordinary Unicode emoji and symbols such as `✓` and `↳` are supported. Custom Discord emotes use message syntax: `<:name:id>` for static emotes and `<a:name:id>` for animated emotes. Use an empty string to omit a marker and its separating space. For example, this partial table overrides three fields and leaves the others at their defaults:

```toml
[activity_markers]
working = "<a:working:123>"
running = "<:tool:456>"
succeeded = ""
```

Replace those sample emote names and IDs with emotes available to the bot. Custom names must contain 2–32 ASCII letters, digits or underscores, and IDs must be positive decimal Discord IDs without leading zeros. Validation checks syntax, not an emote's availability on Discord.

Each marker is limited to 64 UTF-8 bytes and must be empty, a complete recognized Unicode emoji sequence, a custom emote, or ordinary Unicode symbols/punctuation. Whitespace, control or invisible formatting characters, plain text, and Discord mention/Markdown syntax are rejected; joined emoji sequences remain supported. Invalid values report the specific `activity_markers.FIELD` to repair.

To keep all card text with no markers:

```toml
[activity_markers]
working = ""
queued = ""
finishing = ""
running = ""
succeeded = ""
failed = ""
uncertain = ""
stopped = ""
```

Reactions remain independent: the top-level `processing_emoji`, `delivered_emoji`, `failed_emoji` and `uncertain_emoji` fields still accept a Unicode emoji or reaction syntax such as `working:123`. Reaction syntax alone is not valid card-marker syntax. Set these top-level fields outside `[activity_markers]`; disabling reactions with `reactions = false` does not disable card markers. `carillon discord configure` preserves both tuned reactions and activity markers when rewriting admission settings.

After editing the file, validate it and use the existing restart workflow:

```sh
carillon discord check
systemctl --user restart carillon-discord.service
carillon status
```

Use `carillon discord check --provider ID` when several Discord providers are configured. Settings take effect at provider startup; invalid markers fail configuration validation before work starts. Card requests continue to disable all mentions, including with custom markers.

The provider remembers its cards in `<data_dir>/components/<provider>/activity-cards.json` until it removes or finalizes them. After a restart it deletes remembered cards before showing any replacement; if a card cannot be deleted, or its creation was never confirmed, that turn gets no new card. If the file cannot be read (for example it is malformed or not a readable file), the provider logs `activity cards are off until their records can be read`, starts without cards and leaves the file untouched; typing and delivery continue. Inspect it, delete any stale cards it names by hand, then remove or repair it and restart the Discord service. A card created just before a crash, before it was recorded, is not removed automatically. Progress messages share Discord's rate limits with answers: they are bounded and never block delivery in the provider, but Discord can make an answer wait for its share of a common limit.

### Assistant Discord tools

The optional [native Pi integration](../../packages/discord-pi/README.md) lets the
assistant read and manage Discord, independently of slash commands:

```sh
carillon discord tools install
```

It copies this release's extension and skill into the configured agent resources.
It replaces only `extensions/carillon-discord.js` and `skills/discord/SKILL.md`.
Newly accepted turns capture them; queued turns retain their old resources. It
needs no gateway restart. Run it again after updating the release to update the
optional integration, or remove those two files to stop capturing it. The regular
installer does not silently enable administration tools.

A fresh conversation sees `enable_discord`; detailed tools become model-visible
on demand. Pi retains activation in that conversation's native transcript.
The provider executes actions with its existing bot credential, bounded to its
configured guild and Discord permissions/hierarchy. Routes determine which
messages start turns, not which channels the assistant can administer. These
native tools have the installing Unix user's authority; lazy activation is not
an authorization boundary. Explicit sends differ from ordinary final replies.
A write with an uncertain outcome must be inspected before being repeated.

## Failure behavior

A turn is queued before acceptance is acknowledged. Runtime work is executed only after the turn is durably claimed as running.

If Carillon loses a running turn, or the runtime reports that it failed, it marks it interrupted and does not replay it. When the runtime can explain the failure safely, such as a model provider's usage limit, the interruption notice includes that explanation. Otherwise the notice is generic, and the runtime's log (`carillon logs runtime-pi`) holds the technical diagnostic. `carillon inspect TURN_ID` shows either the reported explanation or core's own diagnostic. The runtime retains its saved session. Send another message to continue the conversation normally; separately queued messages continue in order. There is no retry/skip gate. Tool actions may already have happened, and interruption does not undo them. Discord uses warning feedback for interrupted turns, even when the interruption notice itself was delivered successfully.

Pi cancels execution when core disconnects and settles the native operation before admitting another turn. If the runtime process dies, its user service restarts it. A successful runtime result and delivery intent are committed together; delivering a committed answer never requires rerunning the model.

If core cannot persist an outcome, it logs the storage error and leaves the turn recorded as running. Repair storage and restart core; startup records the stranded turn as interrupted without replaying it. Diagnose any turn with:

```sh
carillon inspect TURN_ID
```

Discord delivery uses a separate outbox. An unclaimed delivery can be retried. A delivery that was claimed when the interface stopped is marked uncertain because the external send may already have succeeded.

## Credentials

Platform credentials belong to interface providers. Engine credentials and settings belong to the runtime provider. Internal loopback tokens authenticate provider-to-core traffic and interface-to-provider simulation; the operator credential authorizes diagnostics, command module changes, and assistant Discord operations. Provider IDs `operator` and `discord-bot` are reserved because those credential filenames already belong to the installation. Credentials stay outside prompts, session history, diagnostics, ordinary logs, and the command module.

The packaged Pi state uses `CARILLON_HOME/pi`, not the owner's global `~/.pi/agent`. Model selections made through `/model` are kept there in `model-selection.json`; Pi's own default model remains the runtime default.

## Backup and restore

Stop `carillon.target` before backup or restore. Carillon refuses backup while its service listeners or core lock are active.

The backup contains the entire `CARILLON_HOME` tree, including the registry, provider settings, credentials, SQLite state, common agent resources, runtime resource snapshots and sessions, release identity, and Pi-private authentication, model selection, and model state under `CARILLON_HOME/pi`. The archive therefore contains credentials and private agent state. Keep it owner-readable only and encrypt it before storing or transferring it.

Backup requires `data_dir` and `agent.resource_root` to be installation-relative paths without `..`, resolving inside `CARILLON_HOME`. Absolute paths are rejected even when they point inside the source installation: use `state` and `agents/default`, for example, so a restored configuration cannot refer back to the source. The destination must resolve outside that tree. An external user workspace is allowed, but is not included in this backup.

Before creating a backup or publishing a restore, Carillon checks the configured resource directory, matching operator/provider loopback credentials, the core database's integrity, foreign keys and current schema, and every recorded resource snapshot against its content address, which covers each captured file's path, bytes, and executability. Missing or altered captured resources are rejected, not reconstructed from the live resource root. This archive integrity check is not ongoing tamper protection against the owning Unix user. Restore accepts only regular files and directories and validates in a temporary directory. Validation or prepublication failure leaves the target absent or empty and retryable. A persistence error after publication can leave the complete restored tree at the target; inspect it before retrying rather than assuming publication was undone. This private beta does not migrate obsolete schemas.

This is not a provider-native health check: individual resource files are optional, and providers own their native sessions, settings, and authentication requirements. The entire installation tree is copied, but restore cannot infer whether arbitrary provider-private files were omitted from a manually edited archive. Check the provider after restoring; a successful restore does not attest that external credentials are still valid. Use a backup made by the same current state format, or perform a fresh installation.

Restore publishes the installation tree; it does not register services or execute archived programs. User units live outside `CARILLON_HOME` and must be registered separately before starting a restored bundled release, including a same-user relocation. As the intended non-root owner, with that owner's `HOME`, `PATH`, and optional `XDG_CONFIG_HOME`:

```sh
carillon restore --input /path/to/private-backup.tar.gz --target /new/carillon
/new/carillon/app/bin/carillon-register-services
/new/carillon/app/bin/carillon start
```

The installed registration helper needs no source checkout or release installer. It derives the root from its own installed location, renders the bundled templates with the current owner's environment, replaces Carillon's user units, reloads the user service manager, and enables—but does not start—`carillon.target`. It leaves configuration, credentials and provider state untouched. Stop the old target before relocation; check the configured external workspace and provider health before restarting. Non-bundled service supervisors/providers retain their own registration procedure.

## Update and uninstall

Release archive and staging names use the Cargo workspace version. `scripts/package-release.sh` accepts an optional version assertion and output path; a version differing from the workspace manifest is refused before building or staging. Packaging does not relabel binaries: core and the bundled Discord provider use that workspace identity, while the separately versioned Pi adapter retains its own package identity.

The release builder selects the native Rust host explicitly for Cargo and the corresponding Bun executable target, regardless of Cargo environment/config target overrides. Linux GNU x86_64 and aarch64 hosts produce architecture-labelled archives; other build hosts are refused before staging. The Discord command module is built for `wasm32-unknown-unknown` on every host and shipped as data under `lib/`.

The packager writes `MANIFEST` last. It lists every shipped file, marked `x` when it must be executable and `r` otherwise. Before changing anything, setup and update refuse a release whose files are missing, not executable where required, unlisted, links or special files. This detects incomplete or altered archives; it is not a signature. Whoever can change the archive can also change its manifest.

`install.sh` is first-time setup. Rerunning it retries a failed setup; it keeps the registry, credentials and state but rewrites the Discord settings from its arguments. Update the software with the release archive instead:

```sh
carillon update /path/to/carillon-VERSION-linux-ARCH.tar.gz
```

Update leaves configuration alone. It never runs Discord configuration or `init`, and keeps the registry, provider settings, credentials, agent resources, sessions, SQLite state, Pi state and model selections, logs, and providers installed outside `app/`. It proceeds in this order:

1. It checks that the chosen installation belongs to the invoking non-root user, that `app` carries the ownership marker, and that the user units are this installation's or absent. It takes the maintenance lock and records whether `carillon.target` is running.
2. It unpacks the archive into a private `.update-*` directory inside the installation, accepting only regular files and directories without absolute names or `..`. The archive must hold exactly one `carillon-VERSION` directory. Its `install.sh` and `bin/carillon` must be executable, and `bin/carillon --version` must run here and report that version.
3. The new release checks itself with `install.sh --update --check`, which changes nothing:
   - its payload matches its `MANIFEST`;
   - `mv --exchange` works in the installation directory;
   - its `carillon check` opens the registry and core database exactly as its core service would, requiring the core schema it was built for;
   - its `carillon discord check` loads the Discord settings and checks that it can serve an installed command module.
4. Only then are running services stopped. The release's own `install.sh --update` uses the same application exchange and service registration as setup, and installs the entry point when it is missing.
5. Services that were running are started again and must answer within `--timeout` seconds. Stopped services stay stopped.

A refusal in steps 1–3 changes nothing and leaves the services as they were. The staging directory is removed afterwards. Later failures state what may have changed:

- **The installer cannot be started:** the application was not replaced. The services are stopped; `carillon start` resumes the current release.
- **During publication or registration:** the services stay stopped. `app` may hold either complete application and the units may be mixed. Rerun the same `carillon update` until it succeeds, then `carillon start` if the services should run.
- **When starting or health checks fail:** the new release stays installed and the previous application is not kept. Inspect `carillon logs` and `carillon status`.

There is no automatic rollback, migration step or download service. The state check covers only what it names:
- It covers the registry, the core database's integrity and exact schema, the Discord settings, and an installed command module.
- It does not cover Pi's state (sessions, authentication, model selections), other provider-native state, credentials, or whether the services will start and stay healthy.

`carillon update` with an older release runs the same checks, so it refuses a core schema that release does not know. Passing them does not make a downgrade safe for the unchecked state. There is no supported downgrade; restore a backup made before the update instead.

When intentionally changing a provider release, update that provider's expected `version` in `carillon.toml`, restart, and check `carillon status`. Neither setup nor update rewrites provider expectations: an entry may refer to a replacement provider rather than the bundled implementation. Discord's build version is the workspace version; the Pi adapter's is `@carillon/runtime-pi`'s package version, not the underlying Pi engine version. A version mismatch is a diagnostic, not a protocol-compatibility or execution-safety guarantee.

Maintenance is serialized per installation by an exclusive lock on `CARILLON_HOME/.maintenance.lock`. `update`, `start`, `stop`, `restart`, `backup`, setup reruns and uninstallation take it and refuse at once while another holds it. The running Discord provider takes the same lock for each command module install or reset, so neither overlaps an update's checks and service stop. Direct `systemctl` calls bypass it. Application replacement uses GNU `mv --exchange --no-copy` (coreutils 9.5 or newer) on a filesystem supporting atomic rename exchange. GNU `sync -f` persists the staging filesystem before selection and the containing filesystem after publication; these filesystem-wide barriers may wait for unrelated writes. An interrupted replacement leaves either the complete previous application or the complete new one at `app`, never a missing or partly copied application. Unsupported exchange fails without deleting the previous application.

Service registration stages all definitions and modes before replacing live units, then publishes each as a complete file. It separately syncs the user-unit filesystem before publication, afterward, and after enabling the target, including when `XDG_CONFIG_HOME` is on another filesystem. Application files, unit definitions, and systemd are not one transaction: a failed update may leave the new application selected with a mixture of complete old and new unit definitions. **After failed or interrupted maintenance, rerun it successfully before restarting services**: `carillon update` with the same archive, the installer for a failed first setup, or `carillon-register-services` after standalone registration failure.

Failed publication or registration retains private recovery copies at the paths printed by the scripts. Abrupt process death may also leave `.update-*` or `.app-new.*` directories under `CARILLON_HOME`, or `.carillon-units.*` directories in the user-unit directory. These copies are not selected by services, and later retries do not adopt or delete them. After a successful rerun and inspection, the owner can remove the leftover copies. A sync error does not undo a rename. On a failed first installation, configuration and any complete published application remain for retry; there is no previous release to restore.

The installed `app/bin/carillon-uninstall` derives the installation root from its canonical location, not an inherited `CARILLON_HOME`. Both uninstall entry points require absolute `HOME` and, when set, `XDG_CONFIG_HOME` before service-manager or filesystem changes. It removes Carillon's user units, the `~/.local/bin/carillon` link when it points to this installation, and managed application files. Configuration, credentials, sessions and other data in that installation are kept. When the registered units run another installation, it leaves them and that installation's services alone. It does not remove unrelated owner files or the owner's global Pi directory. Application cleanup keeps its ownership marker until the other entries have been removed and synced. If interrupted removal has deleted the installed helper, retry the release archive's standalone helper as the same owner:

```sh
CARILLON_HOME=/path/to/installation /path/to/release/uninstall.sh
```

The standalone helper accepts only a marked application for recursive deletion. An empty or already absent `app` can finish cleanup without removing any service definitions; a nonempty unmarked tree is refused. Keep the release archive available until removal succeeds.
