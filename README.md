# Carillon

A self-hosted AI agent you talk to in Discord, powered by
[Pi](https://github.com/earendil-works/pi) or [T3 Code](https://github.com/pingdotgg/t3code).

Chat in channels or threads, attach files and images, change a channel's
model and settings with `/settings` or `carillon settings`, and run the
agent's own commands with `/command`. A card shows what
the agent is doing while it works. Send another message while it answers to
steer it, and react to steer, interrupt or stop a waiting one. Conversations,
replies and, on Pi, answers in progress survive restarts.

Carillon runs on your Linux machine as user services, with your own
permissions: the agent can do whatever your account can. It's a public beta,
so state and configuration formats may change between releases without
migrations.

## What you need

- x86_64 Linux with glibc 2.39 or newer and systemd
- GNU coreutils 9.5 or newer, for updates
- A Discord application with a bot, and Message Content Intent turned on
  for it in the Discord Developer Portal
- A model provider account that Pi supports, or a T3 Code server on the
  same machine

## Install

In a terminal, as the user who will own Carillon:

```sh
curl -fsSL https://raw.githubusercontent.com/notes-supply/carillon/main/install.sh | sh
```

It checks your machine, downloads the newest release (betas included),
checks it against its published checksum, and asks you:

- **Pi or T3 Code.** Pi opens its own sign-in and model picker. For T3, give
  its address and a token, then pick one of its projects and a model.
- **Discord.** Paste the bot token (it isn't shown), then pick the server,
  the channels, how the agent answers and who may talk to it.

Then it starts Carillon and links `carillon` into `~/.local/bin`. For a
particular release, run `sh -s -- --version 0.1.0-beta.2` instead of `sh`.

The [operations guide](docs/product/OPERATIONS.md) covers installing without
questions or from the downloaded archive, everyday commands, configuration,
updates and backups. On Pi, the agent has [Discord tools](packages/discord-pi/README.md)
it loads when it needs them.

## Development

Rust lives in `crates/`, TypeScript in `packages/`. [Mise](https://mise.jdx.dev)
installs the tools and runs the tasks:

```sh
mise install
mise run check            # format, lint, types and tests; or check:lint / check:rust / check:ts
mise run test:integration # the services together
mise run test:install     # build the release archive and test installing it
```

`check:lint` runs the project's [ast-grep rules](lint/ast-grep/rules),
ShellCheck and a size budget. Source files stay under 1,000 lines. The longer
ones in `lint/size-budget.txt` may only shrink:
`scripts/check-size.sh --update` records a shrink.

`mise run build:module` builds the Discord module and the
[card colours example](examples/card-colours/README.md).

`mise run package` writes the archive to `dist/`. The
[architecture](docs/product/ARCHITECTURE.md) and
[component guides](docs/product/README.md) describe the internals.

Carillon is licensed under the [Apache License 2.0](LICENSE).
