//! Message admission, activation, durable participation, and conversation identity.
use crate::{
    InterfaceError,
    channels::Routing,
    config::{DIRECT_MESSAGES, DiscordActivation, DiscordConfig, DiscordConversation},
};
use anyhow::{Context, Result, bail};
use axum::http::StatusCode;
use carillon_discord_module::contract::{Intent, Interaction, InteractionKind};
use carillon_installation::filesystem::sync_ancestors;
use carillon_protocol::model::{ExternalEvent, NATIVE_INTENT};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{Mutex, MutexGuard, PoisonError},
};
use twilight_http::{
    Client as DiscordHttp, error::ErrorType as DiscordErrorType,
    response::StatusCode as DiscordStatusCode,
};
use twilight_model::{
    channel::{ChannelType, message::Message},
    id::{
        Id,
        marker::{ChannelMarker, MessageMarker},
    },
};

mod reference;

pub use reference::{Author, Reference, ReferenceKind};

/// An incoming Discord message, shared by gateway and simulation adapters.
/// Without a guild it is a direct message to the bot.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IncomingMessage {
    pub event_id: String,
    pub guild_id: Option<String>,
    pub user_id: String,
    pub channel_id: String,
    #[serde(default)]
    pub parent_channel_id: Option<String>,
    pub content: String,
    /// Files uploaded with the message, in message order. Nothing is
    /// fetched for them before the message is admitted and activated.
    #[serde(default)]
    pub attachments: Vec<SourceAttachment>,
    /// The message it replies to or forwards.
    #[serde(default)]
    pub reference: Option<Reference>,
}

impl IncomingMessage {
    /// A message as the gateway delivers it.
    pub(crate) fn received(message: &Message, bot_user_id: &str) -> Self {
        Self {
            event_id: message.id.to_string(),
            guild_id: message.guild_id.map(|id| id.to_string()),
            user_id: message.author.id.to_string(),
            channel_id: message.channel_id.to_string(),
            parent_channel_id: None,
            content: message.content.clone(),
            attachments: message
                .attachments
                .iter()
                .map(|attachment| SourceAttachment {
                    id: attachment.id.to_string(),
                    filename: attachment.filename.clone(),
                    size: attachment.size,
                    content_type: attachment.content_type.clone(),
                    url: attachment.url.clone(),
                })
                .collect(),
            reference: Reference::of(message, bot_user_id),
        }
    }
}

/// What Discord says about a file uploaded with a message.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SourceAttachment {
    pub id: String,
    /// The uploader's name for the file: display text only.
    pub filename: String,
    pub size: u64,
    #[serde(default)]
    pub content_type: Option<String>,
    /// A signed CDN address; never logged or shown with its query.
    pub url: String,
}
/// Durable record of the threads this provider participates in.
///
/// `threads` maps a thread to the route channel that admitted it, so later
/// thread messages resolve and stay activated without Discord lookups.
/// `created_threads` maps a root message to the thread already created for
/// it, so a retried message never creates a second thread.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ParticipationState {
    pub(crate) threads: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) created_threads: BTreeMap<String, String>,
}

pub struct Participation {
    path: PathBuf,
    state: Mutex<ParticipationState>,
}

