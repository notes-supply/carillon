# Carillon operations

## Supported environment

The first release is a self-contained composition service for one supported Linux host. It includes the Carillon core, initial interface and runtime adapters, example extensions and capabilities, user-service definitions, migrations, and the files needed to create an agent. Running it does not require root, a source checkout, or a development toolchain.

Core, interface adapters, runtime adapters, interface extensions, and capabilities run as separately supervised user services under the installing user's single Unix identity. Native UID and primary/supplementary groups remain inherent to the process; HOME, USER, LOGNAME, shell, and PATH are deterministic unit inputs; ordinary user-session variables remain inherited. Filesystem authority, applications, and passwordless sudo remain available. Carillon has no secondary command, path, capability, or extension authorization framework.

## Setup and everyday operation

Setup runs as the intended non-root owner and rejects root rather than inferring ownership from `SUDO_USER`. `CARILLON_HOME` defaults to that user's `~/.carillon`. It contains private configuration, credentials, agents, Pi state, sessions, durable runtime state, logs, and application files. The user-systemd unit directory contains only generated service definitions owned by the same user. Installation enables but does not start `carillon.target`; it never creates users/groups, changes sudo policy, enables lingering, or mutates system units.

Setup requires immutable Discord guild and bot user IDs, at least one allowlisted immutable operator ID, and at least one immutable channel route with explicit activation and conversation modes. Until all are valid, Discord admission remains closed. Installation remains offline; the Discord token is supplied afterward.

The generated configuration explicitly enables typing and reactions and supplies processing, delivered, failed, and uncertain Unicode emoji. Operators may switch typing and reactions independently or use validated canonical Discord custom `name:id` emoji.

The default workspace is the owner's home and may be set explicitly at installation. It is distinct from immutable Carillon resource snapshots. Pi's native tools and project-local discovery run in that real workspace with ordinary absolute-path behavior and owner authority.

The operator can see the health and selected identity of the core, adapters, extensions, capabilities, agents, native continuations, queued work, storage, and latest backup. Turn inspection connects the external event, participating components and resources, runtime-native evidence, capability activity, and delivery result without unnecessarily exposing prompts or credentials.

## Live extension and restart

An operator can make a new or changed interface adapter, runtime adapter, interface extension, or capability available; select it for an agent or route; and restart or replace the affected component without stopping unrelated parts. Agent-file and runtime-native extension changes likewise become available to later turns without rebuilding Carillon.

Carillon reports whether active work continued, completed, or was interrupted rather than silently moving it across incompatible code or resources. Component lifecycle is observable and deliberate, but the implementation is free to choose the simplest activation and supervision mechanics that provide that behavior.

Interface extensions can add commands or interactions without receiving the platform token. Runtime adapters expose their actual supported features and native resource/session identities. Switching runtime adapters does not silently translate incompatible native state.

## Credentials

Credentials are scoped to their consumers: platform credentials to interface adapters, provider credentials to the Pi runtime, and internal loopback credentials to the relevant component. They remain outside normal session history, diagnostics, and logs. The packaged `carillon-pi` helper authenticates against `CARILLON_HOME/pi`; neither login nor the embedded runtime uses the owner's global `~/.pi/agent`.

## Backup and restore

A backup captures a consistent view of Carillon's database and configuration, common agent files, runtime-owned continuation resources and sessions, selected component identities and configuration, component-owned durable evidence, and the release information needed to interpret them. Pi-private provider credentials, discovered catalogs, selected models, and service-tier defaults live under `CARILLON_HOME/pi` outside the normal backup archive; copy that private directory separately for a complete host move.

Restore reconstructs a coherent installation and reconciles core and component state before accepting new work. The operator can continue from the next safe action without repeating completed runtime work or an external delivery whose outcome is uncertain.

## Upgrades and uninstall

Components can evolve independently when their public Carillon boundary remains compatible; coordinated upgrades remain possible when a boundary itself changes. Native runtime formats remain owned by their runtime adapters and use their supported compatibility behavior rather than being rewritten into one Carillon session format.

Re-running the installer updates application files and Discord admission without replacing configuration, credentials, sessions, or durable state. The installed uninstaller stops and removes Carillon's user units and application files while preserving `CARILLON_HOME` data. It never removes unrelated owner files or the owner's global `~/.pi`.

## One-time root-owned installation cutover

This is an administrator-operated migration, not a compatibility mode. Substitute a concrete account for `OWNER` and use that account's actual home for `NEW_HOME` (normally `/home/OWNER/.carillon`).

1. Record the old unit state and ports. Stop all old system `carillon-*` units. Do not start the user services while any old unit remains active.
2. Create two read-only rollback archives: one of `/var/lib/carillon` and one of `/var/lib/pi-carillon`. Separately save `/etc/carillon/discord.token`. Keep `/opt/carillon`, the system units, and the original directories unchanged until acceptance.
3. As an administrator, copy `/var/lib/carillon` into `NEW_HOME`, preserving its `carillon.toml`, `state/` (including SQLite and component evidence), `credentials/`, `agents/` (including resource files and sessions), and examples. Copy all of `/var/lib/pi-carillon` into `NEW_HOME/pi` to preserve Pi authentication, model state, and `service-tier.json`. Copy the old Discord token to `NEW_HOME/credentials/discord-bot.token`.
4. In `NEW_HOME/carillon.toml`, add `workspace = \"/home/OWNER\"` to `[agent]` (or another intentional real workspace). Keep existing relative paths relative; they now resolve from `NEW_HOME`.
5. Recursively change the copied tree to `OWNER` and that account's primary group; set `NEW_HOME` and state directories to `0700` and token/config files to `0600`. This ownership change is the explicit administrator boundary.
6. As `OWNER` in a real login/session—not through a root installer—set `CARILLON_HOME=NEW_HOME` and run the new release installer with the existing Discord admission IDs/routes. It installs only the user application and user units; it must not infer `OWNER`.
7. Verify `carillon-pi` sees the copied authentication, `carillon model` reports the intended model and Standard/Fast default, `systemctl --user start carillon.target` becomes healthy, existing sessions continue, and one authorized Discord turn succeeds. Also verify an unauthorized event still fails before runtime and the owner's `~/.pi` remains unchanged.
8. Rollback: stop `carillon.target`, leave `NEW_HOME` intact for diagnosis, and restart the untouched old system units against the untouched old directories. Never run both deployments concurrently. After an explicit retention period and backup verification, an administrator may remove the displaced system units, pseudo-users, `/opt/carillon`, and old state.

The exact transport, schemas, queues, and module layout remain engineering choices. The operational contract is a rootless owner installation that preserves native user authority, remote admission, continuity, recoverability, and data-safe rollback.
