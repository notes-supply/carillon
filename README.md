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

`mise run journey` uses local loopback services and a deterministic provider substitute. It exercises fail-closed Discord guild, user, and immutable-channel routing; mention and always activation; thread adoption and channel continuity; processing feedback; duplicate event suppression; continuing and recovered runtime state; abandoned-delivery reconciliation; authenticated diagnostics; live interface-extension/capability/resource activation; independent component restarts; turn inspection; and stopped-service backup/restore.

`mise run package` creates `dist/carillon-0.0.0-linux-x86_64.tar.gz`. The archive contains three toolchain-free Linux executables, Pi's required interface assets and license notice, systemd service definitions, and an installer. The target host needs systemd and standard Linux administration tools, not Rust, Node, Bun, Mise, or a source checkout.

## Install

Extract the release archive, then run its installer as root:

```text
sudo ./carillon-0.0.0/install.sh \
  --guild-id 123456789012345678 \
  --bot-user-id 345678901234567890 \
  --user-id 234567890123456789 \
  --route 456789012345678901:mention:thread \
  --route 567890123456789012:always:channel

```

The installer creates scoped service users, `/opt/carillon/bin`, `/var/lib/carillon`, Pi-private `/var/lib/pi-carillon`, the Discord credential placeholder, explicit Discord routing and feedback defaults, and five independent systemd units. It does not start or enable them and performs no network access.

1. Put the Discord bot token in `/etc/carillon/discord.token`; keep mode `0600`.
2. Open Pi against its private state directory:

```text
sudo runuser -u carillon-runtime -- \
  env --chdir=/var/lib/pi-carillon PI_CODING_AGENT_DIR=/var/lib/pi-carillon \
  /opt/carillon/bin/pi --no-session --no-tools --no-approve
```

Run `/login` in Pi and complete the provider's native login flow.

3. Enable the Discord bot's Guilds, Guild Messages, and privileged Message Content intents. Grant each routed channel View Channel, Send Messages, Send Messages in Threads, Create Public Threads, Read Message History, and Add Reactions.
4. Start the services:

```text
sudo systemctl start \
  carillon-core carillon-runtime-pi carillon-discord \
  carillon-interface-extension carillon-capability
```

5. List or refresh Pi's available models, then persist a selection:

```text
sudo /opt/carillon/bin/carillon model \
  --config /var/lib/carillon/carillon.toml --refresh
sudo /opt/carillon/bin/carillon model \
  --config /var/lib/carillon/carillon.toml PROVIDER/MODEL
```

An allowlisted guild member can make the same selection with Discord's native `/model` picker. `/model` is explicit activation but is accepted only in a configured route (or an eligible child thread). The selection applies to subsequent turns; it does not alter an in-flight turn.

The Discord adapter rejects direct messages, other guilds, users outside the immutable ID allowlist, and channels outside `[[discord.routes]]` before contacting core or discovering models. Each route explicitly sets `activation = "mention" | "always"` and `conversation = "thread" | "channel"`. Mention routes strip only the configured bot user's immutable `<@ID>` mention. Thread routes create or adopt thread-scoped continuations; channel routes keep one continuation in the configured channel and reject child threads.

Generated configuration enables typing and reactions independently and names every terminal emoji explicitly:

```toml
[discord]
typing = true
reactions = true
processing_emoji = "👀"
delivered_emoji = "✅"
failed_emoji = "❌"
uncertain_emoji = "⚠️"

[[discord.routes]]
channel_id = "456789012345678901"
activation = "mention"
conversation = "thread"
```

Emoji values accept Unicode or canonical Discord custom `name:id` syntax and are validated at service startup. Ignored traffic produces no typing, reactions, core work, or replies. Accepted messages receive processing feedback; it transitions to delivered, failed before delivery, or uncertain delivery without changing the underlying turn truth if Discord rejects a feedback operation. `!carillon <argument>` remains the external guild command surface inside the same routes.

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

Backup refuses to run while the core lock or any component listener is active. It includes core SQLite state, configuration, common agent files, Pi resources and sessions, component evidence/state, internal component credentials, and release identity. Pi provider credentials, model catalogs, and selected model remain in `/var/lib/pi-carillon`; they are deliberately excluded from Carillon backups, resources, diagnostics, and logs.

Restore into an empty installation root:

```text
sudo /opt/carillon/bin/carillon restore \
  --input /srv/backup/carillon.tar.gz \
  --target /var/lib/carillon
```

Run restore as root so archived service ownership is preserved, then start the units. Runtime-owned continuation references are relocated by the runtime adapter, and the next safe turn continues without repeating completed work or an uncertain delivery.
