use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc},
    task::JoinHandle,
    time::{sleep, timeout},
};
use tracing::{error, info, warn};
use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use twilight_http::{Client as DiscordHttp, request::channel::reaction::RequestReactionType};
use twilight_model::{
    channel::{ChannelType, message::MessageType},
    id::{
        Id,
        marker::{ChannelMarker, EmojiMarker, MessageMarker},
    },
};
use uuid::Uuid;

use crate::{
    config::{
        DiscordActivation, DiscordConfig, DiscordConversation, DiscordEmoji, LoadedConfig,
        ProviderRole, read_token_file, token_matches,
    },
    model::{
        Delivery, DeliveryAck, DeliveryOutcome, DeliveryReconciliation, EventReceipt,
        ExternalEvent, Health, SimulateRequest, SimulateResponse,
    },
};

#[derive(Debug)]
struct ServiceError {
    status: StatusCode,
    message: String,
}

impl From<anyhow::Error> for ServiceError {
    fn from(error: anyhow::Error) -> Self {
        error!(%error, "component request failed");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

#[derive(Clone)]
struct DiscordState {
    discord: DiscordConfig,
    health: Health,
    core_url: String,
    provider_id: String,
    token: String,
    instance_id: String,
    client: Client,
    participation_path: PathBuf,
    participation: Arc<Mutex<DiscordParticipation>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct DiscordParticipation {
    threads: BTreeMap<String, String>,
    #[serde(default)]
    created_threads: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
struct InboundMessage {
    request: SimulateRequest,
    native: Value,
}

#[derive(Clone, Debug)]
struct MessagePlan {
    event_id: String,
    guild_id: String,
    user_id: String,
    agent_id: String,
    source_channel_id: String,
    source_message_id: String,
    route_channel_id: String,
    current_channel_id: String,
    content: String,
    create_thread: bool,
    adopt_thread: bool,
    native: Value,
}

#[derive(Clone, Debug)]
struct ResolvedMessage {
    event: ExternalEvent,
    conversation_id: String,
}

pub async fn serve_discord(
    config: LoadedConfig,
    token_file: &Path,
    discord_token_file: Option<&Path>,
    deterministic: bool,
) -> Result<()> {
    let discord = config.discord()?.clone();
    let provider = config
        .provider(&discord.provider, ProviderRole::Interface)?
        .clone();
    if !provider.enabled {
        bail!("Discord interface provider is disabled");
    }
    let token = read_token_file(token_file)?;
    if !token_matches(&provider.token_hash, &token) {
        bail!("Discord interface provider token does not match configuration");
    }
    let participation_path = config
        .value
        .data_dir
        .join("components")
        .join(&provider.id)
        .join("participation.json");
    let participation = load_discord_participation(&participation_path)?;
    let state = DiscordState {
        discord,
        health: Health {
            id: provider.id.clone(),
            role: provider.role.as_str().to_owned(),
            version: provider.version.clone(),
            status: "ready".to_owned(),
        },
        core_url: format!("http://{}", config.value.core.listen),
        provider_id: provider.id.clone(),
        token,
        instance_id: Uuid::now_v7().to_string(),
        client: Client::new(),
        participation_path,
        participation: Arc::new(Mutex::new(participation)),
    };
    let reconciled = loop {
        match reconcile_core_deliveries(&state).await {
            Ok(reconciled) => break reconciled,
            Err(failure) => {
                warn!(error = %failure.message, "waiting to reconcile Discord delivery claims");
                sleep(Duration::from_millis(250)).await;
            }
        }
    };
    let router = discord_router(state.clone(), deterministic);
    if deterministic {
        info!(url = %provider.url, deterministic, "Discord interface provider ready");
        return serve_component(provider.url, router, "Discord interface provider").await;
    }

    let discord_token_path = discord_token_file
        .context("--discord-token-file is required outside deterministic mode")?;
    let discord_token = read_token_file(discord_token_path)?;
    let http = Arc::new(DiscordHttp::new(discord_token.clone()));
    let current_user = http.current_user().await?.model().await?;
    if current_user.id.to_string() != state.discord.bot_user_id {
        bail!("configured Discord bot user ID does not match the token");
    }
    for target in reconciled.feedback_target_ids {
        terminal_feedback(&http, &state.discord, &target, Feedback::Uncertain).await;
    }
    let (messages, receiver) = mpsc::channel(64);
    let delivery_state = state.clone();
    let worker_state = state.clone();
    let worker_http = http.clone();
    tokio::spawn(async move {
        message_worker(worker_state, worker_http, receiver).await;
    });
    tokio::spawn(async move {
        delivery_loop(delivery_state, http).await;
    });

    info!(url = %provider.url, deterministic, "Discord interface provider ready");
    tokio::select! {
        result = serve_component(provider.url, router, "Discord interface provider") => result,
        result = gateway_loop(state, discord_token, messages) => {
            result.context("Discord gateway stopped")
        }
    }
}

fn discord_router(state: DiscordState, deterministic: bool) -> Router {
    let router = Router::new().route("/health", get(discord_health));
    if deterministic {
        router.route("/v1/simulate", post(simulate))
    } else {
        router
    }
    .with_state(state)
}

fn load_discord_participation(path: &Path) -> Result<DiscordParticipation> {
    if !path.exists() {
        return Ok(DiscordParticipation::default());
    }
    serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("parse Discord participation state {}", path.display()))
}

async fn discord_health(State(state): State<DiscordState>) -> Result<Json<Health>, ServiceError> {
    Ok(Json(state.health))
}

async fn simulate(
    State(state): State<DiscordState>,
    Json(request): Json<SimulateRequest>,
) -> Result<Response, ServiceError> {
    adapter_identity_admit(
        &state.discord,
        request.guild_id.as_deref(),
        &request.user_id,
    )?;
    let Some(plan) = resolve_message(
        &state.discord,
        request,
        Value::String("deterministic-local-discord".to_owned()),
        &*state.participation.lock().await,
    )?
    else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let resolved = prepare_deterministic_conversation(&state, plan).await?;
    let mut feedback = Vec::new();
    if state.discord.reactions {
        feedback.push("processing".to_owned());
    }
    let receipt = submit_event(&state, &resolved.event).await?;
    let delivery = if receipt.duplicate {
        None
    } else {
        let delivery = timeout(Duration::from_secs(10), async {
            loop {
                if let Some(delivery) = claim_core_delivery(&state).await? {
                    if delivery.turn_id != receipt.turn_id {
                        return Err(ServiceError {
                            status: StatusCode::CONFLICT,
                            message: "deterministic delivery order was violated".to_owned(),
                        });
                    }
                    return Ok(delivery);
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| ServiceError {
            status: StatusCode::GATEWAY_TIMEOUT,
            message: "timed out waiting for deterministic delivery".to_owned(),
        })??;
        acknowledge_core_delivery(
            &state,
            delivery.id,
            DeliveryOutcome::Delivered,
            Some(format!("local:{}", resolved.event.event_id)),
        )
        .await?;
        if state.discord.reactions {
            feedback.push("delivered".to_owned());
        }
        Some(delivery)
    };
    Ok(Json(SimulateResponse {
        receipt,
        delivery,
        conversation_id: resolved.conversation_id,
        feedback,
    })
    .into_response())
}

async fn gateway_loop(
    state: DiscordState,
    token: String,
    messages: mpsc::Sender<InboundMessage>,
) -> Result<()> {
    let intents = Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT;
    let mut shard = Shard::new(ShardId::ONE, token, intents);
    let event_types = EventTypeFlags::MESSAGE_CREATE;
    while let Some(event) = shard.next_event(event_types).await {
        if let Event::MessageCreate(message) = event.context("receive Discord gateway event")? {
            if message.author.bot || message.kind == MessageType::ThreadStarterMessage {
                continue;
            }
            let guild = message.guild_id.map(|id| id.to_string());
            if adapter_identity_admit(
                &state.discord,
                guild.as_deref(),
                &message.author.id.to_string(),
            )
            .is_err()
            {
                continue;
            }
            let request = SimulateRequest {
                event_id: message.id.to_string(),
                guild_id: guild,
                user_id: message.author.id.to_string(),
                channel_id: message.channel_id.to_string(),
                parent_channel_id: None,
                content: message.content.clone(),
            };
            let native = serde_json::to_value(&*message).unwrap_or(Value::Null);
            if messages
                .send(InboundMessage { request, native })
                .await
                .is_err()
            {
                bail!("Discord message worker stopped");
            }
        }
    }
    bail!("Discord gateway event stream ended")
}

fn resolve_message(
    config: &DiscordConfig,
    request: SimulateRequest,
    native: Value,
    participation: &DiscordParticipation,
) -> Result<Option<MessagePlan>, ServiceError> {
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
        return Err(ServiceError {
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
        event_id: request.event_id.clone(),
        guild_id: request.guild_id.unwrap_or_default(),
        user_id: request.user_id,
        agent_id: config.agent.clone(),
        source_channel_id: request.channel_id.clone(),
        source_message_id: request.event_id,
        route_channel_id: route.channel_id.clone(),
        current_channel_id: request.channel_id,
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

async fn prepare_deterministic_conversation(
    state: &DiscordState,
    plan: MessagePlan,
) -> Result<ResolvedMessage, ServiceError> {
    let target = if plan.create_thread {
        plan.source_message_id.clone()
    } else {
        plan.current_channel_id.clone()
    };
    if plan.create_thread || plan.adopt_thread {
        adopt_thread(state, &target, &plan.route_channel_id).await?;
    }
    Ok(resolved_message(plan, target))
}

async fn prepare_discord_conversation(
    state: &DiscordState,
    http: &DiscordHttp,
    plan: MessagePlan,
) -> Result<ResolvedMessage> {
    let target = if plan.create_thread {
        if let Some(thread) = state
            .participation
            .lock()
            .await
            .created_threads
            .get(&plan.source_message_id)
            .cloned()
        {
            thread
        } else {
            let channel = plan
                .source_channel_id
                .parse::<Id<ChannelMarker>>()
                .context("source Discord channel ID is invalid")?;
            let message = plan
                .source_message_id
                .parse::<Id<MessageMarker>>()
                .context("source Discord message ID is invalid")?;
            let thread = http
                .create_thread_from_message(channel, message, &thread_title(&plan.content))
                .await
                .context("create Discord conversation thread")?
                .model()
                .await
                .context("decode Discord conversation thread")?
                .id
                .to_string();
            remember_created_thread(
                state,
                &plan.source_message_id,
                &thread,
                &plan.route_channel_id,
            )
            .await?;
            thread
        }
    } else {
        plan.current_channel_id.clone()
    };
    if plan.adopt_thread {
        adopt_thread(state, &target, &plan.route_channel_id)
            .await
            .map_err(|failure| anyhow::anyhow!(failure.message))?;
    }
    Ok(resolved_message(plan, target))
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
    ResolvedMessage {
        event: ExternalEvent {
            event_id: plan.event_id,
            agent_id: plan.agent_id,
            user_id: plan.user_id,
            audience_id: conversation_id.clone(),
            delivery_target_id: target,
            feedback_target_id: format!("{}:{}", plan.source_channel_id, plan.source_message_id),
            content: plan.content,
            native: json!({
                "discord": plan.native,
                "guild_id": plan.guild_id,
                "route_channel_id": plan.route_channel_id,
            }),
        },
        conversation_id,
    }
}

async fn remember_created_thread(
    state: &DiscordState,
    message_id: &str,
    thread_id: &str,
    route_channel_id: &str,
) -> Result<()> {
    let mut participation = state.participation.lock().await;
    participation
        .created_threads
        .insert(message_id.to_owned(), thread_id.to_owned());
    participation
        .threads
        .insert(thread_id.to_owned(), route_channel_id.to_owned());
    persist_json(&state.participation_path, &*participation)
}

async fn adopt_thread(
    state: &DiscordState,
    thread_id: &str,
    route_channel_id: &str,
) -> Result<(), ServiceError> {
    let mut participation = state.participation.lock().await;
    if participation
        .threads
        .get(thread_id)
        .is_some_and(|parent| parent == route_channel_id)
    {
        return Ok(());
    }
    participation
        .threads
        .insert(thread_id.to_owned(), route_channel_id.to_owned());
    persist_json(&state.participation_path, &*participation)?;
    Ok(())
}

async fn discord_parent_channel(
    http: &DiscordHttp,
    config: &DiscordConfig,
    channel_id: &str,
) -> Result<Option<String>> {
    if config
        .routes
        .iter()
        .any(|route| route.channel_id == channel_id)
    {
        return Ok(None);
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

async fn message_worker(
    state: DiscordState,
    http: Arc<DiscordHttp>,
    mut messages: mpsc::Receiver<InboundMessage>,
) {
    while let Some(mut inbound) = messages.recv().await {
        let discord = &state.discord;
        if adapter_identity_admit(
            discord,
            inbound.request.guild_id.as_deref(),
            &inbound.request.user_id,
        )
        .is_err()
        {
            continue;
        }
        inbound.request.parent_channel_id =
            match discord_parent_channel(&http, discord, &inbound.request.channel_id).await {
                Ok(parent) => parent,
                Err(failure) => {
                    warn!(error = %failure, "resolve Discord message route");
                    continue;
                }
            };
        let Ok(Some(plan)) = resolve_message(
            discord,
            inbound.request,
            inbound.native,
            &*state.participation.lock().await,
        ) else {
            continue;
        };
        let source = format!("{}:{}", plan.source_channel_id, plan.source_message_id);
        let resolved = match prepare_discord_conversation(&state, &http, plan).await {
            Ok(resolved) => resolved,
            Err(failure) => {
                terminal_feedback(&http, discord, &source, Feedback::Failed).await;
                warn!(error = %failure, "Discord conversation resolution failed");
                continue;
            }
        };
        processing_feedback(&http, discord, &resolved.event.feedback_target_id).await;
        let typing = start_typing(http.clone(), discord, &resolved.event.delivery_target_id).await;
        let receipt = submit_event(&state, &resolved.event).await;
        if let Some(task) = typing {
            task.abort();
        }
        match receipt {
            Ok(receipt) if receipt.duplicate => {
                clear_processing_feedback(&http, discord, &resolved.event.feedback_target_id).await;
            }
            Ok(_) => {}
            Err(failure) => {
                warn!(error = %failure.message, "Discord event submission failed");
                terminal_feedback(
                    &http,
                    discord,
                    &resolved.event.feedback_target_id,
                    Feedback::Failed,
                )
                .await;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Feedback {
    Delivered,
    Failed,
    Uncertain,
}

async fn processing_feedback(http: &DiscordHttp, config: &DiscordConfig, target: &str) {
    if config.reactions
        && let Err(failure) = create_feedback_reaction(http, target, &config.processing_emoji).await
    {
        warn!(error = %failure, "Discord processing reaction failed");
    }
}

async fn clear_processing_feedback(http: &DiscordHttp, config: &DiscordConfig, target: &str) {
    if config.reactions
        && let Err(failure) = delete_feedback_reaction(http, target, &config.processing_emoji).await
    {
        warn!(error = %failure, "Discord processing reaction removal failed");
    }
}

async fn terminal_feedback(
    http: &DiscordHttp,
    config: &DiscordConfig,
    target: &str,
    feedback: Feedback,
) {
    if !config.reactions {
        return;
    }
    clear_processing_feedback(http, config, target).await;
    let emoji = match feedback {
        Feedback::Delivered => &config.delivered_emoji,
        Feedback::Failed => &config.failed_emoji,
        Feedback::Uncertain => &config.uncertain_emoji,
    };
    if let Err(failure) = create_feedback_reaction(http, target, emoji).await {
        warn!(error = %failure, "Discord terminal reaction failed");
    }
}

async fn create_feedback_reaction(
    http: &DiscordHttp,
    target: &str,
    configured: &str,
) -> Result<()> {
    let (channel, message) = feedback_ids(target)?;
    match DiscordEmoji::parse(configured)? {
        DiscordEmoji::Unicode(name) => {
            http.create_reaction(
                channel,
                message,
                &RequestReactionType::Unicode { name: &name },
            )
            .await?;
        }
        DiscordEmoji::Custom { name, id } => {
            http.create_reaction(
                channel,
                message,
                &RequestReactionType::Custom {
                    id: Id::<EmojiMarker>::new(id),
                    name: Some(&name),
                },
            )
            .await?;
        }
    }
    Ok(())
}

async fn delete_feedback_reaction(
    http: &DiscordHttp,
    target: &str,
    configured: &str,
) -> Result<()> {
    let (channel, message) = feedback_ids(target)?;
    match DiscordEmoji::parse(configured)? {
        DiscordEmoji::Unicode(name) => {
            http.delete_current_user_reaction(
                channel,
                message,
                &RequestReactionType::Unicode { name: &name },
            )
            .await?;
        }
        DiscordEmoji::Custom { name, id } => {
            http.delete_current_user_reaction(
                channel,
                message,
                &RequestReactionType::Custom {
                    id: Id::<EmojiMarker>::new(id),
                    name: Some(&name),
                },
            )
            .await?;
        }
    }
    Ok(())
}

fn feedback_ids(target: &str) -> Result<(Id<ChannelMarker>, Id<MessageMarker>)> {
    let (channel, message) = target
        .split_once(':')
        .context("Discord feedback target is invalid")?;
    Ok((
        channel
            .parse()
            .context("Discord feedback channel is invalid")?,
        message
            .parse()
            .context("Discord feedback message is invalid")?,
    ))
}

async fn start_typing(
    http: Arc<DiscordHttp>,
    config: &DiscordConfig,
    target: &str,
) -> Option<JoinHandle<()>> {
    if !config.typing {
        return None;
    }
    let channel = match target.parse::<Id<ChannelMarker>>() {
        Ok(channel) => channel,
        Err(failure) => {
            warn!(error = %failure, "Discord typing target is invalid");
            return None;
        }
    };
    if let Err(failure) = http.create_typing_trigger(channel).await {
        warn!(error = %failure, "Discord typing trigger failed");
    }
    Some(tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(8)).await;
            if let Err(failure) = http.create_typing_trigger(channel).await {
                warn!(error = %failure, "Discord typing refresh failed");
            }
        }
    }))
}

async fn delivery_loop(state: DiscordState, http: Arc<DiscordHttp>) {
    loop {
        match claim_core_delivery(&state).await {
            Ok(Some(delivery)) => {
                let target = delivery.target_id.parse::<Id<ChannelMarker>>();
                let (outcome, native_ref, feedback) = match target {
                    Ok(channel_id) => {
                        match send_discord_message(&http, channel_id, &delivery.content).await {
                            Ok(native_ref) => (
                                DeliveryOutcome::Delivered,
                                Some(native_ref),
                                Feedback::Delivered,
                            ),
                            Err(failure) => {
                                warn!(error = %failure, delivery_id = delivery.id, "Discord delivery outcome uncertain");
                                (DeliveryOutcome::Uncertain, None, Feedback::Uncertain)
                            }
                        }
                    }
                    Err(failure) => {
                        warn!(error = %failure, "invalid Discord delivery target");
                        (DeliveryOutcome::Uncertain, None, Feedback::Uncertain)
                    }
                };
                if let Err(failure) =
                    acknowledge_core_delivery(&state, delivery.id, outcome, native_ref).await
                {
                    warn!(error = %failure.message, "delivery acknowledgement failed");
                }
                terminal_feedback(
                    &http,
                    &state.discord,
                    &delivery.feedback_target_id,
                    feedback,
                )
                .await;
            }
            Ok(None) => sleep(Duration::from_millis(500)).await,
            Err(failure) => {
                warn!(error = %failure.message, "delivery poll failed");
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn send_discord_message(
    http: &DiscordHttp,
    channel_id: Id<ChannelMarker>,
    content: &str,
) -> Result<String> {
    let mut ids = Vec::new();
    for chunk in discord_chunks(content) {
        let message = http.create_message(channel_id).content(&chunk).await?;
        ids.push(message.model().await?.id.to_string());
    }
    Ok(ids.join(","))
}

fn discord_chunks(content: &str) -> Vec<String> {
    const LIMIT: usize = 2_000;
    let mut remaining = content.trim();
    let mut chunks = Vec::new();
    while remaining.chars().count() > LIMIT {
        let end = remaining
            .char_indices()
            .nth(LIMIT)
            .map_or(remaining.len(), |(index, _)| index);
        let candidate = &remaining[..end];
        let split = candidate
            .rfind("\n\n")
            .or_else(|| candidate.rfind('\n'))
            .or_else(|| candidate.rfind(char::is_whitespace))
            .filter(|index| *index > 0)
            .unwrap_or(end);
        chunks.push(remaining[..split].trim_end().to_owned());
        remaining = remaining[split..].trim_start();
    }
    if !remaining.is_empty() {
        chunks.push(remaining.to_owned());
    }
    chunks
}

async fn submit_event(
    state: &DiscordState,
    event: &ExternalEvent,
) -> Result<EventReceipt, ServiceError> {
    Ok(state
        .client
        .post(format!("{}/v1/events", state.core_url))
        .bearer_auth(&state.token)
        .header("x-carillon-provider", &state.provider_id)
        .json(event)
        .send()
        .await
        .context("submit event to core")?
        .error_for_status()
        .context("core rejected event")?
        .json()
        .await
        .context("decode event receipt")?)
}

async fn claim_core_delivery(state: &DiscordState) -> Result<Option<Delivery>, ServiceError> {
    Ok(state
        .client
        .get(format!("{}/v1/deliveries", state.core_url))
        .bearer_auth(&state.token)
        .header("x-carillon-provider", &state.provider_id)
        .header("x-carillon-instance", &state.instance_id)
        .send()
        .await
        .context("poll core delivery")?
        .error_for_status()
        .context("core rejected delivery poll")?
        .json()
        .await
        .context("decode core delivery")?)
}

async fn acknowledge_core_delivery(
    state: &DiscordState,
    id: i64,
    outcome: DeliveryOutcome,
    native_ref: Option<String>,
) -> Result<(), ServiceError> {
    state
        .client
        .post(format!("{}/v1/deliveries/{id}/ack", state.core_url))
        .bearer_auth(&state.token)
        .header("x-carillon-provider", &state.provider_id)
        .header("x-carillon-instance", &state.instance_id)
        .json(&DeliveryAck {
            native_ref,
            outcome,
        })
        .send()
        .await
        .context("acknowledge core delivery")?
        .error_for_status()
        .context("core rejected delivery acknowledgement")?;
    Ok(())
}

async fn reconcile_core_deliveries(
    state: &DiscordState,
) -> Result<DeliveryReconciliation, ServiceError> {
    Ok(state
        .client
        .post(format!("{}/v1/deliveries/reconcile", state.core_url))
        .bearer_auth(&state.token)
        .header("x-carillon-provider", &state.provider_id)
        .header("x-carillon-instance", &state.instance_id)
        .send()
        .await
        .context("reconcile abandoned delivery claims")?
        .error_for_status()
        .context("core rejected delivery reconciliation")?
        .json()
        .await
        .context("decode delivery reconciliation")?)
}

fn adapter_identity_admit(
    config: &DiscordConfig,
    guild_id: Option<&str>,
    user_id: &str,
) -> Result<(), ServiceError> {
    let guild = guild_id.ok_or_else(|| ServiceError {
        status: StatusCode::FORBIDDEN,
        message: "direct messages are not accepted".to_owned(),
    })?;
    if guild != config.guild_id {
        return Err(ServiceError {
            status: StatusCode::FORBIDDEN,
            message: "guild is not configured".to_owned(),
        });
    }
    if !config
        .allowed_user_ids
        .iter()
        .any(|allowed| allowed == user_id)
    {
        return Err(ServiceError {
            status: StatusCode::FORBIDDEN,
            message: "user is not allowlisted".to_owned(),
        });
    }
    Ok(())
}

async fn serve_component(url: String, router: Router, name: &str) -> Result<()> {
    let address = url
        .strip_prefix("http://")
        .context("component URL must use http://")?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("bind {name} to {address}"))?;
    info!(%address, component = name, "component ready");
    axum::serve(listener, router).await?;
    Ok(())
}

fn persist_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

    use anyhow::{Context as _, anyhow};
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode, Uri},
    };
    use serde_json::Value;
    use tokio::sync::Mutex;
    use tower::ServiceExt as _;

    use super::{
        DiscordParticipation, DiscordState, MessagePlan, ServiceError, discord_chunks,
        discord_router, resolve_message, resolved_message, thread_title,
    };
    use crate::{
        config::{DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute},
        model::{Health, SimulateRequest},
    };

    #[tokio::test]
    async fn production_discord_does_not_expose_simulation() -> anyhow::Result<()> {
        let state = DiscordState {
            discord: discord_config(DiscordActivation::Always, DiscordConversation::Channel),
            health: Health {
                id: "discord".to_owned(),
                role: "interface".to_owned(),
                version: "test".to_owned(),
                status: "ready".to_owned(),
            },
            core_url: "http://127.0.0.1:1".to_owned(),
            provider_id: "discord".to_owned(),
            token: "token".to_owned(),
            instance_id: "instance".to_owned(),
            client: reqwest::Client::new(),
            participation_path: PathBuf::from("unused"),
            participation: Arc::new(Mutex::new(DiscordParticipation::default())),
        };
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::POST;
        *request.uri_mut() = Uri::from_static("/v1/simulate");
        let response = discord_router(state, false).oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[test]
    fn discord_messages_split_cleanly_at_the_platform_limit() {
        let content = format!("{}\n\n{}", "a".repeat(1_990), "b".repeat(30));
        let chunks = discord_chunks(&content);
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks.concat(),
            format!("{}{}", "a".repeat(1_990), "b".repeat(30))
        );
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 2_000));

        let unbroken = discord_chunks(&"🦀".repeat(2_001));
        assert_eq!(unbroken.len(), 2);
        assert_eq!(unbroken[0].chars().count(), 2_000);
        assert_eq!(unbroken[1], "🦀");
    }

    #[test]
    fn thread_titles_are_deterministic_and_bounded() {
        let title = thread_title(
            "  Build   a production-ready Discord ingress boundary with a deliberately overlong title that must be trimmed safely  ",
        );
        assert_eq!(title.chars().count(), 80);
        assert_eq!(
            title,
            "Build a production-ready Discord ingress boundary with a deliberately overlong t"
        );
    }

    #[test]
    fn mention_activation_strips_only_the_configured_bot() -> anyhow::Result<()> {
        let config = discord_config(DiscordActivation::Mention, DiscordConversation::Thread);
        let ignored = resolve_message(
            &config,
            request("300", None, "hello"),
            Value::Null,
            &DiscordParticipation::default(),
        )
        .map_err(|failure| anyhow!(failure.message))?;
        assert!(ignored.is_none());

        let plan = accepted(resolve_message(
            &config,
            request("300", None, "<@201> hello <@999>"),
            Value::Null,
            &DiscordParticipation::default(),
        ))?;
        assert_eq!(plan.content, "hello <@999>");
        let resolved = resolved_message(plan, "400".to_owned());
        assert_eq!(resolved.conversation_id, "discord-100-400");
        assert_eq!(resolved.event.delivery_target_id, "400");
        Ok(())
    }

    #[test]
    fn mention_thread_adoption_persists_activation_for_later_messages() -> anyhow::Result<()> {
        let config = discord_config(DiscordActivation::Mention, DiscordConversation::Thread);
        let adoption = accepted(resolve_message(
            &config,
            request("400", Some("300"), "<@201> adopt"),
            Value::Null,
            &DiscordParticipation::default(),
        ))?;
        assert!(adoption.adopt_thread);

        let participation = DiscordParticipation {
            threads: BTreeMap::from([("400".to_owned(), "300".to_owned())]),
            ..DiscordParticipation::default()
        };
        let continuation = accepted(resolve_message(
            &config,
            request("400", Some("300"), "continue"),
            Value::Null,
            &participation,
        ))?;
        assert!(!continuation.adopt_thread);
        assert_eq!(
            resolved_message(continuation, "400".to_owned()).conversation_id,
            "discord-100-400"
        );
        Ok(())
    }

    #[test]
    fn always_thread_accepts_children_but_channel_route_does_not() -> anyhow::Result<()> {
        let thread = discord_config(DiscordActivation::Always, DiscordConversation::Thread);
        let thread_message = accepted(resolve_message(
            &thread,
            request("400", Some("300"), "continue"),
            Value::Null,
            &DiscordParticipation::default(),
        ))?;
        assert!(!thread_message.create_thread);

        let channel = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
        assert!(
            resolve_message(
                &channel,
                request("400", Some("300"), "must reject"),
                Value::Null,
                &DiscordParticipation::default(),
            )
            .is_err()
        );
        let first = accepted(resolve_message(
            &channel,
            request("300", None, "first"),
            Value::Null,
            &DiscordParticipation::default(),
        ))?;
        let second = accepted(resolve_message(
            &channel,
            request("300", None, "second"),
            Value::Null,
            &DiscordParticipation::default(),
        ))?;
        assert_eq!(
            resolved_message(first, "300".to_owned()).conversation_id,
            resolved_message(second, "300".to_owned()).conversation_id
        );
        Ok(())
    }

    fn discord_config(
        activation: DiscordActivation,
        conversation: DiscordConversation,
    ) -> DiscordConfig {
        DiscordConfig {
            provider: "discord".to_owned(),
            agent: "default".to_owned(),
            guild_id: "100".to_owned(),
            bot_user_id: "201".to_owned(),
            allowed_user_ids: vec!["200".to_owned()],
            routes: vec![DiscordRoute {
                channel_id: "300".to_owned(),
                activation,
                conversation,
            }],
            typing: true,
            reactions: true,
            processing_emoji: "👀".to_owned(),
            delivered_emoji: "✅".to_owned(),
            failed_emoji: "❌".to_owned(),
            uncertain_emoji: "⚠️".to_owned(),
        }
    }

    fn accepted(
        resolution: std::result::Result<Option<MessagePlan>, ServiceError>,
    ) -> anyhow::Result<MessagePlan> {
        resolution
            .map_err(|failure| anyhow!(failure.message))?
            .context("message was unexpectedly ignored")
    }

    fn request(
        channel_id: &str,
        parent_channel_id: Option<&str>,
        content: &str,
    ) -> SimulateRequest {
        SimulateRequest {
            event_id: "400".to_owned(),
            guild_id: Some("100".to_owned()),
            user_id: "200".to_owned(),
            channel_id: channel_id.to_owned(),
            parent_channel_id: parent_channel_id.map(str::to_owned),
            content: content.to_owned(),
        }
    }
}