impl Participation {
    /// Open durable participation state. The caller owns instance exclusivity.
    ///
    /// # Errors
    /// Returns filesystem or invalid persisted-state errors.
    pub fn load(path: PathBuf) -> Result<Self> {
        fs::create_dir_all(path.parent().context("participation path has no parent")?)?;
        let state = if path.exists() {
            fs::File::open(&path)?.sync_all()?;
            sync_ancestors(path.parent().context("participation path has no parent")?)?;
            serde_json::from_slice(&fs::read(&path)?)
                .with_context(|| format!("parse Discord participation state {}", path.display()))?
        } else {
            ParticipationState::default()
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, ParticipationState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn parent_route(&self, channel_id: &str) -> Option<String> {
        self.lock().threads.get(channel_id).cloned()
    }

    pub(crate) fn created_thread(&self, message_id: &str) -> Option<String> {
        self.lock().created_threads.get(message_id).cloned()
    }

    pub(crate) fn adopt(&self, thread_id: &str, route_channel_id: &str) -> Result<()> {
        let mut state = self.lock();
        let mut next = state.clone();
        next.threads
            .insert(thread_id.to_owned(), route_channel_id.to_owned());
        self.persist(&next)?;
        *state = next;
        Ok(())
    }

    pub(crate) fn remember_created(
        &self,
        message_id: &str,
        thread_id: &str,
        route_channel_id: &str,
    ) -> Result<()> {
        let mut state = self.lock();
        let mut next = state.clone();
        next.created_threads
            .insert(message_id.to_owned(), thread_id.to_owned());
        next.threads
            .insert(thread_id.to_owned(), route_channel_id.to_owned());
        self.persist(&next)?;
        *state = next;
        Ok(())
    }

    fn persist(&self, state: &ParticipationState) -> Result<()> {
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(state)?)?;
        fs::File::open(&temporary)?.sync_all()?;
        fs::rename(temporary, &self.path)?;
        sync_ancestors(
            self.path
                .parent()
                .context("participation path has no parent")?,
        )?;
        Ok(())
    }
}
/// An admitted, activated message and what conversation handling it needs.
#[derive(Clone, Debug)]
pub struct MessagePlan {
    /// The Discord message ID, which is also the provider event ID.
    pub(crate) message_id: String,
    /// `None` for a direct message.
    pub(crate) guild_id: Option<String>,
    pub(crate) user_id: String,
    pub(crate) agent_id: String,
    /// The channel or thread the message arrived in.
    pub(crate) channel_id: String,
    /// The configured route channel that admitted it (the parent channel for
    /// thread children), or [`DIRECT_MESSAGES`].
    pub(crate) route_channel_id: String,
    pub(crate) content: String,
    pub(crate) attachments: Vec<SourceAttachment>,
    pub(crate) reference: Option<Reference>,
    pub(crate) create_thread: bool,
    pub(crate) adopt_thread: bool,
    pub(crate) native: Value,
}

#[derive(Clone, Debug)]
pub struct ResolvedMessage {
    pub event: ExternalEvent,
    pub conversation_id: String,
}
/// Decides what to do with an admitted message.
///
/// `Err` means the channel is not routed (rejected); `Ok(None)` means the
/// route ignores this message (no activation or empty content). The gateway
/// silently drops both; simulation reports the rejection.
fn resolve_message(
    config: &DiscordConfig,
    request: IncomingMessage,
    native: Value,
    participation: &ParticipationState,
) -> Result<Option<MessagePlan>, InterfaceError> {
    let (mentioned, content) = strip_bot_mention(&request.content, &config.bot_user_id);
    let unrouted = || InterfaceError {
        status: StatusCode::FORBIDDEN,
        message: "channel is not configured as a Discord route".to_owned(),
    };
    let (agent_id, route_channel_id, activated, create_thread, adopt_thread) =
        if request.guild_id.is_none() {
            let direct = config.direct_messages.as_ref().ok_or_else(unrouted)?;
            (
                direct.agent.clone(),
                DIRECT_MESSAGES.to_owned(),
                true,
                false,
                false,
            )
        } else {
            let (route, child) = if let Some(route) = config
                .routes
                .iter()
                .find(|route| route.channel_id == request.channel_id)
            {
                (route, false)
            } else if let Some(route) = request.parent_channel_id.as_deref().and_then(|parent| {
                config.routes.iter().find(|route| {
                    route.channel_id == parent && route.conversation == DiscordConversation::Thread
                })
            }) {
                (route, true)
            } else {
                return Err(unrouted());
            };
            let adopted = child
                && participation
                    .threads
                    .get(&request.channel_id)
                    .is_some_and(|parent| parent == &route.channel_id);
            (
                route.agent.clone(),
                route.channel_id.clone(),
                match route.activation {
                    DiscordActivation::Always => true,
                    DiscordActivation::Mention => mentioned || adopted,
                },
                !child && route.conversation == DiscordConversation::Thread,
                child && route.activation == DiscordActivation::Mention && mentioned && !adopted,
            )
        };
    if !activated
        || (content.is_empty() && request.attachments.is_empty() && request.reference.is_none())
    {
        return Ok(None);
    }
    Ok(Some(MessagePlan {
        message_id: request.event_id,
        guild_id: request.guild_id,
        user_id: request.user_id,
        agent_id,
        channel_id: request.channel_id,
        route_channel_id,
        content,
        attachments: request.attachments,
        reference: request.reference,
        create_thread,
        adopt_thread,
        native,
    }))
}

fn strip_bot_mention(content: &str, bot_user_id: &str) -> (bool, String) {
    let direct = format!("<@{bot_user_id}>");
    let nickname = format!("<@!{bot_user_id}>");
    let mentioned = content.contains(&direct) || content.contains(&nickname);
    (
        mentioned,
        content
            .replace(&direct, "")
            .replace(&nickname, "")
            .trim()
            .to_owned(),
    )
}

pub(crate) fn prepare_deterministic_conversation(
    participation: &Participation,
    plan: MessagePlan,
) -> Result<ResolvedMessage, InterfaceError> {
    let target = if plan.create_thread {
        plan.message_id.clone()
    } else {
        plan.channel_id.clone()
    };
    if plan.create_thread || plan.adopt_thread {
        participation.adopt(&target, &plan.route_channel_id)?;
    }
    Ok(resolved_message(plan, target))
}

/// Prepare an admitted message's native target and persist thread participation.
/// Channel routes need no Discord request. Thread routes reconcile/create the
/// native thread before recording it; mentioned existing threads are adopted.
///
/// # Errors
/// Returns Discord or participation-storage failures. A failed call may have
/// created a native thread; retrying the same plan reconciles it before creating.
pub async fn prepare_conversation(
    participation: &Participation,
    http: &DiscordHttp,
    plan: MessagePlan,
) -> Result<ResolvedMessage> {
    let target = if plan.create_thread {
        let thread = if let Some(existing) = participation.created_thread(&plan.message_id) {
            existing
        } else {
            create_conversation_thread(http, &plan).await?
        };
        participation.remember_created(&plan.message_id, &thread, &plan.route_channel_id)?;
        thread
    } else {
        plan.channel_id.clone()
    };
    if plan.adopt_thread {
        participation.adopt(&target, &plan.route_channel_id)?;
    }
    Ok(resolved_message(plan, target))
}

pub(crate) async fn create_conversation_thread(
    http: &DiscordHttp,
    plan: &MessagePlan,
) -> Result<String> {
    let channel = plan
        .channel_id
        .parse::<Id<ChannelMarker>>()
        .context("source Discord channel ID is invalid")?;
    let message = plan
        .message_id
        .parse::<Id<MessageMarker>>()
        .context("source Discord message ID is invalid")?;
    // Discord assigns a message-started thread the source message's ID.
    // Reconcile native state before creating: a previous create may have
    // succeeded even when its response (or our local persistence) was lost.
    match http.channel(Id::new(message.get())).await {
        Ok(response) => {
            let thread = response
                .model()
                .await
                .context("decode existing Discord thread")?;
            if thread.id.get() != message.get()
                || thread.parent_id != Some(channel)
                || !matches!(
                    thread.kind,
                    ChannelType::PublicThread | ChannelType::AnnouncementThread
                )
            {
                bail!("Discord source message does not identify a thread in its route channel");
            }
            return Ok(thread.id.to_string());
        }
        Err(error) if matches!(error.kind(), DiscordErrorType::Response { status, .. } if *status == DiscordStatusCode::NOT_FOUND) =>
            {}
        Err(error) => return Err(error).context("resolve existing Discord conversation thread"),
    }
    Ok(http
        .create_thread_from_message(channel, message, &thread_title(plan))
        .await
        .context("create Discord conversation thread")?
        .model()
        .await
        .context("decode Discord conversation thread")?
        .id
        .to_string())
}

/// The message's words, for a file-only message the first file's name, or
/// for a bare forward the forwarded words.
fn thread_title(plan: &MessagePlan) -> String {
    let words = |text: &str| -> String {
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(80)
            .collect()
    };
    let title = words(&plan.content);
    if !title.is_empty() {
        return title;
    }
    [
        plan.attachments.first().map(|file| words(&file.filename)),
        plan.reference
            .as_ref()
            .map(|reference| words(&reference.content)),
    ]
    .into_iter()
    .flatten()
    .find(|title| !title.is_empty())
    .unwrap_or_else(|| "Attachment".to_owned())
}

/// A guild conversation is its channel or thread; a direct message's is the
/// person's one conversation with the bot.
fn resolved_message(plan: MessagePlan, target: String) -> ResolvedMessage {
    let (conversation_id, parent) = match &plan.guild_id {
        Some(guild) => (
            format!("discord-{guild}-{target}"),
            (target != plan.route_channel_id).then_some(&plan.route_channel_id),
        ),
        None => (format!("discord-dm-{target}"), None),
    };
    let origin = json!({
        "guild_id": plan.guild_id,
        "channel_id": target,
        "source_channel_id": plan.channel_id,
        "message_id": plan.message_id,
        "user_id": plan.user_id,
        "parent_channel_id": parent,
    });
    let content = match &plan.reference {
        Some(reference) if plan.content.is_empty() => reference.quote(),
        Some(reference) => format!("{}\n\n{}", reference.quote(), plan.content),
        None => plan.content,
    };
    ResolvedMessage {
        event: ExternalEvent {
            event_id: plan.message_id.clone(),
            agent_id: plan.agent_id,
            user_id: plan.user_id,
            audience_id: conversation_id.clone(),
            channel_id: plan.route_channel_id.clone(),
            delivery_target_id: target,
            feedback_target_id: format!("{}:{}", plan.channel_id, plan.message_id),
            content,
            native: json!({
                "origin": origin,
                "discord": plan.native,
                "guild_id": plan.guild_id,
                "route_channel_id": plan.route_channel_id,
            }),
            attachments: None,
        },
        conversation_id,
    }
}

/// Resolves the route parent of a message channel: `None` when the channel is
/// itself a route or no thread parent exists, asking Discord only when local
/// participation state cannot answer.
pub(crate) async fn discord_parent_channel(
    http: &DiscordHttp,
    config: &DiscordConfig,
    participation: &Participation,
    channel_id: &str,
) -> Result<Option<String>> {
    if config
        .routes
        .iter()
        .any(|route| route.channel_id == channel_id)
    {
        return Ok(None);
    }
    if let Some(parent) = participation.parent_route(channel_id) {
        return Ok(Some(parent));
    }
    let channel = http
        .channel(
            channel_id
                .parse()
                .context("Discord channel ID is invalid")?,
        )
        .await
        .context("resolve Discord channel route")?
        .model()
        .await
        .context("decode Discord channel route")?;
    if !matches!(
        channel.kind,
        ChannelType::AnnouncementThread | ChannelType::PrivateThread | ChannelType::PublicThread
    ) {
        return Ok(None);
    }
    Ok(channel.parent_id.map(|parent| parent.to_string()))
}
/// Fail-closed identity admission: an allowlisted user, in the configured
/// guild or, while the settings route them, in a direct message, before any
/// further handling.
pub(crate) fn admit_identity(
    config: &DiscordConfig,
    guild_id: Option<&str>,
    user_id: &str,
) -> Result<(), InterfaceError> {
    let refused = |message: &str| InterfaceError {
        status: StatusCode::FORBIDDEN,
        message: message.to_owned(),
    };
    match guild_id {
        None if config.direct_messages.is_none() => {
            return Err(refused("direct messages are not accepted"));
        }
        Some(guild) if guild != config.guild_id => return Err(refused("guild is not configured")),
        _ => {}
    }
    if !config
        .allowed_user_ids
        .iter()
        .any(|allowed| allowed == user_id)
    {
        return Err(refused("user is not allowlisted"));
    }
    Ok(())
}

/// Apply guild/user admission and conversation activation without a gateway.
///
/// # Errors
/// Rejects unconfigured guilds/routes, direct messages without a
/// direct-message route, and users outside the allowlist.
pub fn admit_message(
    config: &DiscordConfig,
    participation: &Participation,
    message: IncomingMessage,
    native: Value,
) -> Result<Option<MessagePlan>, InterfaceError> {
    admit_identity(config, message.guild_id.as_deref(), &message.user_id)?;
    resolve_message(config, message, native, &participation.lock())
}

/// An incoming interaction with the platform identity needed for admission.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InteractionSource {
    pub guild_id: Option<String>,
    pub user_id: String,
    pub channel_id: String,
    /// The parent channel when the interaction happened in a thread.
    #[serde(default)]
    pub parent_channel_id: Option<String>,
    pub kind: InteractionKind,
    /// The command name or the component `custom_id`.
    pub name: String,
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub options: Value,
}

