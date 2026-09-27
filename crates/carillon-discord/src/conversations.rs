//! Message admission, activation, durable participation, and conversation identity.
use crate::{
    InterfaceError,
    config::{DiscordActivation, DiscordConfig, DiscordConversation},
};
use anyhow::{Context, Result, bail};
use axum::http::StatusCode;
use carillon_discord_commands::contract::{Interaction, InteractionKind};
use carillon_installation::filesystem::sync_ancestors;
use carillon_protocol::model::ExternalEvent;
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
    channel::ChannelType,
    id::{
        Id,
        marker::{ChannelMarker, MessageMarker},
    },
};
/// An incoming Discord message, shared by gateway and simulation adapters.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IncomingMessage {
    pub event_id: String,
    pub guild_id: Option<String>,
    pub user_id: String,
    pub channel_id: String,
    #[serde(default)]
    pub parent_channel_id: Option<String>,
    pub content: String,
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
    pub(crate) guild_id: String,
    pub(crate) user_id: String,
    pub(crate) agent_id: String,
    /// The channel or thread the message arrived in.
    pub(crate) channel_id: String,
    /// The configured route channel that admitted it (the parent channel for
    /// thread children).
    pub(crate) route_channel_id: String,
    pub(crate) content: String,
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
        return Err(InterfaceError {
            status: StatusCode::FORBIDDEN,
            message: "channel is not configured as a Discord route".to_owned(),
        });
    };
    let (mentioned, content) = strip_bot_mention(&request.content, &config.bot_user_id);
    let adopted = child
        && participation
            .threads
            .get(&request.channel_id)
            .is_some_and(|parent| parent == &route.channel_id);
    let activated = match route.activation {
        DiscordActivation::Always => true,
        DiscordActivation::Mention => mentioned || adopted,
    };
    if !activated || content.is_empty() {
        return Ok(None);
    }
    Ok(Some(MessagePlan {
        message_id: request.event_id,
        guild_id: request.guild_id.unwrap_or_default(),
        user_id: request.user_id,
        agent_id: config.agent.clone(),
        channel_id: request.channel_id,
        route_channel_id: route.channel_id.clone(),
        content,
        create_thread: !child && route.conversation == DiscordConversation::Thread,
        adopt_thread: child
            && route.activation == DiscordActivation::Mention
            && mentioned
            && !adopted,
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
        .create_thread_from_message(channel, message, &thread_title(&plan.content))
        .await
        .context("create Discord conversation thread")?
        .model()
        .await
        .context("decode Discord conversation thread")?
        .id
        .to_string())
}

fn thread_title(content: &str) -> String {
    content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect()
}

fn resolved_message(plan: MessagePlan, target: String) -> ResolvedMessage {
    let conversation_id = format!("discord-{}-{target}", plan.guild_id);
    let origin = json!({
        "guild_id": plan.guild_id,
        "channel_id": target,
        "source_channel_id": plan.channel_id,
        "message_id": plan.message_id,
        "user_id": plan.user_id,
        "parent_channel_id": if target == plan.route_channel_id { None } else { Some(&plan.route_channel_id) },
    });
    ResolvedMessage {
        event: ExternalEvent {
            event_id: plan.message_id.clone(),
            agent_id: plan.agent_id,
            user_id: plan.user_id,
            audience_id: conversation_id.clone(),
            delivery_target_id: target,
            feedback_target_id: format!("{}:{}", plan.channel_id, plan.message_id),
            content: plan.content,
            native: json!({
                "origin": origin,
                "discord": plan.native,
                "guild_id": plan.guild_id,
                "route_channel_id": plan.route_channel_id,
            }),
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
/// Fail-closed identity admission: the configured guild and an allowlisted
/// user, before any further handling.
pub(crate) fn admit_identity(
    config: &DiscordConfig,
    guild_id: Option<&str>,
    user_id: &str,
) -> Result<(), InterfaceError> {
    let guild = guild_id.ok_or_else(|| InterfaceError {
        status: StatusCode::FORBIDDEN,
        message: "direct messages are not accepted".to_owned(),
    })?;
    if guild != config.guild_id {
        return Err(InterfaceError {
            status: StatusCode::FORBIDDEN,
            message: "guild is not configured".to_owned(),
        });
    }
    if !config
        .allowed_user_ids
        .iter()
        .any(|allowed| allowed == user_id)
    {
        return Err(InterfaceError {
            status: StatusCode::FORBIDDEN,
            message: "user is not allowlisted".to_owned(),
        });
    }
    Ok(())
}

/// Apply guild/user admission and conversation activation without a gateway.
///
/// # Errors
/// Rejects direct messages, unconfigured guilds/routes, and users outside the allowlist.
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

/// An admitted interaction: what the module sees, and the conversation it
/// happened in when the agent participates there.
pub struct AdmittedInteraction {
    pub interaction: Interaction,
    pub audience_id: Option<String>,
}

/// Fail-closed admission for interactions: the configured guild, an
/// allowlisted user, and a routed channel or a thread under a thread route.
/// The conversation is the one messages there would belong to.
///
/// # Errors
/// Rejects unconfigured guilds/routes and users outside the allowlist.
pub fn admit_interaction(
    config: &DiscordConfig,
    source: InteractionSource,
) -> Result<AdmittedInteraction, InterfaceError> {
    admit_identity(config, source.guild_id.as_deref(), &source.user_id)?;
    let guild_id = source.guild_id.unwrap_or_default();
    let audience_id = if let Some(route) = config
        .routes
        .iter()
        .find(|route| route.channel_id == source.channel_id)
    {
        (route.conversation == DiscordConversation::Channel)
            .then(|| format!("discord-{guild_id}-{}", source.channel_id))
    } else if source.parent_channel_id.as_deref().is_some_and(|parent| {
        config.routes.iter().any(|route| {
            route.channel_id == parent && route.conversation == DiscordConversation::Thread
        })
    }) {
        Some(format!("discord-{guild_id}-{}", source.channel_id))
    } else {
        return Err(InterfaceError {
            status: StatusCode::FORBIDDEN,
            message: "channel is not configured as a Discord route".to_owned(),
        });
    };
    Ok(AdmittedInteraction {
        interaction: Interaction {
            kind: source.kind,
            name: source.name,
            values: source.values,
            options: source.options,
            conversation: audience_id.is_some(),
        },
        audience_id,
    })
}

#[cfg(test)]
pub(crate) mod tests;
