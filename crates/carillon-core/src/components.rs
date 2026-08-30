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
    http::{HeaderMap, StatusCode},
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
    time::sleep,
};
use tracing::{error, info, warn};
use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use twilight_http::{Client as DiscordHttp, request::channel::reaction::RequestReactionType};
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::{
        ChannelType,
        message::{
            Component, MessageFlags, MessageType,
            component::{
                ActionRow, Button, ButtonStyle, SelectMenu, SelectMenuOption, SelectMenuType,
            },
        },
    },
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{
        Id,
        marker::{ChannelMarker, EmojiMarker, GuildMarker, MessageMarker},
    },
};
use uuid::Uuid;

use crate::{
    config::{
        DiscordActivation, DiscordConfig, DiscordConversation, DiscordEmoji, LoadedConfig,
        read_token_file, token_matches,
    },
    model::{
        CapabilityRequest, CapabilityResponse, Delivery, DeliveryAck, DeliveryOutcome,
        DeliveryReconciliation, EventReceipt, ExecutionProfileApplication, ExecutionProfileScope,
        ExecutionProfileSelection, ExtensionRequest, ExtensionResponse, ExternalEvent, Health,
        RuntimeExecutionProfileModel, RuntimeExecutionProfiles, SimulateRequest, SimulateResponse,
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
struct ExtensionState {
    token_hash: String,
    id: String,
    version: String,
}

pub async fn serve_extension(config: LoadedConfig, token_file: &Path) -> Result<()> {
    let component = config.value.interface_extension;
    let token = read_token_file(token_file)?;
    if !token_matches(&component.token_hash, &token) {
        bail!("interface extension token does not match configuration");
    }
    let state = ExtensionState {
        token_hash: component.token_hash,
        id: component.id,
        version: component.version,
    };
    let router = Router::new()
        .route("/health", get(extension_health))
        .route("/v1/invoke", post(invoke_extension))
        .with_state(state);
    serve_component(component.url, router, "interface extension").await
}

async fn extension_health(State(state): State<ExtensionState>) -> Json<Health> {
    Json(Health {
        id: state.id,
        role: "interface-extension".to_owned(),
        version: state.version,
        status: "ready".to_owned(),
    })
}

async fn invoke_extension(
    State(state): State<ExtensionState>,
    headers: HeaderMap,
    Json(request): Json<ExtensionRequest>,
) -> Result<Json<ExtensionResponse>, ServiceError> {
    authorize(&headers, &state.token_hash)?;
    if request.command != "carillon" {
        return Err(ServiceError {
            status: StatusCode::NOT_FOUND,
            message: "unknown command".to_owned(),
        });
    }
    let detail = if request.arguments.is_empty() {
        "ready"
    } else {
        request.arguments.as_str()
    };
    Ok(Json(ExtensionResponse {
        content: format!(
            "Carillon {} ({detail}); guild {}; operator {}",
            state.version, request.guild_id, request.user_id
        ),
        native: json!({ "command": request.command, "arguments": request.arguments, "extension": state.id }),
    }))
}

#[derive(Clone)]
struct CapabilityState {
    token_hash: String,
    id: String,
    version: String,
    path: PathBuf,
    evidence: Arc<Mutex<BTreeMap<String, CapabilityResponse>>>,
}

pub async fn serve_capability(config: LoadedConfig, token_file: &Path) -> Result<()> {
    let component = config.value.capability;
    let token = read_token_file(token_file)?;
    if !token_matches(&component.token_hash, &token) {
        bail!("capability token does not match configuration");
    }
    let state_dir = config.value.data_dir.join("components").join(&component.id);
    fs::create_dir_all(&state_dir)?;
    let path = state_dir.join("evidence.json");
    let evidence = if path.exists() {
        serde_json::from_slice(&fs::read(&path)?)?
    } else {
        BTreeMap::new()
    };
    let state = CapabilityState {
        token_hash: component.token_hash,
        id: component.id,
        version: component.version,
        path,
        evidence: Arc::new(Mutex::new(evidence)),
    };
    let router = Router::new()
        .route("/health", get(capability_health))
        .route("/v1/execute", post(execute_capability))
        .with_state(state);
    serve_component(component.url, router, "capability").await
}

async fn capability_health(State(state): State<CapabilityState>) -> Json<Health> {
    Json(Health {
        id: state.id,
        role: "capability".to_owned(),
        version: state.version,
        status: "ready".to_owned(),
    })
}

async fn execute_capability(
    State(state): State<CapabilityState>,
    headers: HeaderMap,
    Json(request): Json<CapabilityRequest>,
) -> Result<Json<CapabilityResponse>, ServiceError> {
    authorize(&headers, &state.token_hash)?;
    if request.operation != "echo" {
        return Err(ServiceError {
            status: StatusCode::BAD_REQUEST,
            message: "unsupported capability operation".to_owned(),
        });
    }
    let mut evidence = state.evidence.lock().await;
    if let Some(existing) = evidence.get(&request.correlation_id) {
        return Ok(Json(existing.clone()));
    }
    let response = CapabilityResponse {
        output: format!("external echo: {}", request.input),
        native: json!({ "component": state.id, "operation": request.operation, "sequence": evidence.len() + 1 }),
    };
    evidence.insert(request.correlation_id, response.clone());
    persist_json(&state.path, &*evidence)?;
    Ok(Json(response))
}

#[derive(Clone)]
struct DiscordState {
    config_path: PathBuf,
    core_url: String,
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
    let token = read_token_file(token_file)?;
    if !token_matches(&config.value.discord.token_hash, &token) {
        bail!("Discord adapter token does not match configuration");
    }
    let participation_path = config
        .value
        .data_dir
        .join("components/discord/participation.json");
    let participation = if participation_path.exists() {
        serde_json::from_slice(&fs::read(&participation_path)?)
            .context("parse Discord participation state")?
    } else {
        DiscordParticipation::default()
    };
    let state = DiscordState {
        config_path: config.path.clone(),
        core_url: format!("http://{}", config.value.core.listen),
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
    let router = Router::new()
        .route("/health", get(discord_health))
        .route("/v1/simulate", post(simulate))
        .with_state(state.clone());
    if !deterministic {
        let discord_token_path = discord_token_file
            .context("--discord-token-file is required outside deterministic mode")?;
        let discord_token = read_token_file(discord_token_path)?;
        let http = Arc::new(DiscordHttp::new(discord_token.clone()));
        let current_user = http.current_user().await?.model().await?;
        if current_user.id.to_string() != config.value.discord.bot_user_id {
            bail!("configured Discord bot user ID does not match the token");
        }
        for target in reconciled.feedback_target_ids {
            terminal_feedback(&http, &config.value.discord, &target, Feedback::Uncertain).await;
        }
        let application_id = http.current_user_application().await?.model().await?.id;
        let guild_id = config
            .value
            .discord
            .guild_id
            .parse::<Id<GuildMarker>>()
            .context("configured Discord guild ID is invalid")?;
        http.interaction(application_id)
            .create_guild_command(guild_id)
            .chat_input("model", "Choose provider, model, and supported settings")
            .await?;
        let (messages, receiver) = mpsc::channel(64);
        let gateway_state = state.clone();
        let gateway_http = http.clone();
        let delivery_state = state.clone();
        let worker_state = state.clone();
        let worker_http = http.clone();
        tokio::spawn(async move {
            if let Err(failure) = Box::pin(gateway_loop(
                gateway_state,
                discord_token,
                gateway_http,
                messages,
            ))
            .await
            {
                error!(error = %failure, "Discord gateway stopped");
            }
        });
        tokio::spawn(async move {
            message_worker(worker_state, worker_http, receiver).await;
        });
        tokio::spawn(async move {
            delivery_loop(delivery_state, http).await;
        });
    }
    let listener = TcpListener::bind(config.value.discord.listen).await?;
    info!(listen = %config.value.discord.listen, deterministic, "Discord adapter ready");
    axum::serve(listener, router).await?;
    Ok(())
}

async fn discord_health(State(state): State<DiscordState>) -> Result<Json<Health>, ServiceError> {
    let config = LoadedConfig::load(&state.config_path)?;
    Ok(Json(Health {
        id: config.value.discord.id,
        role: "interface-adapter".to_owned(),
        version: config.value.discord.version,
        status: "ready".to_owned(),
    }))
}

async fn simulate(
    State(state): State<DiscordState>,
    Json(request): Json<SimulateRequest>,
) -> Result<Response, ServiceError> {
    let config = LoadedConfig::load(&state.config_path)?;
    adapter_identity_admit(
        &config.value.discord,
        request.guild_id.as_deref(),
        &request.user_id,
    )?;
    let Some(plan) = resolve_message(
        &config.value.discord,
        request,
        Value::String("deterministic-local-discord".to_owned()),
        &*state.participation.lock().await,
    )?
    else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let resolved = prepare_deterministic_conversation(&state, plan).await?;
    let mut feedback = Vec::new();
    if config.value.discord.reactions {
        feedback.push("processing".to_owned());
    }
    let receipt = submit_event(&state, &resolved.event).await?;
    let delivery = if receipt.state == "completed" && !receipt.duplicate {
        let delivery = claim_core_delivery(&state).await?;
        if let Some(value) = &delivery {
            acknowledge_core_delivery(
                &state,
                value.id,
                DeliveryOutcome::Delivered,
                Some(format!("local:{}", resolved.event.event_id)),
            )
            .await?;
            if config.value.discord.reactions {
                feedback.push("delivered".to_owned());
            }
        }
        delivery
    } else {
        if receipt.state == "interrupted" && config.value.discord.reactions {
            feedback.push("failed".to_owned());
        }
        None
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
    http: Arc<DiscordHttp>,
    messages: mpsc::Sender<InboundMessage>,
) -> Result<()> {
    let intents = Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT;
    let mut shard = Shard::new(ShardId::ONE, token, intents);
    let event_types = EventTypeFlags::MESSAGE_CREATE | EventTypeFlags::INTERACTION_CREATE;
    while let Some(event) = shard.next_event(event_types).await {
        match event.context("receive Discord gateway event")? {
            Event::MessageCreate(message) => {
                if message.author.bot || message.kind == MessageType::ThreadStarterMessage {
                    continue;
                }
                let config = LoadedConfig::load(&state.config_path)?;
                let guild = message.guild_id.map(|id| id.to_string());
                if adapter_identity_admit(
                    &config.value.discord,
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
            Event::InteractionCreate(interaction) => {
                let interaction_state = state.clone();
                let interaction_http = http.clone();
                tokio::spawn(async move {
                    if let Err(failure) = handle_model_interaction(
                        &interaction_state,
                        &interaction_http,
                        interaction.0,
                    )
                    .await
                    {
                        warn!(error = %failure, "Discord model interaction failed");
                    }
                });
            }
            _ => {}
        }
    }
    Ok(())
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
            guild_id: Some(plan.guild_id),
            user_id: plan.user_id,
            audience_id: conversation_id.clone(),
            delivery_target_id: target,
            feedback_target_id: format!("{}:{}", plan.source_channel_id, plan.source_message_id),
            content: plan.content,
            native: json!({
                "discord": plan.native,
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
        let config = match LoadedConfig::load(&state.config_path) {
            Ok(config) => config,
            Err(failure) => {
                warn!(error = %failure, "reload Discord configuration for received message");
                continue;
            }
        };
        if adapter_identity_admit(
            &config.value.discord,
            inbound.request.guild_id.as_deref(),
            &inbound.request.user_id,
        )
        .is_err()
        {
            continue;
        }
        inbound.request.parent_channel_id =
            match discord_parent_channel(&http, &config.value.discord, &inbound.request.channel_id)
                .await
            {
                Ok(parent) => parent,
                Err(failure) => {
                    warn!(error = %failure, "resolve Discord message route");
                    continue;
                }
            };
        let Ok(Some(plan)) = resolve_message(
            &config.value.discord,
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
                terminal_feedback(&http, &config.value.discord, &source, Feedback::Failed).await;
                warn!(error = %failure, "Discord conversation resolution failed");
                continue;
            }
        };
        processing_feedback(
            &http,
            &config.value.discord,
            &resolved.event.feedback_target_id,
        )
        .await;
        let typing = start_typing(
            http.clone(),
            &config.value.discord,
            &resolved.event.delivery_target_id,
        )
        .await;
        let receipt = submit_event(&state, &resolved.event).await;
        if let Some(task) = typing {
            task.abort();
        }
        match receipt {
            Ok(receipt) if receipt.duplicate => {
                clear_processing_feedback(
                    &http,
                    &config.value.discord,
                    &resolved.event.feedback_target_id,
                )
                .await;
            }
            Ok(receipt) if receipt.state == "interrupted" => {
                terminal_feedback(
                    &http,
                    &config.value.discord,
                    &resolved.event.feedback_target_id,
                    Feedback::Failed,
                )
                .await;
            }
            Ok(_) => {}
            Err(failure) => {
                warn!(error = %failure.message, "Discord event submission failed");
                terminal_feedback(
                    &http,
                    &config.value.discord,
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

const PROFILE_PREFIX: &str = "carillon:profile:";
const PROVIDER_SELECT_ID: &str = "carillon:profile:provider";
const PROVIDER_PAGE_PREFIX: &str = "carillon:profile:provider-page:";
const MODEL_SELECT_ID: &str = "carillon:profile:model";
const MODEL_PAGE_PREFIX: &str = "carillon:profile:model-page:";
const REASONING_SELECT_ID: &str = "carillon:profile:reasoning";
const TIER_SELECT_ID: &str = "carillon:profile:tier";
const SCOPE_SELECT_ID: &str = "carillon:profile:scope";
const PROFILE_PAGE_SIZE: usize = 25;

async fn handle_model_interaction(
    state: &DiscordState,
    http: &DiscordHttp,
    interaction: Interaction,
) -> Result<()> {
    let Some(data) = interaction.data.as_ref() else {
        return Ok(());
    };
    let relevant = match data {
        InteractionData::ApplicationCommand(command) => command.name == "model",
        InteractionData::MessageComponent(component) => {
            component.custom_id.starts_with(PROFILE_PREFIX)
        }
        _ => false,
    };
    if !relevant {
        return Ok(());
    }
    let component = matches!(data, InteractionData::MessageComponent(_));
    let config = LoadedConfig::load(&state.config_path)?;
    let guild = interaction.guild_id.map(|guild| guild.to_string());
    let user = interaction.author_id().map(|user| user.to_string());
    let channel = interaction
        .channel
        .as_ref()
        .map(|channel| channel.id.to_string());
    let parent = interaction
        .channel
        .as_ref()
        .and_then(|channel| channel.parent_id)
        .map(|parent| parent.to_string());
    let admitted = user.as_deref().is_some_and(|user| {
        adapter_identity_admit(&config.value.discord, guild.as_deref(), user).is_ok()
            && channel.as_deref().is_some_and(|channel| {
                adapter_route_admit(&config.value.discord, channel, parent.as_deref()).is_ok()
            })
    });
    if !admitted {
        let response = interaction_response(
            if component {
                InteractionResponseType::UpdateMessage
            } else {
                InteractionResponseType::ChannelMessageWithSource
            },
            "This interaction is not authorized.",
            !component,
        );
        http.interaction(interaction.application_id)
            .create_response(interaction.id, &interaction.token, &response)
            .await
            .context("respond to unauthorized Discord model interaction")?;
        return Ok(());
    }
    let audience = format!(
        "discord-{}-{}",
        guild
            .as_deref()
            .context("Discord model interaction has no guild")?,
        channel
            .as_deref()
            .context("Discord model interaction has no channel")?
    );
    let current_conversation = channel.as_deref().is_some_and(|channel| {
        current_conversation_available(&config.value.discord, channel, parent.as_deref())
    });
    let deferred = deferred_model_response(component);
    http.interaction(interaction.application_id)
        .create_response(interaction.id, &interaction.token, &deferred)
        .await
        .context("defer Discord model interaction")?;

    let response =
        match model_interaction_response(state, data, &audience, current_conversation).await {
            Ok(response) => response,
            Err(failure) => {
                warn!(error = %failure, "model interaction operation failed");
                interaction_data("Execution profiles are currently unavailable.", Vec::new())
            }
        };
    http.interaction(interaction.application_id)
        .update_response(&interaction.token)
        .content(response.content.as_deref())
        .components(response.components.as_deref())
        .await
        .context("update Discord model interaction response")?;
    Ok(())
}

async fn model_interaction_response(
    state: &DiscordState,
    data: &InteractionData,
    audience: &str,
    current_conversation: bool,
) -> Result<InteractionResponseData> {
    let profiles = fetch_runtime_execution_profiles(state).await?;
    match data {
        InteractionData::ApplicationCommand(_) => Ok(provider_picker(&profiles, 0)),
        InteractionData::MessageComponent(component)
            if component.custom_id == PROVIDER_SELECT_ID =>
        {
            let provider = selected_component_value(component)?;
            Ok(model_picker(&profiles, provider, 0))
        }
        InteractionData::MessageComponent(component)
            if component.custom_id.starts_with(PROVIDER_PAGE_PREFIX) =>
        {
            let page = component
                .custom_id
                .strip_prefix(PROVIDER_PAGE_PREFIX)
                .context("unknown provider page component")?
                .parse::<usize>()
                .context("invalid provider page")?;
            Ok(provider_picker(&profiles, page))
        }
        InteractionData::MessageComponent(component)
            if component.custom_id.starts_with(MODEL_PAGE_PREFIX) =>
        {
            let state = component
                .custom_id
                .strip_prefix(MODEL_PAGE_PREFIX)
                .context("unknown model page component")?;
            let (provider, page) = state
                .rsplit_once(':')
                .context("invalid model page component")?;
            Ok(model_picker(
                &profiles,
                provider,
                page.parse::<usize>().context("invalid model page")?,
            ))
        }
        InteractionData::MessageComponent(component) if component.custom_id == MODEL_SELECT_ID => {
            let key = selected_component_value(component)?;
            let model = profile_model(&profiles, key)?;
            Ok(next_setting_picker(
                &profiles,
                model,
                None,
                None,
                current_conversation,
            ))
        }
        InteractionData::MessageComponent(component)
            if component.custom_id == REASONING_SELECT_ID =>
        {
            let values = profile_selection_parts(selected_component_value(component)?, 2)?;
            let model = profile_model(&profiles, values[0])?;
            Ok(next_setting_picker(
                &profiles,
                model,
                Some(values[1]),
                None,
                current_conversation,
            ))
        }
        InteractionData::MessageComponent(component) if component.custom_id == TIER_SELECT_ID => {
            let values = profile_selection_parts(selected_component_value(component)?, 3)?;
            let model = profile_model(&profiles, values[0])?;
            Ok(scope_picker(
                model,
                optional_profile_value(values[1]),
                Some(values[2]),
                current_conversation,
            ))
        }
        InteractionData::MessageComponent(component) if component.custom_id == SCOPE_SELECT_ID => {
            let values = profile_selection_parts(selected_component_value(component)?, 4)?;
            let model = profile_model(&profiles, values[0])?;
            let scope = match values[3] {
                "c" if current_conversation => ExecutionProfileScope::CurrentConversation,
                "c" => bail!("this route parent is not a durable conversation"),
                "n" => ExecutionProfileScope::NewConversations,
                _ => bail!("invalid execution-profile scope"),
            };
            let applied = apply_runtime_execution_profile(
                state,
                &ExecutionProfileSelection {
                    key: model.key.clone(),
                    reasoning: optional_profile_value(values[1]).map(str::to_owned),
                    service_tier: optional_profile_value(values[2]).map(str::to_owned),
                    scope,
                    audience_id: matches!(scope, ExecutionProfileScope::CurrentConversation)
                        .then(|| audience.to_owned()),
                },
            )
            .await?;
            Ok(applied_profile_response(&applied))
        }
        _ => bail!("unsupported model interaction"),
    }
}

fn provider_picker(
    profiles: &RuntimeExecutionProfiles,
    requested_page: usize,
) -> InteractionResponseData {
    if profiles.models.is_empty() {
        return interaction_data(
            "Pi has no available models. Log in through Pi first.",
            Vec::new(),
        );
    }
    let providers = profiles
        .models
        .iter()
        .map(|model| (model.provider.as_str(), model.provider_name.as_str()))
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect::<Vec<_>>();
    let pages = providers.len().div_ceil(PROFILE_PAGE_SIZE);
    let page = requested_page.min(pages - 1);
    let start = page * PROFILE_PAGE_SIZE;
    let selected_provider = profiles
        .selected
        .as_ref()
        .and_then(|selected| selected.requested.model.split_once('/'))
        .map(|(provider, _)| provider);
    let options = providers[start..providers.len().min(start + PROFILE_PAGE_SIZE)]
        .iter()
        .map(|(provider, provider_name)| SelectMenuOption {
            default: selected_provider == Some(*provider),
            description: None,
            emoji: None,
            label: limit_discord_text(provider_name, 100),
            value: (*provider).to_owned(),
        })
        .collect();
    let mut components = vec![text_select(
        PROVIDER_SELECT_ID,
        "Choose a provider",
        options,
    )];
    if pages > 1 {
        components.push(page_buttons(
            format!("{PROVIDER_PAGE_PREFIX}{}", page.saturating_sub(1)),
            format!("{PROVIDER_PAGE_PREFIX}{}", page + 1),
            page == 0,
            page + 1 == pages,
        ));
    }
    interaction_data("Choose a provider.", components)
}

fn model_picker(
    profiles: &RuntimeExecutionProfiles,
    provider: &str,
    requested_page: usize,
) -> InteractionResponseData {
    let models = profiles
        .models
        .iter()
        .filter(|model| model.provider == provider)
        .collect::<Vec<_>>();
    if models.is_empty() {
        return interaction_data("That provider has no available models.", Vec::new());
    }
    let pages = models.len().div_ceil(PROFILE_PAGE_SIZE);
    let page = requested_page.min(pages - 1);
    let start = page * PROFILE_PAGE_SIZE;
    let selected = profiles
        .selected
        .as_ref()
        .map(|selection| selection.requested.model.as_str());
    let options = models[start..models.len().min(start + PROFILE_PAGE_SIZE)]
        .iter()
        .map(|model| SelectMenuOption {
            default: selected == Some(format!("{}/{}", model.provider, model.id).as_str()),
            description: Some(limit_discord_text(&model.id, 100)),
            emoji: None,
            label: limit_discord_text(&model.name, 100),
            value: model.key.clone(),
        })
        .collect();
    let mut components = vec![text_select(MODEL_SELECT_ID, "Choose a model", options)];
    if pages > 1 {
        components.push(page_buttons(
            format!("{MODEL_PAGE_PREFIX}{provider}:{}", page.saturating_sub(1)),
            format!("{MODEL_PAGE_PREFIX}{provider}:{}", page + 1),
            page == 0,
            page + 1 == pages,
        ));
    }
    interaction_data(
        &format!("Provider: **{}**. Choose a model.", models[0].provider_name),
        components,
    )
}

fn next_setting_picker(
    profiles: &RuntimeExecutionProfiles,
    model: &RuntimeExecutionProfileModel,
    reasoning: Option<&str>,
    service_tier: Option<&str>,
    current_conversation: bool,
) -> InteractionResponseData {
    if reasoning.is_none() && !model.reasoning.is_empty() {
        let selected = profiles
            .selected
            .as_ref()
            .filter(|selected| {
                selected.requested.model == format!("{}/{}", model.provider, model.id)
            })
            .and_then(|selected| selected.requested.reasoning.as_deref());
        let options = model
            .reasoning
            .iter()
            .map(|level| SelectMenuOption {
                default: selected == Some(level.as_str()),
                description: None,
                emoji: None,
                label: title_case_profile_value(level),
                value: format!("{}|{level}", model.key),
            })
            .collect();
        return interaction_data(
            &format!(
                "Model: **{}/{}**. Choose reasoning effort.",
                model.provider, model.id
            ),
            vec![text_select(
                REASONING_SELECT_ID,
                "Choose reasoning effort",
                options,
            )],
        );
    }
    if service_tier.is_none() && !model.service_tiers.is_empty() {
        let reasoning_value = reasoning.unwrap_or("-");
        let options = model
            .service_tiers
            .iter()
            .map(|tier| SelectMenuOption {
                default: tier == "standard",
                description: Some(if tier == "fast" {
                    "OpenAI priority service tier".to_owned()
                } else {
                    "OpenAI standard service tier".to_owned()
                }),
                emoji: None,
                label: title_case_profile_value(tier),
                value: format!("{}|{reasoning_value}|{tier}", model.key),
            })
            .collect();
        return interaction_data(
            &format!(
                "Model: **{}/{}**. Choose service tier.",
                model.provider, model.id
            ),
            vec![text_select(
                TIER_SELECT_ID,
                "Choose Standard or Fast",
                options,
            )],
        );
    }
    scope_picker(model, reasoning, service_tier, current_conversation)
}

fn scope_picker(
    model: &RuntimeExecutionProfileModel,
    reasoning: Option<&str>,
    service_tier: Option<&str>,
    current_conversation: bool,
) -> InteractionResponseData {
    let reasoning_value = reasoning.unwrap_or("-");
    let tier_value = service_tier.unwrap_or("-");
    let profile = format!(
        "Model: **{}/{}**{}{}. Choose where to apply it.",
        model.provider,
        model.id,
        reasoning.map_or_else(String::new, |value| format!(", reasoning: **{value}**")),
        service_tier.map_or_else(String::new, |value| format!(", tier: **{value}**")),
    );
    let mut options = Vec::with_capacity(2);
    if current_conversation {
        options.push(SelectMenuOption {
            default: false,
            description: Some("Persist with this native Pi continuation".to_owned()),
            emoji: None,
            label: "This conversation".to_owned(),
            value: format!("{}|{reasoning_value}|{tier_value}|c", model.key),
        });
    }
    options.push(SelectMenuOption {
        default: false,
        description: Some("Use as the default for new conversations".to_owned()),
        emoji: None,
        label: "New conversations".to_owned(),
        value: format!("{}|{reasoning_value}|{tier_value}|n", model.key),
    });
    interaction_data(
        &profile,
        vec![text_select(SCOPE_SELECT_ID, "Choose scope", options)],
    )
}

fn applied_profile_response(applied: &ExecutionProfileApplication) -> InteractionResponseData {
    let scope = match applied.scope {
        ExecutionProfileScope::CurrentConversation => "this conversation",
        ExecutionProfileScope::NewConversations => "new conversations",
    };
    interaction_data(
        &format!(
            "Execution profile set for **{scope}**: model **{}**{}{}.",
            applied.effective.model,
            applied
                .effective
                .reasoning
                .as_deref()
                .map_or_else(String::new, |value| format!(", reasoning **{value}**")),
            applied
                .requested
                .service_tier
                .as_deref()
                .map_or_else(String::new, |value| format!(", requested tier **{value}**")),
        ),
        Vec::new(),
    )
}

fn text_select(custom_id: &str, placeholder: &str, options: Vec<SelectMenuOption>) -> Component {
    Component::ActionRow(ActionRow {
        id: None,
        components: vec![Component::SelectMenu(SelectMenu {
            id: None,
            channel_types: None,
            custom_id: custom_id.to_owned(),
            default_values: None,
            disabled: false,
            kind: SelectMenuType::Text,
            max_values: Some(1),
            min_values: Some(1),
            options: Some(options),
            placeholder: Some(placeholder.to_owned()),
            required: None,
        })],
    })
}

fn page_buttons(
    previous_id: String,
    next_id: String,
    previous_disabled: bool,
    next_disabled: bool,
) -> Component {
    Component::ActionRow(ActionRow {
        id: None,
        components: vec![
            profile_page_button("Previous", previous_id, previous_disabled),
            profile_page_button("Next", next_id, next_disabled),
        ],
    })
}

fn profile_page_button(label: &str, custom_id: String, disabled: bool) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(custom_id),
        disabled,
        emoji: None,
        label: Some(label.to_owned()),
        style: ButtonStyle::Secondary,
        url: None,
        sku_id: None,
    })
}

fn selected_component_value(
    component: &twilight_model::application::interaction::message_component::MessageComponentInteractionData,
) -> Result<&str> {
    component
        .values
        .first()
        .map(String::as_str)
        .context("profile selection is empty")
}

fn profile_selection_parts(value: &str, expected: usize) -> Result<Vec<&str>> {
    let parts = value.split('|').collect::<Vec<_>>();
    if parts.len() != expected {
        bail!("invalid execution-profile selection");
    }
    Ok(parts)
}

fn optional_profile_value(value: &str) -> Option<&str> {
    (value != "-").then_some(value)
}

fn profile_model<'a>(
    profiles: &'a RuntimeExecutionProfiles,
    key: &str,
) -> Result<&'a RuntimeExecutionProfileModel> {
    profiles
        .models
        .iter()
        .find(|model| model.key == key)
        .context("selected model is unavailable")
}

fn title_case_profile_value(value: &str) -> String {
    let mut characters = value.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + characters.as_str()
    })
}

fn current_conversation_available(
    config: &DiscordConfig,
    channel_id: &str,
    parent_channel_id: Option<&str>,
) -> bool {
    parent_channel_id.is_some_and(|parent| {
        config.routes.iter().any(|route| {
            route.channel_id == parent && route.conversation == DiscordConversation::Thread
        })
    }) || config.routes.iter().any(|route| {
        route.channel_id == channel_id && route.conversation == DiscordConversation::Channel
    })
}

fn deferred_model_response(component: bool) -> InteractionResponse {
    InteractionResponse {
        kind: if component {
            InteractionResponseType::DeferredUpdateMessage
        } else {
            InteractionResponseType::DeferredChannelMessageWithSource
        },
        data: (!component).then(|| InteractionResponseData {
            flags: Some(MessageFlags::EPHEMERAL),
            ..InteractionResponseData::default()
        }),
    }
}

fn interaction_response(
    kind: InteractionResponseType,
    content: &str,
    ephemeral: bool,
) -> InteractionResponse {
    InteractionResponse {
        kind,
        data: Some(InteractionResponseData {
            flags: ephemeral.then_some(MessageFlags::EPHEMERAL),
            ..interaction_data(content, Vec::new())
        }),
    }
}

fn interaction_data(content: &str, components: Vec<Component>) -> InteractionResponseData {
    InteractionResponseData {
        content: Some(content.to_owned()),
        components: Some(components),
        ..InteractionResponseData::default()
    }
}

fn limit_discord_text(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

async fn fetch_runtime_execution_profiles(
    state: &DiscordState,
) -> Result<RuntimeExecutionProfiles> {
    state
        .client
        .get(format!("{}/v1/execution-profiles", state.core_url))
        .bearer_auth(&state.token)
        .send()
        .await
        .context("fetch runtime execution profiles")?
        .error_for_status()
        .context("core rejected execution-profile listing")?
        .json()
        .await
        .context("decode runtime execution profiles")
}

async fn apply_runtime_execution_profile(
    state: &DiscordState,
    selection: &ExecutionProfileSelection,
) -> Result<ExecutionProfileApplication> {
    state
        .client
        .put(format!("{}/v1/execution-profile", state.core_url))
        .bearer_auth(&state.token)
        .json(selection)
        .send()
        .await
        .context("apply runtime execution profile")?
        .error_for_status()
        .context("core rejected execution-profile selection")?
        .json()
        .await
        .context("decode applied execution profile")
}

async fn delivery_loop(state: DiscordState, http: Arc<DiscordHttp>) {
    loop {
        match claim_core_delivery(&state).await {
            Ok(Some(delivery)) => {
                let target = delivery.target_id.parse::<Id<ChannelMarker>>();
                let (outcome, native_ref, feedback) = match target {
                    Ok(channel_id) => match http
                        .create_message(channel_id)
                        .content(&delivery.content)
                        .await
                    {
                        Ok(message) => (
                            DeliveryOutcome::Delivered,
                            message.model().await.map(|value| value.id.to_string()).ok(),
                            Feedback::Delivered,
                        ),
                        Err(failure) => {
                            warn!(error = %failure, delivery_id = delivery.id, "Discord delivery outcome uncertain");
                            (DeliveryOutcome::Uncertain, None, Feedback::Uncertain)
                        }
                    },
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
                match LoadedConfig::load(&state.config_path) {
                    Ok(config) => {
                        terminal_feedback(
                            &http,
                            &config.value.discord,
                            &delivery.feedback_target_id,
                            feedback,
                        )
                        .await;
                    }
                    Err(failure) => {
                        warn!(error = %failure, "reload Discord feedback configuration");
                    }
                }
            }
            Ok(None) => sleep(Duration::from_millis(500)).await,
            Err(failure) => {
                warn!(error = %failure.message, "delivery poll failed");
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn submit_event(
    state: &DiscordState,
    event: &ExternalEvent,
) -> Result<EventReceipt, ServiceError> {
    Ok(state
        .client
        .post(format!("{}/v1/events", state.core_url))
        .bearer_auth(&state.token)
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

fn adapter_route_admit(
    config: &DiscordConfig,
    channel_id: &str,
    parent_channel_id: Option<&str>,
) -> Result<(), ServiceError> {
    let direct = config
        .routes
        .iter()
        .any(|route| route.channel_id == channel_id);
    let child = parent_channel_id.is_some_and(|parent| {
        config.routes.iter().any(|route| {
            route.channel_id == parent && route.conversation == DiscordConversation::Thread
        })
    });
    if direct || child {
        Ok(())
    } else {
        Err(ServiceError {
            status: StatusCode::FORBIDDEN,
            message: "channel is not configured as a Discord route".to_owned(),
        })
    }
}

fn authorize(headers: &HeaderMap, expected_hash: &str) -> Result<(), ServiceError> {
    let token = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| ServiceError {
            status: StatusCode::FORBIDDEN,
            message: "component authentication required".to_owned(),
        })?;
    if !token_matches(expected_hash, token) {
        return Err(ServiceError {
            status: StatusCode::FORBIDDEN,
            message: "component authentication failed".to_owned(),
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
    use std::{collections::BTreeMap, net::SocketAddr};

    use anyhow::{Context as _, anyhow};
    use serde_json::Value;

    use super::{
        DiscordParticipation, MessagePlan, ServiceError, deferred_model_response, model_picker,
        next_setting_picker, resolve_message, resolved_message, thread_title,
    };
    use crate::{
        config::{DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute},
        model::{
            ExecutionProfile, ExecutionProfileEvidence, RuntimeExecutionProfileModel,
            RuntimeExecutionProfiles, SimulateRequest,
        },
    };

    #[test]
    fn discord_model_deferral_payloads_match_interaction_kinds() -> serde_json::Result<()> {
        let command = serde_json::to_value(deferred_model_response(false))?;
        assert_eq!(command["type"], 5);
        assert_eq!(command["data"]["flags"], 64);

        let component = serde_json::to_value(deferred_model_response(true))?;
        assert_eq!(component["type"], 6);
        assert_eq!(component["data"], Value::Null);
        Ok(())
    }

    #[test]
    fn discord_model_picker_pages_every_available_model() -> serde_json::Result<()> {
        let models = (0..26)
            .map(|index| RuntimeExecutionProfileModel {
                key: format!("key-{index}"),
                provider: "provider".to_owned(),
                provider_name: "Provider".to_owned(),
                id: format!("model-{index}"),
                name: format!("Model {index}"),
                reasoning: Vec::new(),
                service_tiers: Vec::new(),
            })
            .collect::<Vec<_>>();
        let profiles = RuntimeExecutionProfiles {
            models,
            selected: Some(ExecutionProfileEvidence {
                requested: ExecutionProfile {
                    model: "provider/model-25".to_owned(),
                    reasoning: None,
                    service_tier: None,
                },
                effective: Some(ExecutionProfile {
                    model: "provider/model-25".to_owned(),
                    reasoning: None,
                    service_tier: None,
                }),
            }),
        };
        let first = serde_json::to_value(model_picker(&profiles, "provider", 0))?;
        assert_eq!(
            first["components"][0]["components"][0]["options"]
                .as_array()
                .map(Vec::len),
            Some(25)
        );
        assert_eq!(
            first["components"][1]["components"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );

        let second = serde_json::to_value(model_picker(&profiles, "provider", 1))?;
        assert_eq!(
            second["components"][0]["components"][0]["options"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(
            second["components"][0]["components"][0]["options"][0]["default"],
            Value::Bool(true)
        );
        Ok(())
    }

    #[test]
    fn discord_profile_picker_skips_unsupported_settings() -> serde_json::Result<()> {
        let supported = RuntimeExecutionProfileModel {
            key: "supported".to_owned(),
            provider: "openai-codex".to_owned(),
            provider_name: "OpenAI (ChatGPT Plus/Pro)".to_owned(),
            id: "gpt".to_owned(),
            name: "GPT".to_owned(),
            reasoning: vec!["low".to_owned(), "high".to_owned()],
            service_tiers: vec!["standard".to_owned(), "fast".to_owned()],
        };
        let unsupported = RuntimeExecutionProfileModel {
            key: "unsupported".to_owned(),
            provider: "other".to_owned(),
            provider_name: "Other".to_owned(),
            id: "plain".to_owned(),
            name: "Plain".to_owned(),
            reasoning: Vec::new(),
            service_tiers: Vec::new(),
        };
        let profiles = RuntimeExecutionProfiles {
            models: vec![supported.clone(), unsupported.clone()],
            selected: None,
        };
        let reasoning =
            serde_json::to_value(next_setting_picker(&profiles, &supported, None, None, true))?;
        assert_eq!(
            reasoning["components"][0]["components"][0]["custom_id"],
            "carillon:profile:reasoning"
        );
        let tier = serde_json::to_value(next_setting_picker(
            &profiles,
            &supported,
            Some("low"),
            None,
            true,
        ))?;
        assert_eq!(
            tier["components"][0]["components"][0]["custom_id"],
            "carillon:profile:tier"
        );
        let scope = serde_json::to_value(next_setting_picker(
            &profiles,
            &unsupported,
            None,
            None,
            false,
        ))?;
        assert_eq!(
            scope["components"][0]["components"][0]["custom_id"],
            "carillon:profile:scope"
        );
        assert_eq!(
            scope["components"][0]["components"][0]["options"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        let providers = serde_json::to_value(super::provider_picker(&profiles, 0))?;
        assert_eq!(
            providers["components"][0]["components"][0]["options"][0]["label"],
            "OpenAI (ChatGPT Plus/Pro)"
        );
        Ok(())
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
            id: "discord".to_owned(),
            version: "test".to_owned(),
            listen: SocketAddr::from(([127, 0, 0, 1], 1)),
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
            token_hash: "token".to_owned(),
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