/// An admitted interaction: what the module sees, the route that admitted
/// it, the conversation it happened in when the agent participates there,
/// and where it came from.
pub struct AdmittedInteraction {
    pub interaction: Interaction,
    pub(crate) routing: Routing,
    pub audience_id: Option<String>,
    pub origin: InteractionOrigin,
}

/// Who invoked an admitted interaction, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InteractionOrigin {
    pub guild_id: String,
    pub user_id: String,
    /// The channel or thread the interaction happened in.
    pub channel_id: String,
    /// The configured route that admitted it, or the unrouted channel.
    pub route_channel_id: String,
}

/// Fail-closed admission for interactions: the configured guild, an
/// allowlisted user, and a routed channel, a thread under a thread route, or
/// a channel no route names yet, which reaches no runtime. The conversation
/// is the one messages there would belong to.
///
/// # Errors
/// Rejects direct messages, unconfigured guilds, users outside the
/// allowlist, and threads outside a thread route's channel.
pub fn admit_interaction(
    config: &DiscordConfig,
    source: InteractionSource,
) -> Result<AdmittedInteraction, InterfaceError> {
    let Some(guild_id) = source.guild_id else {
        return Err(InterfaceError {
            status: StatusCode::FORBIDDEN,
            message: "commands are not accepted in direct messages".to_owned(),
        });
    };
    admit_identity(config, Some(&guild_id), &source.user_id)?;
    let (audience_id, routing) = if let Some(route) = config
        .routes
        .iter()
        .find(|route| route.channel_id == source.channel_id)
    {
        (
            (route.conversation == DiscordConversation::Channel)
                .then(|| format!("discord-{guild_id}-{}", source.channel_id)),
            Routing::Routed(route.clone()),
        )
    } else if let Some(parent) = source.parent_channel_id.as_deref() {
        let route = config
            .routes
            .iter()
            .find(|route| {
                route.channel_id == parent && route.conversation == DiscordConversation::Thread
            })
            .ok_or_else(|| InterfaceError {
                status: StatusCode::FORBIDDEN,
                message: "thread is not in a Discord route's channel".to_owned(),
            })?;
        (
            Some(format!("discord-{guild_id}-{}", source.channel_id)),
            Routing::Routed(route.clone()),
        )
    } else {
        // A channel of the guild no route names: its settings page can
        // route it, and nothing reaches a runtime until then.
        (None, Routing::Unrouted(source.channel_id.clone()))
    };
    Ok(AdmittedInteraction {
        interaction: Interaction {
            kind: source.kind,
            name: source.name,
            values: source.values,
            options: source.options,
            conversation: audience_id.is_some(),
            markers: config.activity_markers.clone(),
        },
        origin: InteractionOrigin {
            guild_id,
            user_id: source.user_id,
            channel_id: source.channel_id,
            route_channel_id: routing.channel_id().to_owned(),
        },
        routing,
        audience_id,
    })
}

