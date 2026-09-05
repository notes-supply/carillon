# Carillon operations

## Installation model

The release installs four user units:

- `carillon.target`
- `carillon-core.service`
- `carillon-discord.service`
- `carillon-runtime-pi.service`

Run `install.sh` as the non-root Unix user that should own Carillon. The installer rejects root, creates no users or groups, changes no sudo policy, and does not enable lingering. It enables but does not start `carillon.target`.

`CARILLON_HOME` defaults to `~/.carillon`. It contains configuration, credentials, agent resources, Pi-private state, SQLite state, logs, and installed application files. The configured workspace is separate and defaults to the owner's home.

## Authority

Every included service runs as the installing user with that user's groups, filesystem access, executables, applications, and passwordless sudo configuration. Carillon has no separate command or path permission layer.

## Setup

Installation requires:

- the immutable Discord guild ID;
- the bot user ID;
- at least one allowlisted user ID;
- at least one channel route with activation and conversation mode; and
- an existing absolute workspace path.

After installation:

1. write the Discord bot token to the credential path printed by the installer;
2. authenticate the packaged Pi installation using the printed `carillon-pi` command;
3. start `carillon.target`; and
4. inspect it with `carillon status` and `systemctl --user status carillon.target`.

An administrator may run `loginctl enable-linger <owner>` when the services must start without an interactive login.

## Services

- **Core** owns accepted events, turn state, continuations, and delivery intents.
- **Discord** owns Discord admission, routing, threads, feedback, and message delivery.
- **Pi runtime** owns Pi sessions, inference, native tools, extensions, skills, and private engine settings.

Restart an individual provider with:

```sh
systemctl --user restart carillon-discord.service
systemctl --user restart carillon-runtime-pi.service
```

Core is restarted separately. Provider configuration is static until restart.

Each bundled provider reports its own build version, not the `providers[].version` expectation in configuration. Core's status reports the observed version with `version_mismatch` when the provider ID and role match but its version differs. Unreachable or wrong-identity providers are `unavailable`, with the configured identity and expected version instead.

## Failure behavior

A turn is queued before acceptance is acknowledged. Runtime work is executed only after the turn is durably claimed as running.

If Carillon cannot prove what happened to running model work, the turn is marked uncertain and is not replayed automatically. Later work for that same conversation waits; unrelated conversations continue. A successful runtime result and delivery intent are committed together.

Inspect and resolve an uncertain turn with:

```sh
carillon inspect --config ~/.carillon/carillon.toml TURN_ID
carillon resolve --config ~/.carillon/carillon.toml TURN_ID retry
carillon resolve --config ~/.carillon/carillon.toml TURN_ID skip
```

`retry` queues the turn again and may repeat work whose outcome could not be observed. `skip` records that the turn was deliberately abandoned and unblocks later work without rerunning it.

Discord delivery uses a separate outbox. An unclaimed delivery can be retried. A delivery that was claimed when the interface stopped is marked uncertain because the external send may already have succeeded.

## Credentials

Platform credentials belong to interface providers. Engine credentials and settings belong to the runtime provider. Internal loopback tokens authenticate provider-to-core traffic. Credentials stay outside prompts, session history, diagnostics, and ordinary logs.

The packaged Pi state uses `CARILLON_HOME/pi`, not the owner's global `~/.pi/agent`.

## Backup and restore

Stop `carillon.target` before backup or restore. Carillon refuses backup while its service listeners or core lock are active.

The backup contains the entire `CARILLON_HOME` tree, including configuration, credentials, SQLite state, common agent resources, runtime resource snapshots and sessions, release identity, and Pi-private authentication and model state under `CARILLON_HOME/pi`. The archive therefore contains credentials and private agent state. Keep it owner-readable only and encrypt it before storing or transferring it.

Backup requires `data_dir` and `agent.resource_root` to be installation-relative paths without `..`, resolving inside `CARILLON_HOME`. Absolute paths are rejected even when they point inside the source installation: use `state` and `agents/default`, for example, so a restored configuration cannot refer back to the source. The destination must resolve outside that tree. An external user workspace is allowed, but is not included in this backup.

Before creating a backup or publishing a restore, Carillon checks the configured resource directory, matching operator/provider loopback credentials, and the core database's integrity, foreign keys, and current schema. Restore accepts only regular files and directories and validates in a temporary directory; failure leaves the target absent or empty and retryable. This private beta does not migrate obsolete schemas.

This is not a provider-native health check: individual resource files are optional, and providers own their native sessions, settings, and authentication requirements. The entire installation tree is copied, but restore cannot infer whether arbitrary provider-private files were omitted from a manually edited archive. Check the provider after restoring; a successful restore does not attest that external credentials are still valid. Use a backup made by the same current state format, or perform a fresh installation.

Restore publishes the installation tree; it does not register services or execute archived programs. User units live outside `CARILLON_HOME` and must be registered separately before starting a restored bundled release, including a same-user relocation. As the intended non-root owner, with that owner's `HOME`, `PATH`, and optional `XDG_CONFIG_HOME`:

```sh
carillon restore --input /path/to/private-backup.tar.gz --target /new/carillon
/new/carillon/app/bin/carillon-register-services
systemctl --user start carillon.target
```

The installed registration helper needs no source checkout or release installer. It derives the root from its own installed location, renders the bundled templates with the current owner's environment, replaces Carillon's user units, reloads the user service manager, and enables—but does not start—`carillon.target`. It leaves configuration, credentials and provider state untouched. Stop the old target before relocation; check the configured external workspace and provider health before restarting. Non-bundled service supervisors/providers retain their own registration procedure.

## Update and uninstall

Re-running the installer replaces managed application files and user units while preserving current configuration, credentials, sessions, and SQLite state. It is suitable only when the installed state format is still current.

Stop the target before updating. When intentionally changing a provider release, update that provider's expected `version` in `carillon.toml`, restart, and check `carillon status`. The installer does not rewrite provider expectations: an entry may refer to a replacement provider rather than the bundled implementation. Discord's build version is the Carillon crate version; the Pi adapter's is `@carillon/runtime-pi`'s package version, not the underlying Pi engine version. A version mismatch is a diagnostic, not a protocol-compatibility or execution-safety guarantee.

Application replacement uses GNU `mv --exchange --no-copy` (coreutils 9.5 or newer) on a filesystem supporting atomic rename exchange. GNU `sync -f` persists the staging filesystem's copied files, modes and directories before selection, then persists the containing filesystem after publication (also after application rollback). These filesystem-wide barriers may wait for unrelated writes on the same filesystem. An interrupted replacement leaves either the complete previous application or the complete new one at `app`, never a missing or partly copied application. Unsupported exchange fails without deleting the previous application. Until the publication barrier succeeds, failures retain the other tree rather than discarding a potential recovery copy; a sync error does not mean the rename was rolled back. Abrupt process death may leave a private `.app-new.*` staging directory; it is not selected by services.

If service rendering or publication fails and the registration helper confirms that previous unit definitions are intact, the installer exchanges the previous application back too. After unit publication, a service-manager failure does not roll back the new application against the new units. An unconfirmed registration or rollback failure retains the other application tree and prints its path for inspection. These filesystem steps are not one transaction with systemd: after any interrupted update, rerun the installer successfully before restarting services. On a failed first installation there is no previous application to restore; configuration and the complete new application remain for retry.

The installed uninstaller removes Carillon's user units and managed application files while preserving data under `CARILLON_HOME`. It does not remove unrelated owner files or the owner's global Pi directory.
