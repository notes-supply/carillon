import { Type, type TProperties } from "typebox";

const id = Type.String({ pattern: "^[1-9][0-9]{0,19}$", description: "Discord ID as a string" });
const channel = { channel_id: Type.Optional(id) };
const message = { ...channel, message_id: id };
const limit = Type.Optional(Type.Integer({ minimum: 1, maximum: 100 }));
const bits = Type.String({ pattern: "^[0-9]+$", description: "Decimal permission bitfield" });
const name = Type.String({ minLength: 1, maxLength: 100 });
const color = Type.Optional(Type.Integer({ minimum: 0, maximum: 16777215 }));
function action<A extends string, P extends TProperties>(value: A, fields: P) {
  return Type.Object({ action: Type.Literal(value), ...fields }, { additionalProperties: false });
}

export const groups = [
  {
    name: "discord_server",
    label: "Discord server",
    description:
      "Inspect the configured guild and bot's permissions. Discord channel overwrites and role hierarchy still apply.",
    schema: action("server.info", {}),
  },
  {
    name: "discord_messages",
    label: "Discord messages",
    description:
      "Read, send, edit, delete, pin or react to guild messages. Omitted channel uses this turn's destination. Sends are explicit effects, separate from your normal reply. Mentions are off unless explicitly listed. After an uncertain write inspect the target; do not blindly repeat it.",
    schema: Type.Union([
      action("messages.list", { ...channel, before: Type.Optional(id), limit }),
      action("messages.get", message),
      action("messages.send", {
        ...channel,
        content: Type.String({ maxLength: 2000 }),
        files: Type.Optional(
          Type.Array(Type.String({ minLength: 1 }), {
            maxItems: 10,
            description:
              "Local file paths to attach; 4 MiB combined. Use empty content for attachment-only messages.",
          }),
        ),
        reply_to: Type.Optional(id),
        mention_users: Type.Optional(Type.Array(id, { maxItems: 100 })),
        mention_roles: Type.Optional(Type.Array(id, { maxItems: 100 })),
      }),
      action("messages.edit", { ...message, content: Type.String({ maxLength: 2000 }) }),
      action("messages.delete", message),
      action("messages.pin", { ...message, pinned: Type.Boolean() }),
      action("messages.react", { ...message, emoji: Type.String(), add: Type.Boolean() }),
    ]),
  },
  {
    name: "discord_channels",
    label: "Discord channels",
    description:
      "Manage guild text channels and categories. Permission writes replace only the named target's allow/deny masks, not other overwrites. Reading/managing a channel does not enable bot participation there.",
    schema: Type.Union([
      action("channels.list", {}),
      action("channels.get", channel),
      action("channels.create", {
        name,
        kind: Type.Union([Type.Literal("text"), Type.Literal("category")]),
        parent_id: Type.Optional(id),
      }),
      action("channels.update", {
        ...channel,
        name: Type.Optional(name),
        topic: Type.Optional(Type.String({ maxLength: 1024 })),
        parent_id: Type.Optional(id),
        remove_parent: Type.Optional(Type.Boolean()),
      }),
      action("channels.delete", channel),
      action("channels.overwrite", {
        ...channel,
        target_id: id,
        kind: Type.Union([Type.Literal("role"), Type.Literal("member")]),
        allow: bits,
        deny: bits,
      }),
      action("channels.remove_overwrite", {
        ...channel,
        target_id: id,
        kind: Type.Union([Type.Literal("role"), Type.Literal("member")]),
      }),
    ]),
  },
  {
    name: "discord_threads",
    label: "Discord threads",
    description:
      "List active guild threads, create a public thread in a text channel (optionally from a message), or archive/unarchive and lock/unlock a thread. This does not move historical messages.",
    schema: Type.Union([
      action("threads.list", {}),
      action("threads.create", { ...channel, name, message_id: Type.Optional(id) }),
      action("threads.update", {
        ...channel,
        archived: Type.Boolean(),
        locked: Type.Optional(Type.Boolean()),
      }),
    ]),
  },
  {
    name: "discord_roles",
    label: "Discord roles",
    description:
      "Inspect and manage guild roles. Discord enforces the bot's role hierarchy; a role's permissions are decimal bitfields. Updates change only specified fields.",
    schema: Type.Union([
      action("roles.list", {}),
      action("roles.create", {
        name,
        permissions: bits,
        color,
        hoist: Type.Optional(Type.Boolean()),
      }),
      action("roles.update", {
        role_id: id,
        name: Type.Optional(name),
        permissions: Type.Optional(bits),
        color,
        hoist: Type.Optional(Type.Boolean()),
      }),
      action("roles.delete", { role_id: id }),
    ]),
  },
  {
    name: "discord_members",
    label: "Discord members",
    description:
      "Inspect guild members, assign/remove roles and perform explicitly requested moderation. Member listing may require the server-members intent. Banning does not delete message history. until=null clears a timeout.",
    schema: Type.Union([
      action("members.get", { user_id: id }),
      action("members.list", { after: Type.Optional(id), limit }),
      action("members.role", { user_id: id, role_id: id, add: Type.Boolean() }),
      action("members.timeout", { user_id: id, until: Type.Union([Type.String(), Type.Null()]) }),
      action("members.kick", { user_id: id }),
      action("members.ban", { user_id: id, banned: Type.Boolean() }),
    ]),
  },
];
