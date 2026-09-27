//! Agent operations: one guild, the existing Discord client, no participation changes.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::time::Duration;
use twilight_http::{
    Client, Response, error::ErrorType, request::channel::reaction::RequestReactionType,
};
use twilight_model::{
    channel::{Channel, ChannelType, message::AllowedMentions},
    guild::Permissions,
    http::{
        attachment::Attachment,
        permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    },
    id::{
        Id,
        marker::{ChannelMarker, GuildMarker, MessageMarker, RoleMarker, UserMarker},
    },
    util::Timestamp,
};

#[cfg(test)]
mod tests;

type ChannelId = Id<ChannelMarker>;
type MessageId = Id<MessageMarker>;
type RoleId = Id<RoleMarker>;
type UserId = Id<UserMarker>;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Operation {
    Server(Server),
    Channels(Channels),
    Threads(Threads),
    Messages(Messages),
    Roles(Roles),
    Members(Members),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum Server {
    #[serde(rename = "server.info")]
    Info,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum Channels {
    #[serde(rename = "channels.list")]
    List,
    #[serde(rename = "channels.get")]
    Get { channel_id: ChannelId },
    #[serde(rename = "channels.create")]
    Create {
        name: String,
        kind: NewChannelKind,
        parent_id: Option<ChannelId>,
    },
    #[serde(rename = "channels.update")]
    Update {
        channel_id: ChannelId,
        name: Option<String>,
        topic: Option<String>,
        parent_id: Option<ChannelId>,
        #[serde(default)]
        remove_parent: bool,
    },
    #[serde(rename = "channels.delete")]
    Delete { channel_id: ChannelId },
    #[serde(rename = "channels.overwrite")]
    Overwrite {
        channel_id: ChannelId,
        target_id: Id<twilight_model::id::marker::GenericMarker>,
        kind: OverwriteKind,
        allow: String,
        deny: String,
    },
    #[serde(rename = "channels.remove_overwrite")]
    RemoveOverwrite {
        channel_id: ChannelId,
        target_id: Id<twilight_model::id::marker::GenericMarker>,
        kind: OverwriteKind,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum Threads {
    #[serde(rename = "threads.list")]
    List,
    #[serde(rename = "threads.create")]
    Create {
        channel_id: ChannelId,
        name: String,
        message_id: Option<MessageId>,
    },
    #[serde(rename = "threads.update")]
    Update {
        channel_id: ChannelId,
        archived: bool,
        locked: Option<bool>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum Messages {
    #[serde(rename = "messages.list")]
    List {
        channel_id: ChannelId,
        before: Option<MessageId>,
        #[serde(default = "page_size")]
        limit: u16,
    },
    #[serde(rename = "messages.get")]
    Get {
        channel_id: ChannelId,
        message_id: MessageId,
    },
    #[serde(rename = "messages.send")]
    Send {
        channel_id: ChannelId,
        content: String,
        reply_to: Option<MessageId>,
        #[serde(default)]
        mention_users: Vec<UserId>,
        #[serde(default)]
        mention_roles: Vec<RoleId>,
        #[serde(default)]
        attachments: Vec<Upload>,
    },
    #[serde(rename = "messages.edit")]
    Edit {
        channel_id: ChannelId,
        message_id: MessageId,
        content: String,
    },
    #[serde(rename = "messages.delete")]
    Delete {
        channel_id: ChannelId,
        message_id: MessageId,
    },
    #[serde(rename = "messages.pin")]
    Pin {
        channel_id: ChannelId,
        message_id: MessageId,
        pinned: bool,
    },
    #[serde(rename = "messages.react")]
    React {
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: String,
        add: bool,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum Roles {
    #[serde(rename = "roles.list")]
    List,
    #[serde(rename = "roles.create")]
    Create {
        name: String,
        permissions: String,
        color: Option<u32>,
        #[serde(default)]
        hoist: bool,
    },
    #[serde(rename = "roles.update")]
    Update {
        role_id: RoleId,
        name: Option<String>,
        permissions: Option<String>,
        color: Option<u32>,
        hoist: Option<bool>,
    },
    #[serde(rename = "roles.delete")]
    Delete { role_id: RoleId },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum Members {
    #[serde(rename = "members.get")]
    Get { user_id: UserId },
    #[serde(rename = "members.list")]
    List {
        after: Option<UserId>,
        #[serde(default = "page_size")]
        limit: u16,
    },
    #[serde(rename = "members.role")]
    Role {
        user_id: UserId,
        role_id: RoleId,
        add: bool,
    },
    #[serde(rename = "members.timeout")]
    Timeout {
        user_id: UserId,
        #[serde(deserialize_with = "Option::deserialize")]
        until: Option<String>,
    },
    #[serde(rename = "members.kick")]
    Kick { user_id: UserId },
    #[serde(rename = "members.ban")]
    Ban { user_id: UserId, banned: bool },
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upload {
    filename: String,
    data: String,
}

const fn page_size() -> u16 {
    30
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewChannelKind {
    Text,
    Category,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverwriteKind {
    Role,
    Member,
}

#[derive(Debug, Serialize)]
pub struct OperationError {
    pub outcome: &'static str,
    pub message: String,
}
impl std::fmt::Display for OperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.outcome, self.message)
    }
}
impl std::error::Error for OperationError {}

impl OperationError {
    fn invalid(message: &str) -> Self {
        Self {
            outcome: "rejected",
            message: message.into(),
        }
    }
    fn uncertain() -> Self {
        Self {
            outcome: "uncertain",
            message:
                "Discord may have applied this operation. Inspect the target before trying again."
                    .into(),
        }
    }
}
impl From<twilight_http::Error> for OperationError {
    fn from(error: twilight_http::Error) -> Self {
        match error.kind() {
            ErrorType::Response { status, .. } if status.get() < 500 => Self::invalid(&format!(
                "Discord rejected the request (HTTP {}). Check the target, permissions and role hierarchy; for 429, wait before retrying.",
                status.get()
            )),
            ErrorType::Validation => Self::invalid("Discord request arguments are invalid."),
            _ => Self::uncertain(),
        }
    }
}

impl Operation {
    #[must_use]
    pub const fn mutates(&self) -> bool {
        !matches!(
            self,
            Self::Server(_)
                | Self::Channels(Channels::List | Channels::Get { .. })
                | Self::Threads(Threads::List)
                | Self::Messages(Messages::List { .. } | Messages::Get { .. })
                | Self::Roles(Roles::List)
                | Self::Members(Members::Get { .. } | Members::List { .. })
        )
    }

    /// Execute at most once. Twilight queues rate-limited requests, but does not
    /// retry ambiguous HTTP mutations. A dropped caller cannot imply rollback.
    /// # Errors
    /// Returns a rejection or an explicitly uncertain mutation outcome.
    pub async fn execute(
        self,
        http: &Client,
        guild: Id<GuildMarker>,
    ) -> Result<Value, OperationError> {
        let mutates = self.mutates();
        let result = tokio::time::timeout(Duration::from_secs(30), self.run(http, guild))
            .await
            .unwrap_or_else(|_| Err(OperationError::uncertain()));
        result.map_err(|mut error| {
            if !mutates && error.outcome == "uncertain" {
                error.outcome = "unavailable";
                error.message =
                    "Discord could not complete this read. No mutation was requested.".into();
            }
            error
        })
    }

    async fn run(self, http: &Client, guild: Id<GuildMarker>) -> Result<Value, OperationError> {
        match self {
            Self::Server(operation) => operation.run(http, guild).await,
            Self::Channels(operation) => operation.run(http, guild).await,
            Self::Threads(operation) => operation.run(http, guild).await,
            Self::Messages(operation) => operation.run(http, guild).await,
            Self::Roles(operation) => operation.run(http, guild).await,
            Self::Members(operation) => operation.run(http, guild).await,
        }
    }
}

impl Server {
    async fn run(self, http: &Client, guild: Id<GuildMarker>) -> Result<Value, OperationError> {
        match self {
            Self::Info => {
                let bot = http
                    .current_user()
                    .await?
                    .model()
                    .await
                    .map_err(|_| OperationError::uncertain())?;
                let member = http
                    .guild_member(guild, bot.id)
                    .await?
                    .model()
                    .await
                    .map_err(|_| OperationError::uncertain())?;
                let roles = http
                    .roles(guild)
                    .await?
                    .models()
                    .await
                    .map_err(|_| OperationError::uncertain())?;
                let permissions = roles
                    .iter()
                    .filter(|r| r.id.get() == guild.get() || member.roles.contains(&r.id))
                    .fold(Permissions::empty(), |p, r| p | r.permissions);
                Ok(
                    json!({ "guild_id": guild, "bot_id": bot.id, "permissions": permissions.bits().to_string(), "administrator": permissions.contains(Permissions::ADMINISTRATOR), "note": "Discord channel overwrites and role hierarchy remain authoritative." }),
                )
            }
        }
    }
}

impl Channels {
    #[expect(
        clippy::too_many_lines,
        reason = "direct dispatch over the resource-specific Discord operations"
    )]
    async fn run(self, http: &Client, guild: Id<GuildMarker>) -> Result<Value, OperationError> {
        match self {
            Self::List => {
                let channels = http
                    .guild_channels(guild)
                    .await?
                    .models()
                    .await
                    .map_err(|_| OperationError::uncertain())?;
                Ok(
                    json!({ "items": channels, "complete": true, "scope": "guild channels; threads.list returns active threads separately" }),
                )
            }
            Self::Get { channel_id } => Ok(json!(channel(http, guild, channel_id).await?)),
            Self::Create {
                name,
                kind,
                parent_id,
            } => {
                if let Some(parent) = parent_id {
                    category(http, guild, parent).await?;
                }
                let kind = match kind {
                    NewChannelKind::Text => ChannelType::GuildText,
                    NewChannelKind::Category => ChannelType::GuildCategory,
                };
                if kind == ChannelType::GuildCategory && parent_id.is_some() {
                    return Err(OperationError::invalid("Categories cannot have a parent."));
                }
                let mut request = http.create_guild_channel(guild, &name).kind(kind);
                if let Some(parent) = parent_id {
                    request = request.parent_id(parent);
                }
                decoded(request.await?).await
            }
            Self::Update {
                channel_id,
                name,
                topic,
                parent_id,
                remove_parent,
            } => {
                let target = channel(http, guild, channel_id).await?;
                if parent_id.is_some() && remove_parent {
                    return Err(OperationError::invalid(
                        "Specify a parent or remove_parent, not both.",
                    ));
                }
                if target.kind == ChannelType::GuildCategory && parent_id.is_some() {
                    return Err(OperationError::invalid("Categories cannot have a parent."));
                }
                if let Some(parent) = parent_id {
                    category(http, guild, parent).await?;
                }
                let mut request = http.update_channel(channel_id);
                if let Some(name) = &name {
                    request = request.name(name);
                }
                if let Some(topic) = &topic {
                    request = request.topic(topic);
                }
                if let Some(parent) = parent_id {
                    request = request.parent_id(Some(parent));
                }
                if remove_parent {
                    request = request.parent_id(None);
                }
                decoded(request.await?).await
            }
            Self::Delete { channel_id } => {
                channel(http, guild, channel_id).await?;
                decoded(http.delete_channel(channel_id).await?).await
            }
            Self::Overwrite {
                channel_id,
                target_id,
                kind,
                allow,
                deny,
            } => {
                channel(http, guild, channel_id).await?;
                let allow = permissions(&allow)?;
                let deny = permissions(&deny)?;
                if !(allow & deny).is_empty() {
                    return Err(OperationError::invalid(
                        "A permission cannot be both allowed and denied.",
                    ));
                }
                let kind = match kind {
                    OverwriteKind::Role => {
                        role(http, guild, target_id.cast()).await?;
                        PermissionOverwriteType::Role
                    }
                    OverwriteKind::Member => {
                        http.guild_member(guild, target_id.cast()).await?;
                        PermissionOverwriteType::Member
                    }
                };
                http.update_channel_permission(
                    channel_id,
                    &PermissionOverwrite {
                        id: target_id,
                        kind,
                        allow: Some(allow),
                        deny: Some(deny),
                    },
                )
                .await?;
                Ok(json!({ "channel_id": channel_id, "target_id": target_id, "updated": true }))
            }
            Self::RemoveOverwrite {
                channel_id,
                target_id,
                kind,
            } => {
                channel(http, guild, channel_id).await?;
                match kind {
                    OverwriteKind::Role => {
                        role(http, guild, target_id.cast()).await?;
                        http.delete_channel_permission(channel_id)
                            .role(target_id.cast())
                            .await?;
                    }
                    OverwriteKind::Member => {
                        http.guild_member(guild, target_id.cast()).await?;
                        http.delete_channel_permission(channel_id)
                            .member(target_id.cast())
                            .await?;
                    }
                }
                Ok(json!({ "channel_id": channel_id, "target_id": target_id, "removed": true }))
            }
        }
    }
}

impl Threads {
    async fn run(self, http: &Client, guild: Id<GuildMarker>) -> Result<Value, OperationError> {
        match self {
            Self::List => decoded(http.active_threads(guild).await?).await,
            Self::Create {
                channel_id,
                name,
                message_id,
            } => {
                let parent = channel(http, guild, channel_id).await?;
                if !matches!(
                    parent.kind,
                    ChannelType::GuildText | ChannelType::GuildAnnouncement
                ) {
                    return Err(OperationError::invalid(
                        "A thread needs a text or announcement parent channel.",
                    ));
                }
                if let Some(message) = message_id {
                    http.message(channel_id, message).await?;
                    decoded(
                        http.create_thread_from_message(channel_id, message, &name)
                            .await?,
                    )
                    .await
                } else {
                    decoded(
                        http.create_thread(channel_id, &name, ChannelType::PublicThread)
                            .await?,
                    )
                    .await
                }
            }
            Self::Update {
                channel_id,
                archived,
                locked,
            } => {
                let target = channel(http, guild, channel_id).await?;
                if !matches!(
                    target.kind,
                    ChannelType::PublicThread
                        | ChannelType::PrivateThread
                        | ChannelType::AnnouncementThread
                ) {
                    return Err(OperationError::invalid("Target is not a thread."));
                }
                let mut request = http.update_thread(channel_id).archived(archived);
                if let Some(locked) = locked {
                    request = request.locked(locked);
                }
                decoded(request.await?).await
            }
        }
    }
}

impl Messages {
    #[expect(
        clippy::too_many_lines,
        reason = "direct dispatch over the resource-specific Discord operations"
    )]
    async fn run(self, http: &Client, guild: Id<GuildMarker>) -> Result<Value, OperationError> {
        match self {
            Self::List {
                channel_id,
                before,
                limit,
            } => {
                channel(http, guild, channel_id).await?;
                validate_limit(limit)?;
                let request = http.channel_messages(channel_id);
                let response = if let Some(before) = before {
                    request.before(before).limit(limit).await?
                } else {
                    request.limit(limit).await?
                };
                let messages = response
                    .models()
                    .await
                    .map_err(|_| OperationError::uncertain())?;
                let next = if messages.len() == usize::from(limit) {
                    messages.last().map(|m| m.id)
                } else {
                    None
                };
                Ok(
                    json!({ "items": messages, "next_before": next, "may_have_more": next.is_some() }),
                )
            }
            Self::Get {
                channel_id,
                message_id,
            } => {
                channel(http, guild, channel_id).await?;
                decoded(http.message(channel_id, message_id).await?).await
            }
            Self::Send {
                channel_id,
                content,
                reply_to,
                mention_users,
                mention_roles,
                attachments,
            } => {
                let attachments = uploads(attachments)?;
                if content.is_empty() && attachments.is_empty() {
                    return Err(OperationError::invalid(
                        "A message needs content or attachments.",
                    ));
                }
                channel(http, guild, channel_id).await?;
                if let Some(message) = reply_to {
                    http.message(channel_id, message).await?;
                }
                let mentions = AllowedMentions {
                    users: mention_users,
                    roles: mention_roles,
                    ..AllowedMentions::default()
                };
                let mut request = http
                    .create_message(channel_id)
                    .content(&content)
                    .allowed_mentions(Some(&mentions));
                if !attachments.is_empty() {
                    request = request.attachments(&attachments);
                }
                if let Some(message) = reply_to {
                    request = request.reply(message);
                }
                let mut result = decoded(request.await?).await?;
                if let Some(id) = result.get("id").and_then(Value::as_str) {
                    let url = format!("https://discord.com/channels/{guild}/{channel_id}/{id}");
                    result["url"] = json!(url);
                }
                Ok(result)
            }
            Self::Edit {
                channel_id,
                message_id,
                content,
            } => {
                channel(http, guild, channel_id).await?;
                decoded(
                    http.update_message(channel_id, message_id)
                        .content(Some(&content))
                        .allowed_mentions(Some(&AllowedMentions::default()))
                        .await?,
                )
                .await
            }
            Self::Delete {
                channel_id,
                message_id,
            } => {
                channel(http, guild, channel_id).await?;
                http.delete_message(channel_id, message_id).await?;
                Ok(json!({ "message_id": message_id, "deleted": true }))
            }
            Self::Pin {
                channel_id,
                message_id,
                pinned,
            } => {
                channel(http, guild, channel_id).await?;
                if pinned {
                    http.create_pin(channel_id, message_id).await?;
                } else {
                    http.delete_pin(channel_id, message_id).await?;
                }
                Ok(json!({ "message_id": message_id, "pinned": pinned }))
            }
            Self::React {
                channel_id,
                message_id,
                emoji,
                add,
            } => {
                channel(http, guild, channel_id).await?;
                if emojis::get(&emoji).is_none() {
                    return Err(OperationError::invalid(
                        "Use a Unicode emoji for reactions.",
                    ));
                }
                let emoji = RequestReactionType::Unicode { name: &emoji };
                if add {
                    http.create_reaction(channel_id, message_id, &emoji).await?;
                } else {
                    http.delete_current_user_reaction(channel_id, message_id, &emoji)
                        .await?;
                }
                Ok(json!({ "message_id": message_id, "added": add }))
            }
        }
    }
}

impl Roles {
    async fn run(self, http: &Client, guild: Id<GuildMarker>) -> Result<Value, OperationError> {
        match self {
            Self::List => {
                let items = http
                    .roles(guild)
                    .await?
                    .models()
                    .await
                    .map_err(|_| OperationError::uncertain())?;
                Ok(json!({ "items": items, "complete": true }))
            }
            Self::Create {
                name,
                permissions: bits,
                color,
                hoist,
            } => {
                let mut request = http
                    .create_role(guild)
                    .name(&name)
                    .permissions(permissions(&bits)?)
                    .hoist(hoist);
                if let Some(color) = color {
                    request = request.color(color);
                }
                decoded(request.await?).await
            }
            Self::Update {
                role_id,
                name,
                permissions: bits,
                color,
                hoist,
            } => {
                role(http, guild, role_id).await?;
                let mut request = http.update_role(guild, role_id);
                if let Some(name) = &name {
                    request = request.name(Some(name));
                }
                if let Some(bits) = &bits {
                    request = request.permissions(permissions(bits)?);
                }
                if let Some(color) = color {
                    request = request.color(Some(color));
                }
                if let Some(hoist) = hoist {
                    request = request.hoist(hoist);
                }
                decoded(request.await?).await
            }
            Self::Delete { role_id } => {
                role(http, guild, role_id).await?;
                http.delete_role(guild, role_id).await?;
                Ok(json!({ "role_id": role_id, "deleted": true }))
            }
        }
    }
}

impl Members {
    async fn run(self, http: &Client, guild: Id<GuildMarker>) -> Result<Value, OperationError> {
        match self {
            Self::Get { user_id } => decoded(http.guild_member(guild, user_id).await?).await,
            Self::List { after, limit } => {
                validate_limit(limit)?;
                let mut request = http.guild_members(guild).limit(limit);
                if let Some(after) = after {
                    request = request.after(after);
                }
                let members = request
                    .await?
                    .models()
                    .await
                    .map_err(|_| OperationError::uncertain())?;
                let next = if members.len() == usize::from(limit) {
                    members.last().map(|m| m.user.id)
                } else {
                    None
                };
                Ok(json!({ "items": members, "next_after": next, "may_have_more": next.is_some() }))
            }
            Self::Role {
                user_id,
                role_id,
                add,
            } => {
                role(http, guild, role_id).await?;
                if add {
                    http.add_guild_member_role(guild, user_id, role_id).await?;
                } else {
                    http.remove_guild_member_role(guild, user_id, role_id)
                        .await?;
                }
                Ok(json!({ "user_id": user_id, "role_id": role_id, "added": add }))
            }
            Self::Timeout { user_id, until } => {
                let timestamp =
                    until
                        .as_deref()
                        .map(Timestamp::parse)
                        .transpose()
                        .map_err(|_| {
                            OperationError::invalid(
                                "until must be an ISO 8601 timestamp or null to clear the timeout.",
                            )
                        })?;
                decoded(
                    http.update_guild_member(guild, user_id)
                        .communication_disabled_until(timestamp)
                        .await?,
                )
                .await
            }
            Self::Kick { user_id } => {
                http.remove_guild_member(guild, user_id).await?;
                Ok(json!({ "user_id": user_id, "kicked": true }))
            }
            Self::Ban { user_id, banned } => {
                if banned {
                    http.create_ban(guild, user_id)
                        .delete_message_seconds(0)
                        .await?;
                } else {
                    http.delete_ban(guild, user_id).await?;
                }
                Ok(json!({ "user_id": user_id, "banned": banned }))
            }
        }
    }
}

fn uploads(input: Vec<Upload>) -> Result<Vec<Attachment>, OperationError> {
    if input.len() > 10 {
        return Err(OperationError::invalid(
            "At most 10 attachments are supported.",
        ));
    }
    let mut size = 0;
    input
        .into_iter()
        .enumerate()
        .map(|(index, upload)| {
            if upload.filename.is_empty() || upload.filename.contains(['/', '\\', '\0']) {
                return Err(OperationError::invalid(
                    "Attachment filename must be a plain filename.",
                ));
            }
            let bytes = STANDARD
                .decode(upload.data)
                .map_err(|_| OperationError::invalid("Attachment data must be base64."))?;
            size += bytes.len();
            if size > 4 * 1024 * 1024 {
                return Err(OperationError::invalid(
                    "Attachments exceed the 4 MiB per-message limit.",
                ));
            }
            Ok(Attachment::from_bytes(upload.filename, bytes, index as u64))
        })
        .collect()
}

async fn decoded<T: DeserializeOwned + Serialize + Unpin>(
    response: Response<T>,
) -> Result<Value, OperationError> {
    let model = response
        .model()
        .await
        .map_err(|_| OperationError::uncertain())?;
    serde_json::to_value(model).map_err(|_| OperationError::uncertain())
}
async fn channel(
    http: &Client,
    guild: Id<GuildMarker>,
    id: ChannelId,
) -> Result<Channel, OperationError> {
    let channel = http
        .channel(id)
        .await?
        .model()
        .await
        .map_err(|_| OperationError::uncertain())?;
    if channel.guild_id != Some(guild) {
        return Err(OperationError::invalid(
            "Channel is outside the configured Discord guild.",
        ));
    }
    Ok(channel)
}
async fn category(
    http: &Client,
    guild: Id<GuildMarker>,
    id: ChannelId,
) -> Result<(), OperationError> {
    if channel(http, guild, id).await?.kind != ChannelType::GuildCategory {
        return Err(OperationError::invalid(
            "Parent must be a category in the configured guild.",
        ));
    }
    Ok(())
}
async fn role(http: &Client, guild: Id<GuildMarker>, id: RoleId) -> Result<(), OperationError> {
    let roles = http
        .roles(guild)
        .await?
        .models()
        .await
        .map_err(|_| OperationError::uncertain())?;
    if !roles.iter().any(|role| role.id == id) {
        return Err(OperationError::invalid(
            "Role is outside the configured Discord guild.",
        ));
    }
    Ok(())
}
fn permissions(bits: &str) -> Result<Permissions, OperationError> {
    if bits.is_empty() || !bits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(OperationError::invalid(
            "Permissions must be a decimal bitfield string.",
        ));
    }
    bits.parse::<u64>()
        .map(Permissions::from_bits_retain)
        .map_err(|_| OperationError::invalid("Permission bitfield is too large."))
}
fn validate_limit(limit: u16) -> Result<(), OperationError> {
    if !(1..=100).contains(&limit) {
        return Err(OperationError::invalid("limit must be between 1 and 100."));
    }
    Ok(())
}
