# Carillon operations

## Installation model

The release installs four user units:

- `carillon.target`
- `carillon-core.service`
- `carillon-discord.service`
- `carillon-runtime-pi.service`

Run `install.sh` as the non-root Unix user that should own Carillon. The installer rejects root, creates no users or groups, changes no sudo policy, and does not enable lingering. It enables but does not start `carillon.target`.

`CARILLON_HOME` defaults to `~/.carillon`. It contains the registry (`carillon.toml`), provider-owned settings (`providers/`), credentials, agent resources, Pi-private state, SQLite state, logs, and installed application files (`app/bin` for the programs, `app/lib` for the Discord command module). The configured workspace is separate and defaults to the owner's home.

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

Discord commands that act on a configured instance (`configure`, `register`, `reload`) use the only interface provider with Discord settings. When there is none or several, they list the choices and require `--provider ID`.

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
carillon start             # start carillon.target and wait until providers answer
carillon stop
carillon restart
carillon logs [-f] [-n LINES] [core|discord|runtime-pi]
carillon discord reload    # replace the running command module
carillon update /path/to/carillon-VERSION-linux-ARCH.tar.gz
```

These call `systemctl --user` on `carillon.target` and `tail` on `logs/`. `start` and `restart` report each provider's health; they fail when a provider is still unavailable after `--timeout` seconds (default 60). A version mismatch is reported but is not a failure. Restart an individual provider with:

```sh
systemctl --user restart carillon-discord.service
systemctl --user restart carillon-runtime-pi.service
```

Core is restarted separately. Registry and provider configuration are static until restart.

### Discord commands

`/model` shows the models the runtime offers and lets an allowlisted user change the conversation's selection or the agent default from Discord-native menus. It is admitted like a message: only in the configured guild, from allowlisted users, and in a routed channel or a thread under a thread route. Outside a conversation it changes only the agent default.

The commands and their controls come from `app/lib/carillon_discord_commands.wasm`, which the provider loads at startup and registers with the guild unless its record shows the same definitions already registered for this application and guild. To replace the module without restarting the provider or reconnecting its gateway, put the new file at that path and run:

```sh
carillon discord reload
```

The reply reports the number of commands, whether their definitions changed, and whether they were re-registered with Discord. A module that fails to load or describe itself is refused and the previous one keeps serving. When the definitions changed, the new module is registered before it serves: if Discord rejects the commands, the reply says so and the previous module keeps serving, so fix the module and rerun `reload`; if Discord does not answer, whether it applied them is unknown, the previous module keeps serving, and rerunning `reload` once Discord answers settles it. Interactions already running finish against the module they started with. `carillon discord register` registers the module's commands using the bot token without serving, for example after changing the guild.

Each bundled provider reports its own build version, not the `providers[].version` expectation in configuration. Core's status reports the observed version with `version_mismatch` when the provider ID and role match but its version differs. Unreachable or wrong-identity providers are `unavailable`, with the configured identity and expected version instead.

## Failure behavior

A turn is queued before acceptance is acknowledged. Runtime work is executed only after the turn is durably claimed as running.

If Carillon loses a running turn, it marks it interrupted and does not replay it. The runtime retains its saved session. Send another message to continue the conversation normally; separately queued messages continue in order. There is no retry/skip gate. Tool actions may already have happened, and interruption does not undo them. Discord uses warning feedback for interrupted turns, even when the interruption notice itself was delivered successfully.

Pi cancels execution when core disconnects and settles the native operation before admitting another turn. If the runtime process dies, its user service restarts it. A successful runtime result and delivery intent are committed together; delivering a committed answer never requires rerunning the model.

If core cannot persist an outcome, it logs the storage error and leaves the turn recorded as running. Repair storage and restart core; startup records the stranded turn as interrupted without replaying it. Diagnose any turn with:

```sh
carillon inspect TURN_ID
```

Discord delivery uses a separate outbox. An unclaimed delivery can be retried. A delivery that was claimed when the interface stopped is marked uncertain because the external send may already have succeeded.

## Credentials

Platform credentials belong to interface providers. Engine credentials and settings belong to the runtime provider. Internal loopback tokens authenticate provider-to-core traffic and interface-to-provider simulation; the operator credential authorizes diagnostics and command reloads. Provider IDs `operator` and `discord-bot` are reserved because those credential filenames already belong to the installation. Credentials stay outside prompts, session history, diagnostics, ordinary logs, and the command module.

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

`install.sh` is first-time setup. Rerunning it retries a failed setup; it keeps the registry, credentials and state but rewrites the Discord settings from its arguments. Update the software with the release archive instead:

```sh
carillon update /path/to/carillon-VERSION-linux-ARCH.tar.gz
```

Update leaves configuration alone. It never runs Discord configuration or `init`, and keeps the registry, provider settings, credentials, agent resources, sessions, SQLite state, Pi state and model selections, logs, and providers installed outside `app/`. It proceeds in this order:

1. It checks that the chosen installation belongs to the invoking non-root user, that `app` carries the ownership marker, and that the user units are this installation's or absent. It takes the maintenance lock and records whether `carillon.target` is running.
2. It unpacks the archive into a private `.update-*` directory inside the installation, accepting only regular files and directories without absolute names or `..`. The archive must hold exactly one `carillon-VERSION` directory with the published files, and its `bin/carillon --version` must run here and report that version. A refused archive changes nothing and leaves the services as they were.
3. Only then are running services stopped. The release's own `install.sh --update` uses the same application exchange and service registration as setup, and installs the entry point when it is missing.
4. Services that were running are started again and must answer within `--timeout` seconds. Stopped services stay stopped.

The staging directory is removed afterwards. Failures state what may have changed:

- **Before step 3:** nothing was replaced.
- **During publication or registration:** the services stay stopped. `app` may hold either complete application and the units may be mixed. Rerun the same `carillon update` until it succeeds, then `carillon start` if the services were running.
- **When starting or health checks fail:** the new release stays installed. Inspect `carillon logs` and `carillon status`. To go back, run `carillon update` with the previous release's archive.

There is no automatic rollback, migration step or download service. Update is suitable only when the installed state format is still current.

When intentionally changing a provider release, update that provider's expected `version` in `carillon.toml`, restart, and check `carillon status`. Neither setup nor update rewrites provider expectations: an entry may refer to a replacement provider rather than the bundled implementation. Discord's build version is the workspace version; the Pi adapter's is `@carillon/runtime-pi`'s package version, not the underlying Pi engine version. A version mismatch is a diagnostic, not a protocol-compatibility or execution-safety guarantee.

Maintenance is serialized per installation by an exclusive lock on `CARILLON_HOME/.maintenance.lock`. `update`, `start`, `stop`, `restart`, `backup`, setup reruns and uninstallation take it and refuse at once while another holds it. Direct `systemctl` calls bypass it. Application replacement uses GNU `mv --exchange --no-copy` (coreutils 9.5 or newer) on a filesystem supporting atomic rename exchange. GNU `sync -f` persists the staging filesystem before selection and the containing filesystem after publication; these filesystem-wide barriers may wait for unrelated writes. An interrupted replacement leaves either the complete previous application or the complete new one at `app`, never a missing or partly copied application. Unsupported exchange fails without deleting the previous application.

Service registration stages all definitions and modes before replacing live units, then publishes each as a complete file. It separately syncs the user-unit filesystem before publication, afterward, and after enabling the target, including when `XDG_CONFIG_HOME` is on another filesystem. Application files, unit definitions, and systemd are not one transaction: a failed update may leave the new application selected with a mixture of complete old and new unit definitions. **After failed or interrupted maintenance, rerun it successfully before restarting services**: `carillon update` with the same archive, the installer for a failed first setup, or `carillon-register-services` after standalone registration failure.

Failed publication or registration retains private recovery copies at the paths printed by the scripts. Abrupt process death may also leave `.update-*` or `.app-new.*` directories under `CARILLON_HOME`, or `.carillon-units.*` directories in the user-unit directory. These copies are not selected by services, and later retries do not adopt or delete them. After a successful rerun and inspection, the owner can remove the leftover copies. A sync error does not undo a rename. On a failed first installation, configuration and any complete published application remain for retry; there is no previous release to restore.

The installed `app/bin/carillon-uninstall` derives the installation root from its canonical location, not an inherited `CARILLON_HOME`. Both uninstall entry points require absolute `HOME` and, when set, `XDG_CONFIG_HOME` before service-manager or filesystem changes. It removes Carillon's user units, the `~/.local/bin/carillon` link when it points to this installation, and managed application files. Configuration, credentials, sessions and other data in that installation are kept. When the registered units run another installation, it leaves them and that installation's services alone. It does not remove unrelated owner files or the owner's global Pi directory. Application cleanup keeps its ownership marker until the other entries have been removed and synced. If interrupted removal has deleted the installed helper, retry the release archive's standalone helper as the same owner:

```sh
CARILLON_HOME=/path/to/installation /path/to/release/uninstall.sh
```

The standalone helper accepts only a marked application for recursive deletion. An empty or already absent `app` can finish cleanup without removing any service definitions; a nonempty unmarked tree is refused. Keep the release archive available until removal succeeds.
