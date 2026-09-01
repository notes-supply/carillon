# Carillon operations

## Supported environment

The first release is a self-contained composition service for one supported Linux host. It includes the Carillon core, Discord interface adapter, Pi runtime adapter, their three user services, and the files needed to create an agent. Running it does not require root, a source checkout, or a development toolchain.

Core, interface adapters, runtime adapters, and any explicitly installed interface extensions or capabilities run as separately supervised user services under the installing user's single Unix identity. Optional providers require explicit configuration, credentials, implementation packaging, and supervision; the default release does not install examples. Native UID and primary/supplementary groups remain inherent to each process; HOME, USER, LOGNAME, shell, and PATH are deterministic unit inputs; ordinary user-session variables remain inherited. Filesystem authority, applications, and passwordless sudo remain available. Carillon has no secondary command, path, capability, or extension authorization framework.

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

A backup captures the complete Carillon installation, including its database and configuration, agent files, runtime-owned continuation resources and sessions, component-owned durable evidence, internal credentials, and Pi-private authentication and settings. The archive is created with owner-only permissions because it contains credentials; encrypt it before storing or transferring it outside the owner-controlled host.

Restore reconstructs a coherent installation and reconciles core and component state before accepting new work. The operator can continue from the next safe action without repeating completed runtime work or an external delivery whose outcome is uncertain.

## Upgrades and uninstall

Components can evolve independently when their public Carillon boundary remains compatible; coordinated upgrades remain possible when a boundary itself changes. Native runtime formats remain owned by their runtime adapters and use their supported compatibility behavior rather than being rewritten into one Carillon session format.

Re-running the installer updates a supported per-user installation: it replaces managed application files and user units and updates Discord admission without replacing configuration, credentials, sessions, or durable state.

The installed uninstaller stops and removes Carillon's user units and application files while preserving `CARILLON_HOME` data. It never removes unrelated owner files or the owner's global `~/.pi`.

The exact transport, schemas, queues, and module layout remain engineering choices. The operational contract is a rootless owner installation that preserves native user authority, remote admission, continuity, recoverability, and data-safe rollback.
