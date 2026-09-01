use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
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
        ProviderRole, read_token_file, token_matches,
    },
    model::{
        CapabilityRequest, CapabilityResponse, Delivery, DeliveryAck, DeliveryOutcome,
        DeliveryReconciliation, EventReceipt, ExecutionProfileApplication, ExecutionProfileScope,
        ExecutionProfileSelection, ExtensionRequest, ExternalEvent, Health, ProviderEvidence,
        ProviderTurnResponse, RuntimeExecutionProfileModel, RuntimeExecutionProfiles, ServiceTier,
        SimulateRequest, SimulateResponse,
    },
};

const DISCORD_MESSAGE_LIMIT: usize = 2_000;

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
    path: PathBuf,
    evidence: Arc<Mutex<BTreeMap<String, ProviderEvidence>>>,
}

pub async fn serve_extension(
    config: LoadedConfig,
    provider_id: &str,
    token_file: &Path,
) -> Result<()> {
    let provider = config
        .provider(provider_id, ProviderRole::InterfaceExtension)?
        .clone();
    let token = read_token_file(token_file)?;
    if !token_matches(&provider.token_hash, &token) {
        bail!("interface extension token does not match configuration");
    }
    let state_dir = config.value.data_dir.join("components").join(&provider.id);
    fs::create_dir_all(&state_dir)?;
    let path = state_dir.join("evidence.json");
    let evidence = if path.exists() {
        serde_json::from_slice(&fs::read(&path)?)?
    } else {
        BTreeMap::new()
    };
    let state = ExtensionState {
        token_hash: provider.token_hash,
        id: provider.id,
        version: provider.version,
        path,
        evidence: Arc::new(Mutex::new(evidence)),
    };
    let router = Router::new()
        .route("/health", get(extension_health))
        .route("/v1/invoke", post(invoke_extension))
        .route("/v1/evidence/{id}", get(extension_evidence))
        .with_state(state);
    serve_component(provider.url, router, "interface extension").await
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
) -> Result<Json<ProviderTurnResponse>, ServiceError> {
    authorize(&headers, &state.token_hash)?;
    if request.command != "carillon" {
        return Err(ServiceError {
            status: StatusCode::NOT_FOUND,
            message: "unknown command".to_owned(),
        });
    }
    let mut evidence = state.evidence.lock().await;
    if let Some(existing) = evidence.get(&request.correlation_id) {
        return existing
            .result
            .clone()
            .map(Json)
            .ok_or_else(|| ServiceError {
                status: StatusCode::CONFLICT,
                message: "extension operation is not terminal".to_owned(),
            });
    }
    let detail = if request.arguments.is_empty() {
        "ready"
    } else {
        request.arguments.as_str()
    };
    let response = ProviderTurnResponse {
        correlation_id: request.correlation_id.clone(),
        output: format!(
            "Carillon {} ({detail}); audience {}; operator {}",
            state.version, request.audience_id, request.user_id
        ),
        continuation_ref: format!("extension:{}", request.correlation_id),
        native: json!({ "command": request.command, "arguments": request.arguments, "extension": state.id }),
        capability_activity: Vec::new(),
    };
    let correlation_id = request.correlation_id;
    evidence.insert(
        correlation_id.clone(),
        ProviderEvidence {
            correlation_id: correlation_id.clone(),
            state: "completed".to_owned(),
            error: None,
            result: Some(response.clone()),
        },
    );
    if let Err(failure) = persist_json(&state.path, &*evidence) {
        evidence.remove(&correlation_id);
        return Err(failure.into());
    }
    Ok(Json(response))
}

