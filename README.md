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

`mise run journey` uses local loopback services and a deterministic runtime provider substitute. It exercises fail-closed Discord guild, user, and immutable-channel routing; mention and always activation; thread adoption and channel continuity; processing feedback; duplicate event suppression; continuing and recovered runtime state; abandoned-delivery reconciliation; authenticated diagnostics; live interface-extension/capability/resource activation; independent provider restarts; turn inspection; and stopped-service backup/restore.

`mise run package` creates `dist/carillon-0.0.0-linux-x86_64.tar.gz`. The archive contains the toolchain-free core and Pi runtime, a private pinned Pi CLI, user-systemd definitions, install/uninstall scripts, Pi interface assets, and the Pi license notice. The target host needs systemd and standard Linux user tools, not Rust, Node, Bun, Mise, a source checkout, or root installation.

## Install

Extract the release archive. As the ordinary Unix user who will own and run Carillon:

```text
./carillon-0.0.0/install.sh \
  --guild-id 123456789012345678 \
  --bot-user-id 345678901234567890 \
  --user-id 234567890123456789 \
  --route 456789012345678901:mention:thread \
  --route 567890123456789012:always:channel
```

The installer rejects root. `CARILLON_HOME` overrides the default `~/.carillon`; configuration, credentials, agents, Pi-private state, sessions, runtime state, application files, and logs stay beneath it with private modes. `--workspace PATH` overrides the default owner home used by Pi's native tools and project-local discovery. Relative configuration paths resolve from `CARILLON_HOME`.

The installer writes and enables user units in `${XDG_CONFIG_HOME:-~/.config}/systemd/user` but does not start them. Every process runs directly as the installing user with that user's UID and primary/supplementary groups. The units set HOME, USER, LOGNAME, shell, and PATH deterministically from the installing owner while retaining ordinary user-manager session variables. There are no Carillon service users, permission brokers, path ACLs, containers, privilege proxies, or tool approval databases.

1. Put the Discord bot token in `~/.carillon/credentials/discord-bot.token`; keep mode `0600` (substitute `CARILLON_HOME` when overridden).
2. Authenticate the packaged Pi CLI against Carillon-private state:

```text
~/.carillon/app/bin/carillon-pi --no-session --no-tools --no-approve
```

Run `/login`. This helper sets `PI_CODING_AGENT_DIR` to `CARILLON_HOME/pi`; it neither reads nor registers the owner's global `~/.pi` agent directory and does not install a global `pi` command.

3. Enable the Discord bot's Guilds, Guild Messages, and privileged Message Content intents. Grant each routed channel View Channel, Send Messages, Send Messages in Threads, Create Public Threads, Read Message History, and Add Reactions.
4. Start Carillon:

```text
systemctl --user start carillon.target
```

An administrator may explicitly enable boot-without-login persistence with `loginctl enable-linger OWNER`. Carillon does not perform that privileged host mutation.

5. List or refresh Pi's execution profiles, then set defaults for new conversations:

```text
~/.carillon/app/bin/carillon model \
  --config ~/.carillon/carillon.toml --refresh
~/.carillon/app/bin/carillon model \
  --config ~/.carillon/carillon.toml \
  --reasoning high --service-tier fast PROVIDER/MODEL
```

Use `--scope current-conversation --audience AUDIENCE_ID` to change one native continuation instead. Add `--interface PROVIDER_ID` when more than one interface is enabled; a single enabled interface remains the default. Catalog discovery, refresh, and application use that conversation's pinned runtime. Authenticated OpenAI Codex models expose **Standard** (normal processing) and **Fast** (OpenAI priority processing); other providers omit the speed selector. Pi persists the default beneath `CARILLON_HOME/pi`.

An allowlisted guild member can use Discord's native `/model` flow: provider, model, supported settings, then `this conversation` or `new conversations`. `/model` is accepted only in a configured route (or an eligible child thread), applies after the interaction, and does not alter an in-flight turn.

