# Discord agent access

An optional Pi extension and skill that let the agent read and manage the
Discord server through Carillon's Discord provider. It is separate from the
Wasm module behind slash commands, and importing it starts nothing.

## Install

```sh
carillon stop
carillon discord tools install
carillon start
```

This copies the release's extension and skill into the agent's resources,
replacing only `extensions/carillon-discord.js` and `skills/discord/SKILL.md`.
Core must be stopped while it runs; if it fails, run it again before starting.
Run it again after an update. Remove the two files to turn it off for new
turns. The regular installer doesn't enable these tools.

A fresh conversation sees one small tool, `enable_discord`; the skill tells
the agent to call it when needed, which adds the Discord tools to that
conversation. That's context management, not a security boundary: the agent
already has your Unix authority.

## Other local agents

With the provider running, other harnesses (T3, Claude Code, Codex) can use
the same operations:

```sh
carillon discord call --help
carillon discord call messages list --channel-id CHANNEL_ID --limit 20
carillon discord tools install --workspace /path/to/workspace
```

The workspace install writes the skill to `.claude/skills/discord/` and
`.agents/skills/discord/`. The CLI prints JSON, needs explicit IDs, and reads
the operator credential itself, so credentials never pass through model
arguments. If the harness runs as another Unix user, `--invocation 'COMMAND'`
records a command that user can already run; it changes no permissions.

## What it can do

Operations run in the provider with the bot's credential, within the
configured guild and Discord's own permissions and role hierarchy:

- server and bot discovery;
- channels and categories: list, create, edit, remove, permission overwrites;
- threads: list active, create, archive, lock;
- messages: read history, send, reply, edit, delete, pin and react, with up
  to ten local files (4 MiB together);
- roles: list, create, edit, remove;
- members: look up, list, assign roles, time out, kick, ban and unban.

It doesn't cover DMs, private-thread membership, archived threads, custom
emoji, webhooks, voice, audit logs or bulk moderation. Sends mention only the
user and role IDs listed explicitly. Results leave out Discord's signed
attachment URLs.

Requests time out after 30 seconds in the provider and 35 in the client. A
write whose answer was lost is reported as `uncertain` and never repeated
automatically; check before trying again. Explicit sends are separate from
the agent's normal reply.

## Where things live

- [`index.ts`](src/index.ts): the extension and its tools;
  [`activation.ts`](src/activation.ts): per-conversation activation;
  [`schemas.ts`](src/schemas.ts): the model-facing schemas.
- [`client.ts`](src/client.ts): the provider client, which only talks to a
  loopback address from the registry.
- [Rust operations](../../crates/carillon-discord/src/operations.rs): Discord
  calls and guild checks.
- [`skill`](skill/SKILL.md): the agent's instructions.

The runtime gives the extension the current conversation's Discord origin and
provider connection through a per-session `carillon:context` event, taken from
the admitted message and the installation's registry, never from user text.
