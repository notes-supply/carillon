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

Platform credentials belong to interface providers. Engine credentials and selected model state belong to the runtime provider. Internal loopback tokens authenticate provider-to-core traffic. Credentials stay outside prompts, session history, diagnostics, and ordinary logs.

The packaged Pi state uses `CARILLON_HOME/pi`, not the owner's global `~/.pi/agent`.

## Backup and restore

Stop `carillon.target` before backup or restore. Carillon refuses backup while its service listeners or core lock are active.

The backup contains the current Carillon configuration, SQLite state, common agent resources, runtime resource snapshots and sessions, and release identity. Pi-private authentication and model state under `CARILLON_HOME/pi` is not included; copy that private directory separately for a complete host move.

Restore validates the archive and replaces the managed Carillon state. This private beta does not migrate obsolete schemas. Use a backup made by the same current state format, or perform a fresh installation.

## Update and uninstall

Re-running the installer replaces managed application files and user units while preserving current configuration, credentials, sessions, and SQLite state. It is suitable only when the installed state format is still current.

The installed uninstaller removes Carillon's user units and managed application files while preserving data under `CARILLON_HOME`. It does not remove unrelated owner files or the owner's global Pi directory.