The Discord interface provider rejects direct messages, other guilds, users outside the immutable ID allowlist, and channels outside `[[discord.routes]]` before contacting core or discovering models. Each route explicitly sets `activation = "mention" | "always"` and `conversation = "thread" | "channel"`. Mention routes strip only the configured bot user's immutable `<@ID>` mention. Thread routes create or adopt thread-scoped continuations; channel routes keep one continuation in the configured channel and reject child threads.

Core configuration declares stable provider instances in `[[providers]]`. Each instance has one role (`interface`, `runtime`, `interface-extension`, or `capability`); `[agent].runtime_provider` selects the default runtime for new conversations and each `[[agent.interface_extensions]]` entry binds one extension to one interface. Existing conversations remain pinned to the runtime provider that owns their opaque continuation. The Discord adapter owns its optional `[discord]` block and refuses to launch when that block is absent or invalid; core and other interface providers do not require Discord fields.

Generated configuration enables typing and reactions independently and names every terminal emoji explicitly:

```toml
[discord]
provider = "discord-private"
agent = "default"
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

The agent resource directory is `CARILLON_HOME/agents/default`; the configured workspace is separate:

```text
SYSTEM.md
AGENTS.md
settings.json
extensions/
skills/
sessions/
```

The release installs only the core, Discord interface, and Pi runtime services. External interface-extension and capability providers require explicit `[[providers]]` records, agent bindings, credentials, binaries, and user-service packaging supplied by their operator; the release does not include example external providers.

The included Pi-native example resources are independent of those external providers and can be activated explicitly:

```text
~/.carillon/app/bin/carillon enable-demo-resources \
  --config ~/.carillon/carillon.toml
```

New turns copy `SYSTEM.md`, `AGENTS.md`, `settings.json`, `extensions/`, and `skills/` into a read-only content-addressed snapshot before dispatch. The recorded hashes and runtime inputs therefore describe the same immutable bytes.

Redacted `/health` endpoints remain unauthenticated for service supervision. Operator diagnostics use the installation's scoped operator credential through the CLI:

```text
~/.carillon/app/bin/carillon status \
  --config ~/.carillon/carillon.toml
~/.carillon/app/bin/carillon recent \
  --config ~/.carillon/carillon.toml --event-id EVENT_ID
~/.carillon/app/bin/carillon inspect \
  --config ~/.carillon/carillon.toml TURN_ID
```

Restart a default provider with `systemctl --user restart carillon-discord` or `carillon-runtime-pi`; core is supervised separately by `carillon-core`. All three services are ordinary processes owned by the same user and grouped by `carillon.target`; restarting one does not restart its siblings. Explicitly packaged optional providers can be supervised independently in the same way. Runtime correlation evidence prevents completed model work from being rerun; a delivery claimed before interruption becomes `uncertain` rather than being repeated.

`~/.carillon/app/bin/carillon-uninstall` stops and removes Carillon's user units and application files. It deliberately preserves `CARILLON_HOME` configuration and data and never touches unrelated owner files or `~/.pi`.

## Backup and restore

Stop the user target before backup:

```text
systemctl --user stop carillon.target
~/.carillon/app/bin/carillon backup \
  --config ~/.carillon/carillon.toml \
  --output ~/backups/carillon.tar.gz
```

Backup refuses to run while the core lock or any provider listener is active. It includes core SQLite state, configuration, common agent files, Pi resource snapshots and sessions, provider evidence/state, internal provider credentials, and release identity. Pi provider credentials, model catalogs, and selected model remain in `CARILLON_HOME/pi`; copy that private directory separately when moving the complete installation.

Restore into an empty owner-controlled root:

```text
~/.carillon/app/bin/carillon restore \
  --input ~/backups/carillon.tar.gz \
  --target ~/.carillon-restored
```

Restore runs as the owner. Runtime-owned continuation references are relocated by the runtime provider, and the next safe turn continues without repeating completed work or an uncertain delivery.
