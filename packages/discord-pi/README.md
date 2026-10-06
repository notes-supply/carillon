# Discord tools for Pi

Optional native Pi extension for Carillon's Discord provider. This is separate
from the Wasm module that renders slash commands. It does not start a service,
load model credentials, or make Discord calls when imported.

## Installed use

```sh
carillon discord tools install
```

This copies the release's bundled extension and skill into the configured agent's
resource root. It replaces only `extensions/carillon-discord.js` and
`skills/discord/SKILL.md`; put personal customizations elsewhere. Re-run after a
release update to update this optional integration. Remove those two files to
stop including it in newly accepted turns. No gateway restart is required.
Already accepted turns keep their captured extension and skill, as with other
agent resources. The ordinary release installer does not enable these tools.

The skill teaches the assistant to call `enable_discord`. A fresh conversation
exposes that small activation tool, not the detailed Discord schemas. Activation
checks the connection, adds the domain tools without replacing existing tools,
and takes effect on the next inference within the same turn. Pi's native system
messages persist the tool loadout. Reopening the same conversation restores it;
another conversation starts inactive. This is context management, not a security
barrier: native extensions and Bash have the installing Unix user's authority.

## Ownership and construction

- [`index.ts`](src/index.ts): native extension entry point and tool adaptation.
- [`activation.ts`](src/activation.ts): native per-conversation activation.
- [`schemas.ts`](src/schemas.ts): model-facing action-specific schemas, grouped by
  server, messages, channels, threads, roles and members.
- [`client.ts`](src/client.ts): bounded authenticated provider client. No bot token
  and no model-supplied endpoint; the operator credential is read for, and sent to,
  only an HTTP loopback socket address, as the registry records providers.
- [Rust operations](../../crates/carillon-discord/src/operations.rs): Discord
  API execution, configured-guild validation, mention behavior and failures.
- [`skill`](skill/SKILL.md): progressive discovery and operating guidance.

The Pi runtime supplies a private `carillon:context` event after loading extensions
and before constructing the session. Application assembly supplies:

```ts
{
  interface_id: "discord-private",
  connection: { url, token_file, token_hash },
  native: { origin: {
    guild_id, channel_id, source_channel_id, message_id, user_id,
    parent_channel_id // optional / null
  } }
}
```

`native` is captured from admitted interface input; it is not parsed from user
text. The connection comes from the installation registry and credentials, not
from captured resources or model arguments. Each session gets a separate event
bus and client closure. Another runtime can call the same provider endpoint
without importing Pi. For direct Pi composition, pass explicit `extensionContext`
to `createPiExecutionSession` and include this extension in the resource root.
No globals or process environment variables carry the current conversation.

The integration defaults channel-scoped actions to the admitted target, which
may be a new thread rather than the triggering message's source channel. Replies
require an explicit message ID. Reading/administering other channels never
changes participation routes. The configured guild and Discord's permissions
and hierarchy bound operations; activation does not grant permissions.

## Operations and limits

`POST /v1/operations` accepts one action-specific JSON request, authenticated with
the installation operator credential. Reads return IDs and honest pagination.
Normal final-answer delivery remains core-owned; explicit tool sends are separate
side effects and must not also be sent through the normal outbox.

Coverage: guild/bot discovery; channel/category listing, creation, edits, removal
and per-target permission overwrites; active thread listing, public/message thread
creation and archive/lock changes; message history/read/send/reply/edit/delete,
pins and Unicode reactions; role listing/creation/edit/removal; member lookup and
pagination, role assignment, timeout, kick and ban/unban. Message sends support
up to ten explicit local files, 4 MiB combined. File reads happen in the extension,
not via arbitrary provider filesystem paths. `files` become bounded wire
`attachments` containing filenames and base64 bytes. The provider uses Twilight's
multipart API. Mentions are disabled except explicitly listed user/role IDs.
Results never carry Discord's signed attachment addresses: messages read,
sent or edited keep their attachments' names, sizes and types, including those
of replied-to and forwarded messages, without the reusable links.

Not all Discord APIs are covered: no DMs, private-thread membership, archived
thread enumeration, custom emoji management, webhooks, voice, audit logs or bulk
moderation. These are incremental operations, not reasons for another framework.

Provider requests time out after 30 seconds, client requests after 35 seconds;
HTTP request bodies are capped at 6 MiB and client responses at 2 MiB. List calls
are bounded and expose cursors or explicitly identify active-only scope. A lost
write response is `uncertain`, not failed-and-safe-to-retry. No tool request is
replayed automatically. Native Discord errors are summarized without raw
upstream bodies, credentials or stack traces. This is not exactly-once execution.

## Verification

```sh
mise run check
mise run test:integration
mise run test:install
mise run test:persistence
```

The native adapter tests drive actual Pi sessions and this extension with
controlled inference and a local provider stub. Rust tests drive the production
operations through a local Discord HTTP substitute; shared fixtures verify request
conformance. Install tests load the bundled artifact outside source dependencies.
These tests do not prove a live bot's permissions or a live model's tool choices.
