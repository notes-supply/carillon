# Carillon

A self-hosted AI agent you talk to in Discord, powered by
[Pi](https://github.com/earendil-works/pi) or [T3 Code](https://github.com/pingdotgg/t3code).

Chat in channels or threads, attach files and images, pick models with
`/model` and run the agent's own commands with `/command`. A card shows what
the agent is doing while it works. Conversations and replies survive
restarts.

Carillon runs on your Linux machine as user services, with your own
permissions: the agent can do whatever your account can. It's a public beta,
so state and configuration formats may change between releases without
migrations.

## What you need

- x86_64 Linux with glibc 2.39 or newer and systemd
- GNU coreutils 9.5 or newer, for updates
- A Discord bot in your server, with the `applications.commands` scope
- A model provider account that Pi supports, or a T3 Code server

## Install

```sh
version=0.1.0-beta.1
base=https://github.com/notes-supply/carillon/releases/download/v$version
curl -fLO "$base/carillon-$version-linux-x86_64.tar.gz"
curl -fLO "$base/SHA256SUMS"
sha256sum -c SHA256SUMS
tar -xzf "carillon-$version-linux-x86_64.tar.gz"
"carillon-$version/install.sh" --guild-id GUILD_ID --bot-user-id BOT_ID \
  --user-id YOUR_DISCORD_ID --route CHANNEL_ID:always:thread
```

The installer links the `carillon` command into `~/.local/bin` and prints
the next steps. Put the bot token where it says, sign in to a model provider
with the printed `carillon-pi` command, then start Carillon:

```sh
~/.local/bin/carillon start    # or just `carillon start` if ~/.local/bin is on your PATH
```

The [operations guide](docs/product/OPERATIONS.md) covers the options,
everyday commands, configuration, updates and backups.
[T3 Code](packages/runtime-t3/README.md) is optional.

## Development

Rust lives in `crates/`, TypeScript in `packages/`. [Mise](https://mise.jdx.dev)
installs the tools and runs the tasks:

```sh
mise install
mise run check            # format, lint, types and tests; or check:rust / check:ts
mise run test:integration # the services together
mise run test:install     # build the release archive and test installing it
```

`mise run package` writes the archive to `dist/`. The
[architecture](docs/product/ARCHITECTURE.md) and
[component guides](docs/product/README.md) describe the internals.

Carillon is licensed under the [Apache License 2.0](LICENSE).
