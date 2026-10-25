# Discord agent access

Discord reading and administration for Carillon's agents, through Carillon's
Discord provider. It is separate from the Wasm module behind slash commands.

## In the Pi runtime

The Pi runtime brings these tools as its built-in `discord` extension, which
its agents select by default when Carillon has a Discord provider; an agent's
`settings.json` can remove it. A conversation sees one small tool,
`enable_discord`: it identifies the guild, bot and current origin, returns
the Discord skill's guidance, and loads the Discord tools into that
conversation. That's context management, not a security boundary: the agent
already has your Unix authority.

## Other local agents

With the provider running, your other harnesses (Claude Code, Codex) use
the same operations through the CLI, as the skill tells them; a release
ships it as `app/lib/carillon-discord-SKILL.md`, for you to give those
harnesses if you want them to administer the server:

```sh
carillon discord call --help
carillon discord call messages list --channel-id CHANNEL_ID --limit 20
```

The CLI prints JSON, needs explicit IDs, and reads the operator credential
itself, so credentials never pass through model arguments.

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

- [`index.ts`](src/index.ts): the pi-durable extension and its tools;
  [`schemas.ts`](src/schemas.ts): the model-facing schemas.
- [`client.ts`](src/client.ts): the provider client, which only talks to a
  loopback address from the registry.
- [Rust operations](../../crates/carillon-discord/src/operations.rs): Discord
  calls and guild checks.
- [`skill`](skill/SKILL.md): the agent's instructions.

The runtime gives the tools each conversation's Discord origin, from the
latest message it admitted there, and its provider connection, from the
installation's registry; never from the model's arguments.