/// The event for text a person submitted through an interaction in a
/// conversation, anchored on the provider's marker message for it: the
/// marker is the event's identity and feedback target, as a person's own
/// message would be. `native.intent` tells the runtime why it was sent.
pub(crate) fn submitted_event(
    agent_id: &str,
    audience_id: &str,
    origin: &InteractionOrigin,
    marker_id: &str,
    text: &str,
    intent: Intent,
) -> ExternalEvent {
    let parent = (origin.channel_id != origin.route_channel_id).then_some(&origin.route_channel_id);
    ExternalEvent {
        event_id: marker_id.to_owned(),
        agent_id: agent_id.to_owned(),
        user_id: origin.user_id.clone(),
        audience_id: audience_id.to_owned(),
        channel_id: origin.route_channel_id.clone(),
        delivery_target_id: origin.channel_id.clone(),
        feedback_target_id: format!("{}:{marker_id}", origin.channel_id),
        content: text.to_owned(),
        native: json!({
            NATIVE_INTENT: intent,
            "origin": {
                "guild_id": origin.guild_id,
                "channel_id": origin.channel_id,
                "source_channel_id": origin.channel_id,
                "message_id": marker_id,
                "user_id": origin.user_id,
                "parent_channel_id": parent,
            },
            "guild_id": origin.guild_id,
            "route_channel_id": origin.route_channel_id,
        }),
        attachments: None,
    }
}

#[cfg(test)]
pub(crate) mod tests;