async fn extension_evidence(
    State(state): State<ExtensionState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<ProviderEvidence>, ServiceError> {
    authorize(&headers, &state.token_hash)?;
    state
        .evidence
        .lock()
        .await
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or_else(|| ServiceError {
            status: StatusCode::NOT_FOUND,
            message: "extension evidence not found".to_owned(),
        })
}

#[derive(Clone)]
struct CapabilityState {
    token_hash: String,
    id: String,
    version: String,
    path: PathBuf,
    evidence: Arc<Mutex<BTreeMap<String, CapabilityResponse>>>,
}

pub async fn serve_capability(
    config: LoadedConfig,
    provider_id: &str,
    token_file: &Path,
) -> Result<()> {
    let provider = config
        .provider(provider_id, ProviderRole::Capability)?
        .clone();
    let token = read_token_file(token_file)?;
    if !token_matches(&provider.token_hash, &token) {
        bail!("capability token does not match configuration");
    }
    let state_dir = config.value.data_dir.join("components").join(&provider.id);
    fs::create_dir_all(&state_dir)?;
    let path = state_dir.join("evidence.json");
    let evidence = if path.exists() {
        serde_json::from_slice(&fs::read(&path)?)?
    } else {
        BTreeMap::new()
    };
    let state = CapabilityState {
        token_hash: provider.token_hash,
        id: provider.id,
        version: provider.version,
        path,
        evidence: Arc::new(Mutex::new(evidence)),
    };
    let router = Router::new()
        .route("/health", get(capability_health))
        .route("/v1/execute", post(execute_capability))
        .with_state(state);
    serve_component(provider.url, router, "capability").await
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
    pending_messages: Vec<PendingMessage>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct InboundMessage {
    request: SimulateRequest,
    native: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "stage", content = "message", rename_all = "snake_case")]
enum PendingMessage {
    Inbound(InboundMessage),
    Planned(MessagePlan),
}

impl PendingMessage {
    fn event_id(&self) -> &str {
        match self {
            Self::Inbound(inbound) => &inbound.request.event_id,
            Self::Planned(plan) => &plan.event_id,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
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

type InboundMessageQueues = Arc<Mutex<BTreeMap<String, VecDeque<InboundMessage>>>>;
type AudienceMessageQueues = Arc<Mutex<BTreeMap<String, VecDeque<ResolvedMessage>>>>;

pub async fn serve_discord(
    config: LoadedConfig,
    token_file: &Path,
    discord_token_file: Option<&Path>,
    deterministic: bool,
) -> Result<()> {
    let provider = config
        .provider(&config.discord()?.provider, ProviderRole::Interface)?
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
    let participation = if participation_path.exists() {
        serde_json::from_slice(&fs::read(&participation_path)?).with_context(|| {
            format!(
                "parse Discord participation state {}",
                participation_path.display()
            )
        })?
    } else {
        DiscordParticipation::default()
    };
    let state = DiscordState {
        config_path: config.path.clone(),
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
    let router = Router::new().route("/health", get(discord_health));
    let router = if deterministic {
        router.route("/v1/simulate", post(simulate))
    } else {
        router
    };
    let router = router.with_state(state.clone());
    if deterministic {
        info!(url = %provider.url, deterministic, "Discord interface provider ready");
        return serve_component(provider.url, router, "Discord interface provider").await;
    }

    let discord_token_path = discord_token_file
        .context("--discord-token-file is required outside deterministic mode")?;
    let discord_token = read_token_file(discord_token_path)?;
    let http = Arc::new(DiscordHttp::new(discord_token.clone()));
    let current_user = http.current_user().await?.model().await?;
    if current_user.id.to_string() != config.discord()?.bot_user_id {
        bail!("configured Discord bot user ID does not match the token");
    }
    for target in reconciled.feedback_target_ids {
        terminal_feedback(&http, config.discord()?, &target, Feedback::Uncertain).await;
    }
    let application_id = http.current_user_application().await?.model().await?.id;
    let guild_id = config
        .discord()?
        .guild_id
        .parse::<Id<GuildMarker>>()
        .context("configured Discord guild ID is invalid")?;
    http.interaction(application_id)
        .create_guild_command(guild_id)
        .chat_input("model", "Choose provider, model, and supported settings")
        .await?;
    let (messages, receiver) = mpsc::channel(64);
    let queues = Arc::new(Mutex::new(BTreeMap::new()));
    let gateway_http = http.clone();
    let delivery_state = state.clone();
    start_message_worker(state.clone(), http.clone(), queues.clone(), receiver);
    tokio::spawn(async move {
        delivery_loop(delivery_state, http).await;
    });
    info!(url = %provider.url, deterministic, "Discord interface provider ready");
    tokio::select! {
        result = gateway_loop(state, discord_token, gateway_http, messages) => {
            result?;
            bail!("Discord gateway stopped");
        }
        result = serve_component(provider.url, router, "Discord interface provider") => result,
    }
}

async fn discord_health(State(state): State<DiscordState>) -> Result<Json<Health>, ServiceError> {
    let config = LoadedConfig::load(&state.config_path)?;
    let provider = config.provider(&state.provider_id, ProviderRole::Interface)?;
    Ok(Json(Health {
        id: provider.id.clone(),
        role: provider.role.as_str().to_owned(),
        version: provider.version.clone(),
        status: "ready".to_owned(),
    }))
}

async fn simulate(
    State(state): State<DiscordState>,
    Json(request): Json<SimulateRequest>,
) -> Result<Response, ServiceError> {
    let config = LoadedConfig::load(&state.config_path)?;
    adapter_identity_admit(
        config.discord()?,
        request.guild_id.as_deref(),
        &request.user_id,
    )?;
    let Some(plan) = resolve_message(
        config.discord()?,
        request,
        Value::String("deterministic-local-discord".to_owned()),
        &*state.participation.lock().await,
    )?
    else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let resolved = prepare_deterministic_conversation(&state, plan).await?;
    let mut feedback = Vec::new();
    if config.discord()?.reactions {
        feedback.push("processing".to_owned());
    }
    let receipt = submit_event(&state, &resolved.event).await?;
    let delivery = if !receipt.duplicate && receipt.state != "processing" {
        let delivery = claim_core_delivery(&state).await?;
        if let Some(value) = &delivery {
            acknowledge_core_delivery(
                &state,
                value.id,
                DeliveryOutcome::Delivered,
                value
                    .content
                    .as_ref()
                    .map(|_| format!("local:{}", resolved.event.event_id)),
            )
            .await?;
            if config.discord()?.reactions {
                feedback.push(
                    if value.content.is_some() {
                        "delivered"
                    } else {
                        "failed"
                    }
                    .to_owned(),
                );
            }
        }
        delivery
    } else {
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
                let guild = message.guild_id.map(|id| id.to_string());
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
        let thread = plan.source_message_id.clone();
        if !discord_message_thread_exists(http, &plan).await? {
            let channel = plan
                .source_channel_id
                .parse::<Id<ChannelMarker>>()
                .context("source Discord channel ID is invalid")?;
            let message = plan
                .source_message_id
                .parse::<Id<MessageMarker>>()
                .context("source Discord message ID is invalid")?;
            let created = async {
                let created = http
                    .create_thread_from_message(channel, message, &thread_title(&plan.content))
                    .await
                    .context("create Discord conversation thread")?
                    .model()
                    .await
                    .context("decode Discord conversation thread")?;
                if created.id.to_string() != thread {
                    bail!("Discord conversation thread did not inherit its source message ID");
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if let Err(failure) = created
                && !discord_message_thread_exists(http, &plan).await?
            {
                return Err(failure);
            }
        }
        adopt_thread(state, &thread, &plan.route_channel_id)
            .await
            .map_err(|failure| anyhow::anyhow!(failure.message))?;
        thread
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

async fn discord_message_thread_exists(http: &DiscordHttp, plan: &MessagePlan) -> Result<bool> {
    let thread = plan
        .source_message_id
        .parse::<Id<ChannelMarker>>()
        .context("source Discord message ID is invalid")?;
    let Ok(response) = http.channel(thread).await else {
        return Ok(false);
    };
    let channel = response
        .model()
        .await
        .context("decode existing Discord conversation thread")?;
    let expected_parent = plan
        .route_channel_id
        .parse::<Id<ChannelMarker>>()
        .context("route Discord channel ID is invalid")?;
    if !matches!(
        channel.kind,
        ChannelType::AnnouncementThread | ChannelType::PrivateThread | ChannelType::PublicThread
    ) || channel.parent_id != Some(expected_parent)
    {
        bail!("source Discord message ID belongs to an unexpected channel");
    }
    Ok(true)
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

async fn remember_inbound_message(state: &DiscordState, inbound: &InboundMessage) -> Result<bool> {
    let mut participation = state.participation.lock().await;
    if let Some(pending) = participation
        .pending_messages
        .iter()
        .find(|pending| pending.event_id() == inbound.request.event_id)
    {
        return match pending {
            PendingMessage::Inbound(existing) if existing == inbound => Ok(false),
            PendingMessage::Planned(_) => Ok(false),
            PendingMessage::Inbound(_) => {
                bail!("Discord event changed while its submission was pending")
            }
        };
    }
    let mut updated = participation.clone();
    updated
        .pending_messages
        .push(PendingMessage::Inbound(inbound.clone()));
    persist_json(&state.participation_path, &updated)?;
    *participation = updated;
    Ok(true)
}

async fn remember_pending_message(state: &DiscordState, plan: &MessagePlan) -> Result<()> {
    let mut participation = state.participation.lock().await;
    let existing = participation
        .pending_messages
        .iter()
        .position(|pending| pending.event_id() == plan.event_id);
    if let Some(index) = existing {
        match &participation.pending_messages[index] {
            PendingMessage::Planned(existing) if existing == plan => return Ok(()),
            PendingMessage::Planned(_) => {
                bail!("Discord event changed while its submission was pending")
            }
            PendingMessage::Inbound(_) => {}
        }
    }
    let mut updated = participation.clone();
    let planned = PendingMessage::Planned(plan.clone());
    if let Some(index) = existing {
        updated.pending_messages[index] = planned;
    } else {
        updated.pending_messages.push(planned);
    }
    persist_json(&state.participation_path, &updated)?;
    *participation = updated;
    Ok(())
}

async fn clear_pending_message(state: &DiscordState, event_id: &str) -> Result<()> {
    let mut participation = state.participation.lock().await;
    let mut updated = participation.clone();
    let prior_len = updated.pending_messages.len();
    updated
        .pending_messages
        .retain(|pending| pending.event_id() != event_id);
    if updated.pending_messages.len() != prior_len {
        persist_json(&state.participation_path, &updated)?;
        *participation = updated;
    }
    Ok(())
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
    let mut updated = participation.clone();
    updated
        .threads
        .insert(thread_id.to_owned(), route_channel_id.to_owned());
    persist_json(&state.participation_path, &updated)?;
    *participation = updated;
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
    inbound_queues: InboundMessageQueues,
    queues: AudienceMessageQueues,
    mut messages: mpsc::Receiver<InboundMessage>,
) {
    while let Some(inbound) = messages.recv().await {
        if let Ok(config) = LoadedConfig::load(&state.config_path)
            && let Ok(discord) = config.discord()
            && adapter_identity_admit(
                discord,
                inbound.request.guild_id.as_deref(),
                &inbound.request.user_id,
            )
            .is_err()
        {
            continue;
        }
        let remembered = loop {
            match remember_inbound_message(&state, &inbound).await {
                Ok(remembered) => break remembered,
                Err(failure) => {
                    warn!(error = %failure, event_id = %inbound.request.event_id, "persist Discord message before queueing");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        };
        if !remembered {
            continue;
        }
        let channel = inbound.request.channel_id.clone();
        if queue_inbound_message(&mut *inbound_queues.lock().await, inbound) {
            tokio::spawn(channel_message_worker(
                state.clone(),
                http.clone(),
                inbound_queues.clone(),
                queues.clone(),
                channel,
            ));
        }
    }
}

fn queue_inbound_message(
    queues: &mut BTreeMap<String, VecDeque<InboundMessage>>,
    inbound: InboundMessage,
) -> bool {
    let channel = inbound.request.channel_id.clone();
    if let Some(queue) = queues.get_mut(&channel) {
        queue.push_back(inbound);
        false
    } else {
        queues.insert(channel, VecDeque::from([inbound]));
        true
    }
}

async fn channel_message_worker(
    state: DiscordState,
    http: Arc<DiscordHttp>,
    inbound_queues: InboundMessageQueues,
    queues: AudienceMessageQueues,
    channel: String,
) {
    loop {
        let mut inbound = {
            let mut inbound_queues = inbound_queues.lock().await;
            let Some(queue) = inbound_queues.get_mut(&channel) else {
                return;
            };
            if let Some(inbound) = queue.pop_front() {
                inbound
            } else {
                inbound_queues.remove(&channel);
                return;
            }
        };
        let discord = loop {
            match LoadedConfig::load(&state.config_path) {
                Ok(config) => match config.discord() {
                    Ok(discord) => break discord.clone(),
                    Err(failure) => {
                        warn!(error = %failure, "validate Discord configuration for received message");
                    }
                },
                Err(failure) => {
                    warn!(error = %failure, "reload Discord configuration for received message");
                }
            }
            sleep(Duration::from_secs(1)).await;
        };
        if adapter_identity_admit(
            &discord,
            inbound.request.guild_id.as_deref(),
            &inbound.request.user_id,
        )
        .is_err()
        {
            if let Err(failure) = clear_pending_message(&state, &inbound.request.event_id).await {
                warn!(error = %failure, "clear rejected Discord message");
            }
            continue;
        }
        let inbound_event_id = inbound.request.event_id.clone();
        inbound.request.parent_channel_id = loop {
            match discord_parent_channel(&http, &discord, &inbound.request.channel_id).await {
                Ok(parent) => break parent,
                Err(failure) => {
                    warn!(error = %failure, "resolve Discord message route; retrying received message");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        };
        let Ok(Some(plan)) = resolve_message(
            &discord,
            inbound.request,
            inbound.native,
            &*state.participation.lock().await,
        ) else {
            if let Err(failure) = clear_pending_message(&state, &inbound_event_id).await {
                warn!(error = %failure, "clear ignored Discord message");
            }
            continue;
        };
        let source = format!("{}:{}", plan.source_channel_id, plan.source_message_id);
        loop {
            match queue_message_plan(&state, &http, &queues, plan.clone()).await {
                Ok(()) => break,
                Err(failure) if pending_message(&state, &plan.event_id).await => {
                    warn!(error = %failure, event_id = %plan.event_id, "retrying persisted Discord message");
                    tokio::spawn(replay_pending_message(
                        state.clone(),
                        http.clone(),
                        queues.clone(),
                        plan.clone(),
                    ));
                    break;
                }
                Err(failure) if pending_inbound_message(&state, &plan.event_id).await => {
                    warn!(error = %failure, event_id = %plan.event_id, "retrying Discord message persistence transition");
                    sleep(Duration::from_secs(1)).await;
                }
                Err(failure) => {
                    terminal_feedback(&http, &discord, &source, Feedback::Failed).await;
                    warn!(error = %failure, "Discord conversation resolution failed");
                    break;
                }
            }
        }
    }
}

fn start_message_worker(
    state: DiscordState,
    http: Arc<DiscordHttp>,
    queues: AudienceMessageQueues,
    receiver: mpsc::Receiver<InboundMessage>,
) {
    tokio::spawn(async move {
        let inbound_queues = Arc::new(Mutex::new(BTreeMap::new()));
        resume_pending_messages(&state, &http, &inbound_queues, &queues).await;
        message_worker(state, http, inbound_queues, queues, receiver).await;
    });
}

async fn resume_pending_messages(
    state: &DiscordState,
    http: &Arc<DiscordHttp>,
    inbound_queues: &InboundMessageQueues,
    queues: &AudienceMessageQueues,
) {
    let pending = state.participation.lock().await.pending_messages.clone();
    for message in pending {
        match message {
            PendingMessage::Inbound(inbound) => {
                let channel = inbound.request.channel_id.clone();
                if queue_inbound_message(&mut *inbound_queues.lock().await, inbound) {
                    tokio::spawn(channel_message_worker(
                        state.clone(),
                        http.clone(),
                        inbound_queues.clone(),
                        queues.clone(),
                        channel,
                    ));
                }
            }
            PendingMessage::Planned(plan) => {
                if let Err(failure) = queue_message_plan(state, http, queues, plan.clone()).await {
                    warn!(error = %failure, event_id = %plan.event_id, "retrying persisted Discord message");
                    tokio::spawn(replay_pending_message(
                        state.clone(),
                        http.clone(),
                        queues.clone(),
                        plan,
                    ));
                }
            }
        }
    }
}

async fn pending_message(state: &DiscordState, event_id: &str) -> bool {
    state
        .participation
        .lock()
        .await
        .pending_messages
        .iter()
        .any(
            |pending| matches!(pending, PendingMessage::Planned(plan) if plan.event_id == event_id),
        )
}

async fn pending_inbound_message(state: &DiscordState, event_id: &str) -> bool {
    state
        .participation
        .lock()
        .await
        .pending_messages
        .iter()
        .any(|pending| matches!(pending, PendingMessage::Inbound(inbound) if inbound.request.event_id == event_id))
}

async fn replay_pending_message(
    state: DiscordState,
    http: Arc<DiscordHttp>,
    queues: AudienceMessageQueues,
    plan: MessagePlan,
) {
    loop {
        match queue_message_plan(&state, &http, &queues, plan.clone()).await {
            Ok(()) => return,
            Err(failure) => {
                warn!(error = %failure, event_id = %plan.event_id, "retrying persisted Discord message");
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn queue_message_plan(
    state: &DiscordState,
    http: &Arc<DiscordHttp>,
    queues: &AudienceMessageQueues,
    plan: MessagePlan,
) -> Result<()> {
    remember_pending_message(state, &plan).await?;
    let resolved = prepare_discord_conversation(state, http, plan).await?;
    let audience = resolved.event.audience_id.clone();
    if queue_resolved_message(&mut *queues.lock().await, resolved) {
        tokio::spawn(audience_message_worker(
            state.clone(),
            http.clone(),
            queues.clone(),
            audience,
        ));
    }
    Ok(())
}

fn queue_resolved_message(
    queues: &mut BTreeMap<String, VecDeque<ResolvedMessage>>,
    resolved: ResolvedMessage,
) -> bool {
    let audience = resolved.event.audience_id.clone();
    if let Some(queue) = queues.get_mut(&audience) {
        queue.push_back(resolved);
        false
    } else {
        queues.insert(audience, VecDeque::from([resolved]));
        true
    }
}

async fn audience_message_worker(
    state: DiscordState,
    http: Arc<DiscordHttp>,
    queues: AudienceMessageQueues,
    audience: String,
) {
    loop {
        let resolved = {
            let mut queues = queues.lock().await;
            let Some(queue) = queues.get_mut(&audience) else {
                return;
            };
            if let Some(resolved) = queue.pop_front() {
                resolved
            } else {
                queues.remove(&audience);
                return;
            }
        };
        submit_resolved_message(&state, &http, resolved).await;
    }
}

async fn submit_resolved_message(
    state: &DiscordState,
    http: &Arc<DiscordHttp>,
    resolved: ResolvedMessage,
) {
    let discord = loop {
        match LoadedConfig::load(&state.config_path) {
            Ok(config) => match config.discord() {
                Ok(discord) => break discord.clone(),
                Err(failure) => {
                    warn!(error = %failure, "validate Discord configuration for event submission");
                }
            },
            Err(failure) => {
                warn!(error = %failure, "reload Discord configuration for event submission");
            }
        }
        sleep(Duration::from_secs(1)).await;
    };
    processing_feedback(http, &discord, &resolved.event.feedback_target_id).await;
    let typing = start_typing(http.clone(), &discord, &resolved.event.delivery_target_id).await;
    let receipt = loop {
        match submit_event(state, &resolved.event).await {
            Ok(receipt) => break Ok(receipt),
            Err(failure) if pending_message(state, &resolved.event.event_id).await => {
                warn!(
                    error = %failure.message,
                    event_id = %resolved.event.event_id,
                    "retrying persisted Discord event submission"
                );
                sleep(Duration::from_secs(1)).await;
            }
            Err(failure) => break Err(failure),
        }
    };
    if let Some(task) = typing {
        task.abort();
    }
    match receipt {
        Ok(receipt) if receipt.duplicate && receipt.state != "processing" => {
            if let Err(failure) = clear_pending_message(state, &resolved.event.event_id).await {
                warn!(error = %failure, "clear submitted Discord message intent");
            }
            clear_processing_feedback(http, &discord, &resolved.event.feedback_target_id).await;
        }
        Ok(_) => {
            if let Err(failure) = clear_pending_message(state, &resolved.event.event_id).await {
                warn!(error = %failure, "clear submitted Discord message intent");
            }
        }
        Err(failure) => {
            warn!(error = %failure.message, "Discord event submission failed");
            terminal_feedback(
                http,
                &discord,
                &resolved.event.feedback_target_id,
                Feedback::Failed,
            )
            .await;
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
const MODEL_SELECT_ID: &str = "carillon:profile:model";
const MODEL_PAGE_PREFIX: &str = "carillon:profile:p:";
const REASONING_SELECT_ID: &str = "carillon:profile:reasoning";
const TIER_SELECT_ID: &str = "carillon:profile:tier";
const APPLY_CURRENT_PREFIX: &str = "carillon:profile:c:";
const APPLY_NEW_PREFIX: &str = "carillon:profile:n:";
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
    let discord = config.discord()?;
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
        adapter_identity_admit(discord, guild.as_deref(), user).is_ok()
            && channel.as_deref().is_some_and(|channel| {
                adapter_route_admit(discord, channel, parent.as_deref()).is_ok()
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
    let current_conversation = channel
        .as_deref()
        .is_some_and(|channel| current_conversation_available(discord, channel, parent.as_deref()));
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
    let profiles =
        fetch_runtime_execution_profiles(state, current_conversation.then_some(audience)).await?;
    let InteractionData::MessageComponent(component) = data else {
        let model = initial_profile_model(&profiles)?;
        return Ok(selected_profile_panel(
            &profiles,
            model,
            current_conversation,
        ));
    };
    if component.custom_id.starts_with(APPLY_CURRENT_PREFIX) {
        if !current_conversation {
            bail!("this route parent is not a durable conversation");
        }
        return apply_profile_selection(
            state,
            &profiles,
            component
                .custom_id
                .strip_prefix(APPLY_CURRENT_PREFIX)
                .context("invalid current-conversation profile")?,
            ExecutionProfileScope::CurrentConversation,
            Some(audience.to_owned()),
        )
        .await;
    }
    if let Some(value) = component.custom_id.strip_prefix(APPLY_NEW_PREFIX) {
        return apply_profile_selection(
            state,
            &profiles,
            value,
            ExecutionProfileScope::NewConversations,
            None,
        )
        .await;
    }
    profile_draft_response(&profiles, component, current_conversation)
}

fn profile_draft_response(
    profiles: &RuntimeExecutionProfiles,
    component: &twilight_model::application::interaction::message_component::MessageComponentInteractionData,
    current_conversation: bool,
) -> Result<InteractionResponseData> {
    let custom_id = component.custom_id.as_str();
    if custom_id == PROVIDER_SELECT_ID {
        let model = provider_profile_model(profiles, selected_component_value(component)?)?;
        return Ok(selected_profile_panel(
            profiles,
            model,
            current_conversation,
        ));
    }
    if let Some(value) = custom_id.strip_prefix(MODEL_PAGE_PREFIX) {
        let (provider, page) = value
            .rsplit_once(':')
            .context("invalid model page component")?;
        let page = page.parse::<usize>().context("invalid model page")?;
        let model = profiles
            .models
            .iter()
            .filter(|model| model.provider == provider)
            .nth(page * PROFILE_PAGE_SIZE)
            .context("model page is unavailable")?;
        return Ok(profile_panel(
            profiles,
            model,
            None,
            None,
            current_conversation,
            page,
        ));
    }
    if custom_id == MODEL_SELECT_ID {
        let model = profile_model(profiles, selected_component_value(component)?)?;
        return Ok(selected_profile_panel(
            profiles,
            model,
            current_conversation,
        ));
    }
    if custom_id == REASONING_SELECT_ID || custom_id == TIER_SELECT_ID {
        let values = profile_selection_parts(selected_component_value(component)?, 3)?;
        let model = profile_model(profiles, values[0])?;
        let tier = optional_profile_value(values[2])
            .map(str::parse)
            .transpose()
            .map_err(anyhow::Error::msg)?;
        return Ok(profile_panel(
            profiles,
            model,
            optional_profile_value(values[1]),
            tier,
            current_conversation,
            model_page(profiles, model),
        ));
    }
    bail!("unsupported model interaction")
}

fn selected_profile_panel(
    profiles: &RuntimeExecutionProfiles,
    model: &RuntimeExecutionProfileModel,
    current_conversation: bool,
) -> InteractionResponseData {
    let (reasoning, tier) = selected_profile_values(profiles, model);
    profile_panel(
        profiles,
        model,
        reasoning,
        tier,
        current_conversation,
        model_page(profiles, model),
    )
}

async fn apply_profile_selection(
    state: &DiscordState,
    profiles: &RuntimeExecutionProfiles,
    value: &str,
    scope: ExecutionProfileScope,
    audience_id: Option<String>,
) -> Result<InteractionResponseData> {
    let values = profile_selection_parts(value, 3)?;
    let model = profile_model(profiles, values[0])?;
    let applied = apply_runtime_execution_profile(
        state,
        &ExecutionProfileSelection {
            key: model.key.clone(),
            reasoning: optional_profile_value(values[1]).map(str::to_owned),
            service_tier: optional_profile_value(values[2])
                .map(str::parse)
                .transpose()
                .map_err(anyhow::Error::msg)?,
            scope,
            audience_id,
            interface_id: Some(state.provider_id.clone()),
        },
    )
    .await?;
    Ok(applied_profile_response(&applied))
}

fn profile_panel(
    profiles: &RuntimeExecutionProfiles,
    model: &RuntimeExecutionProfileModel,
    reasoning: Option<&str>,
    service_tier: Option<ServiceTier>,
    current_conversation: bool,
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
        .collect::<BTreeMap<_, _>>();
    if providers.len() > PROFILE_PAGE_SIZE {
        return interaction_data(
            "Pi exposed more providers than Discord can display.",
            Vec::new(),
        );
    }

    let mut components = Vec::with_capacity(5);
    if providers.len() > 1 {
        let options = providers
            .into_iter()
            .map(|(provider, name)| SelectMenuOption {
                default: provider == model.provider,
                description: None,
                emoji: None,
                label: limit_discord_text(name, 100),
                value: provider.to_owned(),
            })
            .collect();
        components.push(text_select(
            PROVIDER_SELECT_ID,
            "Choose a provider",
            options,
        ));
    }

    let models = profiles
        .models
        .iter()
        .filter(|candidate| candidate.provider == model.provider)
        .collect::<Vec<_>>();
    let pages = models.len().div_ceil(PROFILE_PAGE_SIZE);
    let page = requested_page.min(pages.saturating_sub(1));
    let start = page * PROFILE_PAGE_SIZE;
    let options = models[start..models.len().min(start + PROFILE_PAGE_SIZE)]
        .iter()
        .map(|candidate| SelectMenuOption {
            default: candidate.key == model.key,
            description: Some(limit_discord_text(&candidate.id, 100)),
            emoji: None,
            label: limit_discord_text(&candidate.name, 100),
            value: candidate.key.clone(),
        })
        .collect();
    components.push(text_select(MODEL_SELECT_ID, "Choose a model", options));
    components.extend(profile_setting_selects(model, reasoning, service_tier));
    components.push(profile_action_row(
        model,
        reasoning,
        service_tier,
        current_conversation,
        page,
        pages,
    ));

    interaction_data(
        &format!(
            "**{} / {}**\nChange any setting, then choose where to apply it.",
            model.provider_name, model.name
        ),
        components,
    )
}

fn profile_setting_selects(
    model: &RuntimeExecutionProfileModel,
    reasoning: Option<&str>,
    service_tier: Option<ServiceTier>,
) -> Vec<Component> {
    let mut components = Vec::with_capacity(2);
    let reasoning_value = reasoning.unwrap_or("-");
    let selected_tier = service_tier.unwrap_or(ServiceTier::Standard);
    if !model.reasoning.is_empty() {
        let mut options = vec![SelectMenuOption {
            default: reasoning.is_none(),
            description: Some("Use the provider or model default".to_owned()),
            emoji: None,
            label: "Default".to_owned(),
            value: format!("{}|-|{selected_tier}", model.key),
        }];
        options.extend(model.reasoning.iter().map(|level| SelectMenuOption {
            default: reasoning == Some(level.as_str()),
            description: None,
            emoji: None,
            label: title_case_profile_value(level),
            value: format!("{}|{level}|{selected_tier}", model.key),
        }));
        components.push(text_select(
            REASONING_SELECT_ID,
            "Reasoning effort",
            options,
        ));
    }
    if !model.service_tiers.is_empty() {
        let options = model
            .service_tiers
            .iter()
            .map(|tier| SelectMenuOption {
                default: *tier == selected_tier,
                description: match tier {
                    ServiceTier::Standard => Some("Normal processing".to_owned()),
                    ServiceTier::Fast => Some("OpenAI priority processing".to_owned()),
                },
                emoji: None,
                label: title_case_profile_value(tier.as_str()),
                value: format!("{}|{reasoning_value}|{tier}", model.key),
            })
            .collect();
        components.push(text_select(TIER_SELECT_ID, "Speed", options));
    }
    components
}

fn profile_action_row(
    model: &RuntimeExecutionProfileModel,
    reasoning: Option<&str>,
    service_tier: Option<ServiceTier>,
    current_conversation: bool,
    page: usize,
    pages: usize,
) -> Component {
    let reasoning_value = reasoning.unwrap_or("-");
    let tier_value = if model.service_tiers.is_empty() {
        "-"
    } else {
        service_tier.unwrap_or(ServiceTier::Standard).as_str()
    };
    let selection = format!("{}|{reasoning_value}|{tier_value}", model.key);
    let mut buttons = Vec::with_capacity(4);
    if pages > 1 {
        buttons.push(profile_action_button(
            "Previous",
            format!(
                "{MODEL_PAGE_PREFIX}{}:{}",
                model.provider,
                page.saturating_sub(1)
            ),
            ButtonStyle::Secondary,
            page == 0,
        ));
        buttons.push(profile_action_button(
            "Next",
            format!("{MODEL_PAGE_PREFIX}{}:{}", model.provider, page + 1),
            ButtonStyle::Secondary,
            page + 1 == pages,
        ));
    }
    if current_conversation {
        buttons.push(profile_action_button(
            "Apply to this conversation",
            format!("{APPLY_CURRENT_PREFIX}{selection}"),
            ButtonStyle::Primary,
            false,
        ));
    }
    buttons.push(profile_action_button(
        "Use for new conversations",
        format!("{APPLY_NEW_PREFIX}{selection}"),
        if current_conversation {
            ButtonStyle::Secondary
        } else {
            ButtonStyle::Primary
        },
        false,
    ));
    Component::ActionRow(ActionRow {
        id: None,
        components: buttons,
    })
}

fn initial_profile_model(
    profiles: &RuntimeExecutionProfiles,
) -> Result<&RuntimeExecutionProfileModel> {
    profiles
        .selected
        .as_ref()
        .and_then(|selected| {
            profiles.models.iter().find(|model| {
                selected.requested.model == format!("{}/{}", model.provider, model.id)
            })
        })
        .or_else(|| profiles.models.first())
        .context("Pi has no available models")
}

fn provider_profile_model<'a>(
    profiles: &'a RuntimeExecutionProfiles,
    provider: &str,
) -> Result<&'a RuntimeExecutionProfileModel> {
    let selected = profiles
        .selected
        .as_ref()
        .map(|profile| profile.requested.model.as_str());
    profiles
        .models
        .iter()
        .find(|model| {
            model.provider == provider
                && selected == Some(format!("{}/{}", model.provider, model.id).as_str())
        })
        .or_else(|| {
            profiles
                .models
                .iter()
                .find(|model| model.provider == provider)
        })
        .context("selected provider has no available models")
}

fn selected_profile_values<'a>(
    profiles: &'a RuntimeExecutionProfiles,
    model: &RuntimeExecutionProfileModel,
) -> (Option<&'a str>, Option<ServiceTier>) {
    profiles
        .selected
        .as_ref()
        .filter(|selected| selected.requested.model == format!("{}/{}", model.provider, model.id))
        .map_or((None, None), |selected| {
            (
                selected.requested.reasoning.as_deref(),
                selected.requested.service_tier,
            )
        })
}

fn model_page(
    profiles: &RuntimeExecutionProfiles,
    selected: &RuntimeExecutionProfileModel,
) -> usize {
    profiles
        .models
        .iter()
        .filter(|model| model.provider == selected.provider)
        .position(|model| model.key == selected.key)
        .unwrap_or_default()
        / PROFILE_PAGE_SIZE
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
                .effective
                .service_tier
                .map_or_else(String::new, |value| format!(", speed **{value}**")),
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

fn profile_action_button(
    label: &str,
    custom_id: String,
    style: ButtonStyle,
    disabled: bool,
) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(custom_id),
        disabled,
        emoji: None,
        label: Some(label.to_owned()),
        style,
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

fn split_discord_message(value: &str) -> Vec<&str> {
    if value.is_empty() {
        return vec![value];
    }
    let mut chunks = Vec::new();
    let mut remaining = value;
    while !remaining.is_empty() {
        let end = discord_chunk_end(remaining);
        chunks.push(&remaining[..end]);
        remaining = &remaining[end..];
    }
    chunks
}

fn discord_chunk_end(value: &str) -> usize {
    let mut length = 0;
    let mut line_break = None;
    let mut word_break = None;
    for (index, character) in value.char_indices() {
        let character_length = character.len_utf16();
        if length + character_length > DISCORD_MESSAGE_LIMIT {
            let clean_break = line_break
                .filter(|(_, length)| *length >= DISCORD_MESSAGE_LIMIT / 2)
                .or_else(|| word_break.filter(|(_, length)| *length >= DISCORD_MESSAGE_LIMIT / 2));
            return clean_break.map_or(index, |(boundary, _)| boundary);
        }
        length += character_length;
        let boundary = index + character.len_utf8();
        if character == '\n' {
            line_break = Some((boundary, length));
        } else if character.is_whitespace() {
            word_break = Some((boundary, length));
        }
    }
    value.len()
}

async fn fetch_runtime_execution_profiles(
    state: &DiscordState,
    audience_id: Option<&str>,
) -> Result<RuntimeExecutionProfiles> {
    let mut url = reqwest::Url::parse(&format!("{}/v1/execution-profiles", state.core_url))?;
    if let Some(audience_id) = audience_id {
        url.query_pairs_mut()
            .append_pair("interface_id", &state.provider_id)
            .append_pair("audience_id", audience_id);
    }
    let request = state
        .client
        .get(url)
        .bearer_auth(&state.token)
        .header("x-carillon-provider", &state.provider_id);
    request
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
        .header("x-carillon-provider", &state.provider_id)
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
                let (outcome, native_ref, feedback) = if let Some(content) = &delivery.content {
                    match delivery.target_id.parse::<Id<ChannelMarker>>() {
                        Ok(channel_id) => {
                            let mut native_ref = None;
                            let mut failure = None;
                            for chunk in split_discord_message(content) {
                                match http.create_message(channel_id).content(chunk).await {
                                    Ok(message) => {
                                        if native_ref.is_none() {
                                            native_ref = message
                                                .model()
                                                .await
                                                .map(|value| value.id.to_string())
                                                .ok();
                                        }
                                    }
                                    Err(error) => {
                                        failure = Some(error);
                                        break;
                                    }
                                }
                            }
                            if let Some(failure) = failure {
                                warn!(error = %failure, delivery_id = delivery.id, "Discord delivery outcome uncertain");
                                (DeliveryOutcome::Uncertain, native_ref, Feedback::Uncertain)
                            } else {
                                (DeliveryOutcome::Delivered, native_ref, Feedback::Delivered)
                            }
                        }
                        Err(failure) => {
                            warn!(error = %failure, "invalid Discord delivery target");
                            (DeliveryOutcome::Uncertain, None, Feedback::Uncertain)
                        }
                    }
                } else {
                    (DeliveryOutcome::Delivered, None, Feedback::Failed)
                };
                acknowledge_core_delivery_until_recorded(&state, delivery.id, outcome, native_ref)
                    .await;
                match LoadedConfig::load(&state.config_path)
                    .and_then(|config| config.discord().cloned())
                {
                    Ok(discord) => {
                        terminal_feedback(&http, &discord, &delivery.feedback_target_id, feedback)
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
    let mut retrying = false;
    loop {
        let response = state
            .client
            .post(format!("{}/v1/events", state.core_url))
            .bearer_auth(&state.token)
            .header("x-carillon-provider", &state.provider_id)
            .json(event)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(failure) if !retrying && (failure.is_connect() || failure.is_builder()) => {
                return Err(anyhow::Error::new(failure)
                    .context("submit event to core")
                    .into());
            }
            Err(failure) => {
                retrying = true;
                warn!(error = %failure, event_id = %event.event_id, "event submission outcome uncertain; retrying");
                sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        if response.status() == StatusCode::CONFLICT {
            retrying = true;
            warn!(event_id = %event.event_id, "earlier audience turn is pending; retrying event submission");
            sleep(Duration::from_millis(250)).await;
            continue;
        }
        if response.status().is_server_error() {
            retrying = true;
            warn!(status = %response.status(), event_id = %event.event_id, "event submission outcome uncertain; retrying");
            sleep(Duration::from_millis(250)).await;
            continue;
        }
        let response = response.error_for_status().context("core rejected event")?;
        match response.json().await {
            Ok(receipt) => return Ok(receipt),
            Err(failure) => {
                retrying = true;
                warn!(error = %failure, event_id = %event.event_id, "event receipt was ambiguous; retrying");
                sleep(Duration::from_millis(250)).await;
            }
        }
    }
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

async fn acknowledge_core_delivery_until_recorded(
    state: &DiscordState,
    id: i64,
    outcome: DeliveryOutcome,
    native_ref: Option<String>,
) {
    loop {
        match acknowledge_core_delivery(state, id, outcome, native_ref.clone()).await {
            Ok(()) => return,
            Err(failure) => {
                warn!(
                    error = %failure.message,
                    delivery_id = id,
                    "retrying delivery acknowledgement"
                );
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
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
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use anyhow::{Context as _, anyhow};
    use axum::{
        Json, Router,
        extract::State,
        http::StatusCode,
        response::{IntoResponse as _, Response},
        routing::post,
    };
    use serde_json::{Value, json};
    use tempfile::tempdir;
    use tokio::{
        net::TcpListener,
        sync::{Mutex, mpsc},
        time::{sleep, timeout},
    };

    use super::{
        DISCORD_MESSAGE_LIMIT, DiscordHttp, DiscordParticipation, DiscordState, InboundMessage,
        MessagePlan, PendingMessage, ResolvedMessage, ServiceError, adopt_thread,
        clear_pending_message, deferred_model_response, message_worker, pending_message,
        profile_panel, queue_inbound_message, queue_resolved_message, remember_inbound_message,
        remember_pending_message, resolve_message, resolved_message, resume_pending_messages,
        split_discord_message, submit_event, submit_resolved_message, thread_title,
    };
    use crate::{
        config::{
            DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute, LoadedConfig,
        },
        model::{
            EventReceipt, ExternalEvent, RuntimeExecutionProfileModel, RuntimeExecutionProfiles,
            ServiceTier, SimulateRequest,
        },
        operations::initialize,
    };

    #[derive(Clone)]
    struct SubmissionTestState {
        attempts: Arc<AtomicUsize>,
    }

    async fn pending_then_ambiguous_submission(
        State(state): State<SubmissionTestState>,
        Json(event): Json<ExternalEvent>,
    ) -> Response {
        match state.attempts.fetch_add(1, Ordering::SeqCst) {
            0 => StatusCode::CONFLICT.into_response(),
            1 => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            2 => (StatusCode::OK, "not JSON").into_response(),
            _ => Json(EventReceipt {
                event_id: event.event_id,
                turn_id: "turn".to_owned(),
                state: "completed".to_owned(),
                duplicate: true,
            })
            .into_response(),
        }
    }

    async fn successful_submission(
        State(state): State<SubmissionTestState>,
        Json(event): Json<ExternalEvent>,
    ) -> Json<EventReceipt> {
        state.attempts.fetch_add(1, Ordering::SeqCst);
        Json(EventReceipt {
            event_id: event.event_id,
            turn_id: "turn".to_owned(),
            state: "completed".to_owned(),
            duplicate: false,
        })
    }

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
    fn discord_profile_panel_pages_models_and_keeps_apply_visible() -> serde_json::Result<()> {
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
            selected: None,
        };
        let first = serde_json::to_value(profile_panel(
            &profiles,
            &profiles.models[0],
            None,
            None,
            false,
            0,
        ))?;
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
            Some(3)
        );

        let second = serde_json::to_value(profile_panel(
            &profiles,
            &profiles.models[25],
            None,
            None,
            false,
            1,
        ))?;
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
    fn discord_profile_panel_shows_supported_settings_together() -> anyhow::Result<()> {
        let supported = RuntimeExecutionProfileModel {
            key: "a".repeat(64),
            provider: "openai-codex".to_owned(),
            provider_name: "OpenAI (ChatGPT Plus/Pro)".to_owned(),
            id: "gpt".to_owned(),
            name: "GPT".to_owned(),
            reasoning: vec!["low".to_owned(), "high".to_owned()],
            service_tiers: vec![ServiceTier::Standard, ServiceTier::Fast],
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
        let panel = serde_json::to_value(profile_panel(
            &profiles,
            &supported,
            Some("high"),
            Some(ServiceTier::Fast),
            true,
            0,
        ))?;
        assert_eq!(panel["components"].as_array().map(Vec::len), Some(5));
        assert_eq!(
            panel["components"][0]["components"][0]["custom_id"],
            "carillon:profile:provider"
        );
        assert_eq!(
            panel["components"][1]["components"][0]["custom_id"],
            "carillon:profile:model"
        );
        assert_eq!(
            panel["components"][2]["components"][0]["custom_id"],
            "carillon:profile:reasoning"
        );
        assert_eq!(
            panel["components"][3]["components"][0]["custom_id"],
            "carillon:profile:tier"
        );
        let speed_options = panel["components"][3]["components"][0]["options"]
            .as_array()
            .context("speed options missing")?;
        assert_eq!(speed_options.len(), 2);
        assert_eq!(speed_options[0]["label"], "Standard");
        assert_eq!(speed_options[1]["label"], "Fast");
        let buttons = panel["components"][4]["components"]
            .as_array()
            .context("profile buttons missing")?;
        assert_eq!(buttons.len(), 2);
        assert!(buttons.iter().all(|button| {
            button["custom_id"]
                .as_str()
                .is_some_and(|custom_id| custom_id.len() <= 100)
        }));

        let minimal =
            serde_json::to_value(profile_panel(&profiles, &unsupported, None, None, false, 0))?;
        assert_eq!(minimal["components"].as_array().map(Vec::len), Some(3));
        assert_eq!(
            minimal["components"][2]["components"][0]["custom_id"],
            "carillon:profile:n:unsupported|-|-"
        );
        Ok(())
    }

    #[tokio::test]
    async fn event_submission_retries_pending_and_ambiguous_attempts() -> anyhow::Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let attempts = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route("/v1/events", post(pending_then_ambiguous_submission))
            .with_state(SubmissionTestState {
                attempts: attempts.clone(),
            });
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let state = DiscordState {
            config_path: PathBuf::new(),
            core_url: format!("http://{address}"),
            provider_id: "discord".to_owned(),
            token: "token".to_owned(),
            instance_id: "instance".to_owned(),
            client: reqwest::Client::new(),
            participation_path: PathBuf::new(),
            participation: Arc::new(Mutex::new(DiscordParticipation::default())),
        };
        let receipt = submit_event(
            &state,
            &ExternalEvent {
                event_id: "event".to_owned(),
                agent_id: "default".to_owned(),
                user_id: "user".to_owned(),
                audience_id: "audience".to_owned(),
                delivery_target_id: "channel".to_owned(),
                feedback_target_id: "channel:message".to_owned(),
                content: "hello".to_owned(),
                native: json!({}),
            },
        )
        .await
        .map_err(|failure| anyhow!(failure.message))?;
        server.abort();
        assert!(receipt.duplicate);
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
        Ok(())
    }

    #[tokio::test]
    async fn discord_submission_waits_for_configuration_recovery() -> anyhow::Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let attempts = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route("/v1/events", post(successful_submission))
            .with_state(SubmissionTestState {
                attempts: attempts.clone(),
            });
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let directory = tempdir()?;
        let root = directory.path().join("install");
        let config_path = initialize(
            &root,
            directory.path(),
            "100",
            "101",
            &["102".to_owned()],
            &[DiscordRoute {
                channel_id: "103".to_owned(),
                activation: DiscordActivation::Always,
                conversation: DiscordConversation::Channel,
            }],
            49_100,
        )?;
        let mut config = LoadedConfig::load(&config_path)?;
        let discord = config
            .value
            .discord
            .as_mut()
            .context("Discord configuration missing")?;
        discord.typing = false;
        discord.reactions = false;
        let valid_config = toml::to_string_pretty(&config.value)?;
        fs::write(&config_path, "invalid = [")?;

        let state = DiscordState {
            config_path: config_path.clone(),
            core_url: format!("http://{address}"),
            provider_id: "discord-private".to_owned(),
            token: "token".to_owned(),
            instance_id: "instance".to_owned(),
            client: reqwest::Client::new(),
            participation_path: root.join("state/participation.json"),
            participation: Arc::new(Mutex::new(DiscordParticipation::default())),
        };
        let http = Arc::new(DiscordHttp::new("token".to_owned()));
        let submission = tokio::spawn(async move {
            submit_resolved_message(
                &state,
                &http,
                ResolvedMessage {
                    event: ExternalEvent {
                        event_id: "event".to_owned(),
                        agent_id: "default".to_owned(),
                        user_id: "102".to_owned(),
                        audience_id: "audience".to_owned(),
                        delivery_target_id: "103".to_owned(),
                        feedback_target_id: "103:104".to_owned(),
                        content: "hello".to_owned(),
                        native: json!({}),
                    },
                    conversation_id: "audience".to_owned(),
                },
            )
            .await;
        });
        sleep(Duration::from_millis(1_100)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
        fs::write(&config_path, valid_config)?;
        timeout(Duration::from_secs(3), submission).await??;
        server.abort();
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn discord_inbound_waits_for_configuration_recovery() -> anyhow::Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let attempts = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route("/v1/events", post(successful_submission))
            .with_state(SubmissionTestState {
                attempts: attempts.clone(),
            });
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let directory = tempdir()?;
        let root = directory.path().join("install");
        let config_path = initialize(
            &root,
            directory.path(),
            "100",
            "101",
            &["102".to_owned()],
            &[DiscordRoute {
                channel_id: "103".to_owned(),
                activation: DiscordActivation::Always,
                conversation: DiscordConversation::Channel,
            }],
            49_200,
        )?;
        let mut config = LoadedConfig::load(&config_path)?;
        let discord = config
            .value
            .discord
            .as_mut()
            .context("Discord configuration missing")?;
        discord.typing = false;
        discord.reactions = false;
        let valid_config = toml::to_string_pretty(&config.value)?;
        fs::write(&config_path, "invalid = [")?;

        let state = DiscordState {
            config_path: config_path.clone(),
            core_url: format!("http://{address}"),
            provider_id: "discord-private".to_owned(),
            token: "token".to_owned(),
            instance_id: "instance".to_owned(),
            client: reqwest::Client::new(),
            participation_path: root.join("state/participation.json"),
            participation: Arc::new(Mutex::new(DiscordParticipation::default())),
        };
        let (sender, receiver) = mpsc::channel(1);
        let worker = tokio::spawn(message_worker(
            state.clone(),
            Arc::new(DiscordHttp::new("token".to_owned())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(Mutex::new(BTreeMap::new())),
            receiver,
        ));
        sender
            .send(InboundMessage {
                request: SimulateRequest {
                    event_id: "104".to_owned(),
                    guild_id: Some("100".to_owned()),
                    user_id: "102".to_owned(),
                    channel_id: "103".to_owned(),
                    parent_channel_id: None,
                    content: "hello".to_owned(),
                },
                native: json!({}),
            })
            .await?;
        sleep(Duration::from_millis(1_100)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
        let persisted: DiscordParticipation =
            serde_json::from_slice(&fs::read(&state.participation_path)?)?;
        assert!(matches!(
            persisted.pending_messages.first(),
            Some(PendingMessage::Inbound(inbound)) if inbound.request.event_id == "104"
        ));
        fs::write(&config_path, valid_config)?;
        timeout(Duration::from_secs(3), async {
            while attempts.load(Ordering::SeqCst) == 0 {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        worker.abort();
        server.abort();
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn pending_message_survives_restart_until_submission() -> anyhow::Result<()> {
        let directory = tempdir()?;
        let participation_path = directory.path().join("participation.json");
        let state = DiscordState {
            config_path: PathBuf::new(),
            core_url: String::new(),
            provider_id: "discord".to_owned(),
            token: "token".to_owned(),
            instance_id: "instance".to_owned(),
            client: reqwest::Client::new(),
            participation_path: participation_path.clone(),
            participation: Arc::new(Mutex::new(DiscordParticipation::default())),
        };
        let plan = MessagePlan {
            event_id: "message".to_owned(),
            guild_id: "guild".to_owned(),
            user_id: "user".to_owned(),
            agent_id: "default".to_owned(),
            source_channel_id: "channel".to_owned(),
            source_message_id: "message".to_owned(),
            route_channel_id: "channel".to_owned(),
            current_channel_id: "channel".to_owned(),
            content: "hello".to_owned(),
            create_thread: false,
            adopt_thread: false,
            native: json!({ "message": "message" }),
        };

        remember_pending_message(&state, &plan).await?;
        assert!(pending_message(&state, "message").await);
        let persisted: DiscordParticipation =
            serde_json::from_slice(&fs::read(&participation_path)?)?;
        assert_eq!(
            persisted.pending_messages.first(),
            Some(&PendingMessage::Planned(plan.clone()))
        );

        let restarted = DiscordState {
            participation: Arc::new(Mutex::new(persisted)),
            ..state
        };
        clear_pending_message(&restarted, "message").await?;
        let completed: DiscordParticipation =
            serde_json::from_slice(&fs::read(&participation_path)?)?;
        assert!(completed.pending_messages.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn failed_message_intent_write_does_not_change_live_participation() -> anyhow::Result<()>
    {
        let directory = tempdir()?;
        let state = DiscordState {
            config_path: PathBuf::new(),
            core_url: String::new(),
            provider_id: "discord".to_owned(),
            token: "token".to_owned(),
            instance_id: "instance".to_owned(),
            client: reqwest::Client::new(),
            participation_path: directory.path().join("missing/participation.json"),
            participation: Arc::new(Mutex::new(DiscordParticipation::default())),
        };
        let plan = MessagePlan {
            event_id: "message".to_owned(),
            guild_id: "guild".to_owned(),
            user_id: "user".to_owned(),
            agent_id: "default".to_owned(),
            source_channel_id: "channel".to_owned(),
            source_message_id: "message".to_owned(),
            route_channel_id: "channel".to_owned(),
            current_channel_id: "channel".to_owned(),
            content: "hello".to_owned(),
            create_thread: true,
            adopt_thread: false,
            native: json!({}),
        };

        assert!(remember_pending_message(&state, &plan).await.is_err());
        assert!(state.participation.lock().await.pending_messages.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn failed_thread_adoption_does_not_change_live_participation() -> anyhow::Result<()> {
        let directory = tempdir()?;
        let state = DiscordState {
            config_path: PathBuf::new(),
            core_url: String::new(),
            provider_id: "discord".to_owned(),
            token: "token".to_owned(),
            instance_id: "instance".to_owned(),
            client: reqwest::Client::new(),
            participation_path: directory.path().join("missing/participation.json"),
            participation: Arc::new(Mutex::new(DiscordParticipation::default())),
        };

        assert!(adopt_thread(&state, "thread", "parent").await.is_err());
        assert!(state.participation.lock().await.threads.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn persisted_message_submission_recovers_after_restart() -> anyhow::Result<()> {
        let directory = tempdir()?;
        let root = directory.path().join("install");
        let config_path = initialize(
            &root,
            directory.path(),
            "100",
            "101",
            &["102".to_owned()],
            &[DiscordRoute {
                channel_id: "103".to_owned(),
                activation: DiscordActivation::Always,
                conversation: DiscordConversation::Channel,
            }],
            49_300,
        )?;
        let mut config = LoadedConfig::load(&config_path)?;
        let discord = config
            .value
            .discord
            .as_mut()
            .context("Discord configuration missing")?;
        discord.typing = false;
        discord.reactions = false;
        fs::write(&config_path, toml::to_string_pretty(&config.value)?)?;

        let probe = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let address = probe.local_addr()?;
        drop(probe);
        let attempts = Arc::new(AtomicUsize::new(0));
        let participation_path = directory.path().join("participation.json");
        let state = DiscordState {
            config_path,
            core_url: format!("http://{address}"),
            provider_id: "discord-private".to_owned(),
            token: "token".to_owned(),
            instance_id: "instance".to_owned(),
            client: reqwest::Client::new(),
            participation_path,
            participation: Arc::new(Mutex::new(DiscordParticipation::default())),
        };
        let inbound = InboundMessage {
            request: SimulateRequest {
                event_id: "104".to_owned(),
                guild_id: Some("100".to_owned()),
                user_id: "102".to_owned(),
                channel_id: "103".to_owned(),
                parent_channel_id: None,
                content: "hello".to_owned(),
            },
            native: json!({}),
        };
        assert!(remember_inbound_message(&state, &inbound).await?);
        let persisted: DiscordParticipation =
            serde_json::from_slice(&fs::read(&state.participation_path)?)?;
        assert_eq!(
            persisted.pending_messages.first(),
            Some(&PendingMessage::Inbound(inbound))
        );
        let restarted = DiscordState {
            participation: Arc::new(Mutex::new(persisted)),
            ..state
        };
        resume_pending_messages(
            &restarted,
            &Arc::new(DiscordHttp::new("token".to_owned())),
            &Arc::new(Mutex::new(BTreeMap::new())),
            &Arc::new(Mutex::new(BTreeMap::new())),
        )
        .await;
        sleep(Duration::from_millis(100)).await;
        assert!(pending_message(&restarted, "104").await);

        let listener = TcpListener::bind(address).await?;
        let router = Router::new()
            .route("/v1/events", post(successful_submission))
            .with_state(SubmissionTestState {
                attempts: attempts.clone(),
            });
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        timeout(Duration::from_secs(3), async {
            while pending_message(&restarted, "104").await {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        server.abort();

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn discord_parent_resolution_queues_are_channel_isolated_and_fifo() -> anyhow::Result<()> {
        let inbound = |event_id: &str, channel_id: &str| InboundMessage {
            request: SimulateRequest {
                event_id: event_id.to_owned(),
                guild_id: Some("guild".to_owned()),
                user_id: "user".to_owned(),
                channel_id: channel_id.to_owned(),
                parent_channel_id: None,
                content: "hello".to_owned(),
            },
            native: json!({}),
        };
        let mut queues = BTreeMap::new();

        assert!(queue_inbound_message(
            &mut queues,
            inbound("a-1", "thread-a")
        ));
        assert!(!queue_inbound_message(
            &mut queues,
            inbound("a-2", "thread-a")
        ));
        assert!(queue_inbound_message(
            &mut queues,
            inbound("b-1", "thread-b")
        ));
        let first_a = queues
            .get_mut("thread-a")
            .context("thread a queue")?
            .pop_front()
            .context("first thread a message")?;
        assert_eq!(first_a.request.event_id, "a-1");
        assert_eq!(
            queues
                .get_mut("thread-b")
                .context("thread b queue")?
                .pop_front()
                .context("thread b message")?
                .request
                .event_id,
            "b-1"
        );
        assert_eq!(
            queues
                .get_mut("thread-a")
                .context("thread a queue")?
                .pop_front()
                .context("second thread a message")?
                .request
                .event_id,
            "a-2"
        );
        Ok(())
    }

    #[test]
    fn audience_submission_queues_are_independent_and_fifo() -> anyhow::Result<()> {
        let resolved = |event_id: &str, audience: &str| ResolvedMessage {
            event: ExternalEvent {
                event_id: event_id.to_owned(),
                agent_id: "default".to_owned(),
                user_id: "user".to_owned(),
                audience_id: audience.to_owned(),
                delivery_target_id: "channel".to_owned(),
                feedback_target_id: "channel:message".to_owned(),
                content: "hello".to_owned(),
                native: json!({}),
            },
            conversation_id: audience.to_owned(),
        };
        let mut queues = BTreeMap::new();

        assert!(queue_resolved_message(&mut queues, resolved("a-1", "a")));
        assert!(!queue_resolved_message(&mut queues, resolved("a-2", "a")));
        assert!(queue_resolved_message(&mut queues, resolved("b-1", "b")));
        assert_eq!(queues.len(), 2);
        let audience_a = queues.get_mut("a").context("audience a queue")?;
        assert_eq!(
            audience_a
                .pop_front()
                .context("first audience a event")?
                .event
                .event_id,
            "a-1"
        );
        assert_eq!(
            audience_a
                .pop_front()
                .context("second audience a event")?
                .event
                .event_id,
            "a-2"
        );
        assert_eq!(
            queues
                .get_mut("b")
                .context("audience b queue")?
                .pop_front()
                .context("audience b event")?
                .event
                .event_id,
            "b-1"
        );
        Ok(())
    }

    #[test]
    fn discord_messages_split_on_clean_boundaries() {
        let prose = format!("{}\n{}", "a".repeat(1_200), "word ".repeat(300));
        let prose_chunks = split_discord_message(&prose);
        let unicode = "🦀".repeat(1_001);
        let unicode_chunks = split_discord_message(&unicode);

        assert_eq!(prose_chunks.len(), 2);
        assert!(prose_chunks[0].ends_with('\n'));
        assert_eq!(prose_chunks.concat(), prose);
        assert_eq!(unicode_chunks.concat(), unicode);
        assert!(
            prose_chunks
                .iter()
                .chain(unicode_chunks.iter())
                .all(|chunk| chunk.encode_utf16().count() <= DISCORD_MESSAGE_LIMIT)
        );
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
