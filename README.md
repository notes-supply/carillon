# Carillon

Carillon is a self-hosted, live-extensible agent composition service. The current product brief is in [`docs/product/`](docs/product/README.md).

## Build and prove the release

The repository uses Mise as its only development entry point:

```text
mise install
mise run journey
mise run check
mise run package
```

`mise run journey` uses local loopback services and a deterministic provider substitute. It exercises fail-closed Discord admission, continuing and recovered runtime state, duplicate event suppression, abandoned-delivery reconciliation, authenticated diagnostics, live interface-extension/capability/resource activation, independent component restarts, turn inspection, and stopped-service backup/restore.

`mise run package` creates `dist/carillon-0.0.0-linux-x86_64.tar.gz`. The archive contains two self-contained Linux executables, systemd service definitions, and an installer. The target host needs systemd and standard Linux administration tools, not Rust, Node, Bun, Mise, or a source checkout.

## Install

Extract the release archive, then run its installer as root:

```text
sudo ./carillon-0.0.0/install.sh \
  --guild-id 123456789012345678 \
  --user-id 234567890123456789
```

The installer creates scoped service users, `/opt/carillon/bin`, `/var/lib/carillon`, credential placeholders in `/etc/carillon`, and five independent systemd units. It does not start or enable them.

1. Put the Discord bot token in `/etc/carillon/discord.token`; keep mode `0600`.
2. Put an Anthropic API key in `/etc/carillon/provider.key`; keep mode `0600`.
3. Select a Pi model in `/var/lib/carillon/agents/default/settings.json`.
4. Enable the Discord bot's Guilds, Guild Messages, and privileged Message Content intents.
5. Start the services:

```text
sudo systemctl start \
  carillon-core carillon-runtime-pi carillon-discord \
  carillon-interface-extension carillon-capability
```

The Discord adapter rejects direct messages, other guilds, and users outside the immutable ID allowlist before contacting core. Ordinary guild messages go to the selected Pi continuation. `!carillon <argument>` is the external guild command surface; the credential-owning adapter performs delivery.

## Operate and extend

The agent directory is `/var/lib/carillon/agents/default`:

```text
SYSTEM.md
AGENTS.md
settings.json
extensions/
skills/
sessions/
```

Activate the included first-release additions without rebuilding a component:

```text
sudo /opt/carillon/bin/carillon component \
  --config /var/lib/carillon/carillon.toml guild-command enable
sudo /opt/carillon/bin/carillon component \
  --config /var/lib/carillon/carillon.toml echo-capability enable
sudo /opt/carillon/bin/carillon enable-demo-resources \
  --config /var/lib/carillon/carillon.toml
```

New turns copy `SYSTEM.md`, `AGENTS.md`, `settings.json`, `extensions/`, and `skills/` into a read-only content-addressed snapshot before dispatch. The recorded hashes and runtime inputs therefore describe the same immutable bytes.

Redacted `/health` endpoints remain unauthenticated for service supervision. Operator diagnostics use the installation's scoped operator credential through the CLI:

```text
sudo /opt/carillon/bin/carillon status \
  --config /var/lib/carillon/carillon.toml
sudo /opt/carillon/bin/carillon recent \
  --config /var/lib/carillon/carillon.toml --event-id EVENT_ID
sudo /opt/carillon/bin/carillon inspect \
  --config /var/lib/carillon/carillon.toml TURN_ID
```

Restart one component with `systemctl restart carillon-discord`, `carillon-runtime-pi`, `carillon-interface-extension`, or `carillon-capability`. The units have no shared `PartOf` lifecycle. Runtime correlation evidence prevents completed model work from being rerun; a delivery claimed before interruption becomes `uncertain` rather than being repeated.

## Backup and restore

Stop all five units before backup:

```text
sudo systemctl stop 'carillon-*'
sudo /opt/carillon/bin/carillon backup \
  --config /var/lib/carillon/carillon.toml \
  --output /srv/backup/carillon.tar.gz
```

Backup refuses to run while the core lock or any component listener is active. It includes core SQLite state, configuration, common agent files, Pi resources and sessions, component evidence/state, internal component credentials, and release identity.

Restore into an empty installation root:

```text
sudo /opt/carillon/bin/carillon restore \
  --input /srv/backup/carillon.tar.gz \
  --target /var/lib/carillon
```

Run restore as root so archived service ownership is preserved, then start the units. Runtime-owned continuation references are relocated by the runtime adapter, and the next safe turn continues without repeating completed work or an uncertain delivery.
