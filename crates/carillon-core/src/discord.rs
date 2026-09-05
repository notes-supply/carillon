//! The Discord interface provider: platform authentication, guild/user/route
//! admission, thread routing, feedback, and message delivery. Core knows none
//! of this; the provider talks to core only through [`CoreClient`].

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
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
use reqwest::{Client, Method, RequestBuilder};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::mpsc,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tracing::{error, info, warn};
use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use twilight_http::{
    Client as DiscordHttp, error::ErrorType as DiscordErrorType,
    request::channel::reaction::RequestReactionType, response::StatusCode as DiscordStatusCode,
};
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
        ProviderRole, RELEASE_VERSION, read_token_file, token_matches,
    },
    model::{
        Delivery, DeliveryAck, DeliveryOutcome, DeliveryReconciliation, EventReceipt,
        ExternalEvent, Health, INSTANCE_HEADER, PROVIDER_HEADER, SimulateRequest, SimulateResponse,
    },
};

#[derive(Debug)]
struct ServiceError {
    status: StatusCode,
    message: String,
}

impl ServiceError {
    fn transient(message: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message,
        }
    }

    fn protocol(message: String) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message,
        }
    }

    /// Whether the failure is worth retrying: core unreachable or failing,
    /// rather than rejecting the request or violating its wire contract.
    fn is_transient(&self) -> bool {
        self.status.is_server_error() && self.status != StatusCode::BAD_GATEWAY
    }
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

/// The provider's authenticated client for core's provider API.
#[derive(Clone)]
struct CoreClient {
    http: Client,
    base_url: String,
    token: String,
    provider_id: String,
    /// Fresh per process start; core releases delivery claims held under
    /// other, presumed dead, instance identities during reconciliation.
    instance_id: String,
}

impl CoreClient {
    fn new(config: &LoadedConfig, provider_id: String, token: String) -> Self {
        Self {
            http: Client::new(),
            base_url: format!("http://{}", config.value.core.listen),
            token,
            provider_id,
            instance_id: Uuid::now_v7().to_string(),
        }
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .header(PROVIDER_HEADER, &self.provider_id)
            .header(INSTANCE_HEADER, &self.instance_id)
    }

    async fn submit_event(&self, event: &ExternalEvent) -> Result<EventReceipt, ServiceError> {
        let request = self.request(Method::POST, "/v1/events").json(event);
        core_json(request, "submit event to core").await
    }

    async fn claim_delivery(&self) -> Result<Option<Delivery>, ServiceError> {
        let request = self.request(Method::GET, "/v1/deliveries");
        core_json(request, "claim core delivery").await
    }

    async fn acknowledge_delivery(
        &self,
        id: i64,
        outcome: DeliveryOutcome,
        native_ref: Option<String>,
    ) -> Result<(), ServiceError> {
        let request = self
            .request(Method::POST, &format!("/v1/deliveries/{id}/ack"))
            .json(&DeliveryAck {
                native_ref,
                outcome,
            });
        core_send(request, "acknowledge core delivery").await?;
        Ok(())
    }

    async fn reconcile(&self) -> Result<DeliveryReconciliation, ServiceError> {
        let request = self.request(Method::POST, "/v1/deliveries/reconcile");
        core_json(request, "reconcile abandoned delivery claims").await
    }
}

async fn core_send(
    request: RequestBuilder,
    action: &str,
) -> Result<reqwest::Response, ServiceError> {
    let response = request
        .send()
        .await
        .map_err(|error| ServiceError::transient(format!("{action}: {error}")))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let detail = response.text().await.unwrap_or_default();
    Err(ServiceError {
        status,
        message: if detail.is_empty() {
            format!("{action}: core responded with {status}")
        } else {
            format!("{action}: {detail}")
        },
    })
}

async fn core_json<T: DeserializeOwned>(
    request: RequestBuilder,
    action: &str,
) -> Result<T, ServiceError> {
    let bytes = core_send(request, action)
        .await?
        .bytes()
        .await
        .map_err(|error| ServiceError::transient(format!("read {action}: {error}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ServiceError::protocol(format!("decode {action}: {error}")))
}

/// Durable record of the threads this provider participates in.
///
/// `threads` maps a thread to the route channel that admitted it, so later
/// thread messages resolve and stay activated without Discord lookups.
/// `created_threads` maps a root message to the thread already created for
/// it, so a retried message never creates a second thread.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct ParticipationState {
    threads: BTreeMap<String, String>,
    #[serde(default)]
    created_threads: BTreeMap<String, String>,
}

struct Participation {
    path: PathBuf,
    state: Mutex<ParticipationState>,
}

impl Participation {
    fn load(path: PathBuf) -> Result<Self> {
        fs::create_dir_all(path.parent().context("participation path has no parent")?)?;
        let state = if path.exists() {
            fs::File::open(&path)?.sync_all()?;
            crate::filesystem::sync_ancestors(
                path.parent().context("participation path has no parent")?,
            )?;
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

    fn lock(&self) -> MutexGuard<'_, ParticipationState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn parent_route(&self, channel_id: &str) -> Option<String> {
        self.lock().threads.get(channel_id).cloned()
    }

    fn created_thread(&self, message_id: &str) -> Option<String> {
        self.lock().created_threads.get(message_id).cloned()
    }

    fn adopt(&self, thread_id: &str, route_channel_id: &str) -> Result<()> {
        let mut state = self.lock();
        let mut next = state.clone();
        next.threads
            .insert(thread_id.to_owned(), route_channel_id.to_owned());
        self.persist(&next)?;
        *state = next;
        Ok(())
    }

    fn remember_created(
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
        crate::filesystem::sync_ancestors(
            self.path
                .parent()
                .context("participation path has no parent")?,
        )?;
        Ok(())
    }
}

#[derive(Clone)]
struct DiscordState {
    discord: DiscordConfig,
    health: Health,
    core: CoreClient,
    participation: Arc<Participation>,
}

#[derive(Clone, Debug)]
struct InboundMessage {
    request: SimulateRequest,
    native: Value,
}

/// An admitted, activated message and what conversation handling it needs.
#[derive(Clone, Debug)]
struct MessagePlan {
    /// The Discord message ID, which is also the provider event ID.
    message_id: String,
    guild_id: String,
    user_id: String,
    agent_id: String,
    /// The channel or thread the message arrived in.
    channel_id: String,
    /// The configured route channel that admitted it (the parent channel for
    /// thread children).
    route_channel_id: String,
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

pub async fn serve(
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
    // Establish instance exclusivity before touching provider state or claims.
    let address = provider.listen_address()?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("bind Discord interface provider to {address}"))?;
    let participation = Arc::new(Participation::load(
        config
            .value
            .data_dir
            .join("components")
            .join(&provider.id)
            .join("participation.json"),
    )?);
    let state = DiscordState {
        discord,
        health: Health {
            id: provider.id.clone(),
            role: provider.role.as_str().to_owned(),
            version: RELEASE_VERSION.to_owned(),
            status: "ready".to_owned(),
        },
        core: CoreClient::new(&config, provider.id.clone(), token),
        participation,
    };
    let reconciled = loop {
        match state.core.reconcile().await {
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
        axum::serve(listener, router).await?;
        return Ok(());
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
    info!(url = %provider.url, deterministic, "Discord interface provider ready");
    tokio::select! {
        result = axum::serve(listener, router).into_future() => result.map_err(Into::into),
        result = gateway_loop(state, discord_token, messages) => {
            result.context("Discord gateway stopped")
        },
        result = delivery_loop(delivery_state, http) => {
            result.context("Discord delivery worker stopped")
        },
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

async fn discord_health(State(state): State<DiscordState>) -> Json<Health> {
    Json(state.health)
}

async fn simulate(
    State(state): State<DiscordState>,
    headers: HeaderMap,
    Json(request): Json<SimulateRequest>,
) -> Result<Response, ServiceError> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !token_matches(&crate::config::hash_token(&state.core.token), token) {
        return Err(ServiceError {
            status: StatusCode::FORBIDDEN,
            message: "simulation requires the interface provider credential".to_owned(),
        });
    }
    admit_identity(
        &state.discord,
        request.guild_id.as_deref(),
        &request.user_id,
    )?;
    let Some(plan) = resolve_message(
        &state.discord,
        request,
        Value::String("deterministic-local-discord".to_owned()),
        &state.participation.lock(),
    )?
    else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let resolved = prepare_deterministic_conversation(&state, plan)?;
    let mut feedback = Vec::new();
    if state.discord.reactions {
        feedback.push("processing".to_owned());
    }
    let receipt = state.core.submit_event(&resolved.event).await?;
    let delivery = if receipt.duplicate {
        None
    } else {
        let delivery = timeout(Duration::from_secs(10), async {
            loop {
                if let Some(delivery) = state.core.claim_delivery().await? {
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
        state
            .core
            .acknowledge_delivery(
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
            if admit_identity(
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

/// Decides what to do with an admitted message.
///
/// `Err` means the channel is not routed (rejected); `Ok(None)` means the
/// route ignores this message (no activation or empty content). The gateway
/// silently drops both; simulation reports the rejection.
fn resolve_message(
    config: &DiscordConfig,
    request: SimulateRequest,
    native: Value,
    participation: &ParticipationState,
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

fn prepare_deterministic_conversation(
    state: &DiscordState,
    plan: MessagePlan,
) -> Result<ResolvedMessage, ServiceError> {
    let target = if plan.create_thread {
        plan.message_id.clone()
    } else {
        plan.channel_id.clone()
    };
    if plan.create_thread || plan.adopt_thread {
        state.participation.adopt(&target, &plan.route_channel_id)?;
    }
    Ok(resolved_message(plan, target))
}

async fn prepare_discord_conversation(
    state: &DiscordState,
    http: &DiscordHttp,
    plan: MessagePlan,
) -> Result<ResolvedMessage> {
    let target = if plan.create_thread {
        let thread = if let Some(existing) = state.participation.created_thread(&plan.message_id) {
            existing
        } else {
            create_conversation_thread(http, &plan).await?
        };
        state
            .participation
            .remember_created(&plan.message_id, &thread, &plan.route_channel_id)?;
        thread
    } else {
        plan.channel_id.clone()
    };
    if plan.adopt_thread {
        state.participation.adopt(&target, &plan.route_channel_id)?;
    }
    Ok(resolved_message(plan, target))
}

async fn create_conversation_thread(http: &DiscordHttp, plan: &MessagePlan) -> Result<String> {
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
async fn discord_parent_channel(
    http: &DiscordHttp,
    state: &DiscordState,
    channel_id: &str,
) -> Result<Option<String>> {
    if state
        .discord
        .routes
        .iter()
        .any(|route| route.channel_id == channel_id)
    {
        return Ok(None);
    }
    if let Some(parent) = state.participation.parent_route(channel_id) {
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

fn discord_status_is_terminal(status: DiscordStatusCode) -> bool {
    status.is_client_error() && status != DiscordStatusCode::TOO_MANY_REQUESTS
}

fn discord_failure_is_terminal(failure: &anyhow::Error) -> bool {
    failure
        .downcast_ref::<twilight_http::Error>()
        .is_some_and(|failure| match failure.kind() {
            DiscordErrorType::Response { status, .. } => discord_status_is_terminal(*status),
            DiscordErrorType::BuildingRequest
            | DiscordErrorType::CreatingHeader { .. }
            | DiscordErrorType::Json
            | DiscordErrorType::Unauthorized
            | DiscordErrorType::Validation => true,
            _ => false,
        })
}

async fn message_worker(
    state: DiscordState,
    http: Arc<DiscordHttp>,
    mut messages: mpsc::Receiver<InboundMessage>,
) {
    'messages: while let Some(mut inbound) = messages.recv().await {
        let discord = &state.discord;
        inbound.request.parent_channel_id = loop {
            match discord_parent_channel(&http, &state, &inbound.request.channel_id).await {
                Ok(parent) => break parent,
                Err(failure) if discord_failure_is_terminal(&failure) => {
                    let source = format!(
                        "{}:{}",
                        inbound.request.channel_id, inbound.request.event_id
                    );
                    terminal_feedback(&http, discord, &source, Feedback::Failed).await;
                    warn!(error = %failure, "Discord message route is unavailable");
                    continue 'messages;
                }
                Err(failure) => {
                    warn!(error = %failure, "waiting to resolve Discord message route");
                    sleep(Duration::from_millis(250)).await;
                }
            }
        };
        let Ok(Some(plan)) = resolve_message(
            discord,
            inbound.request,
            inbound.native,
            &state.participation.lock(),
        ) else {
            continue;
        };
        let source = format!("{}:{}", plan.channel_id, plan.message_id);
        let resolved = loop {
            match prepare_discord_conversation(&state, &http, plan.clone()).await {
                Ok(resolved) => break resolved,
                Err(failure) if discord_failure_is_terminal(&failure) => {
                    terminal_feedback(&http, discord, &source, Feedback::Failed).await;
                    warn!(error = %failure, "Discord conversation cannot be prepared");
                    continue 'messages;
                }
                Err(failure) => {
                    warn!(error = %failure, "waiting to prepare Discord conversation");
                    sleep(Duration::from_millis(250)).await;
                }
            }
        };
        processing_feedback(&http, discord, &resolved.event.feedback_target_id).await;
        let typing = start_typing(http.clone(), discord, &resolved.event.delivery_target_id).await;
        let receipt = submit_event_with_retry(&state.core, &resolved.event).await;
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
    let emoji = DiscordEmoji::parse(configured)?;
    http.create_reaction(channel, message, &reaction_type(&emoji))
        .await?;
    Ok(())
}

async fn delete_feedback_reaction(
    http: &DiscordHttp,
    target: &str,
    configured: &str,
) -> Result<()> {
    let (channel, message) = feedback_ids(target)?;
    let emoji = DiscordEmoji::parse(configured)?;
    http.delete_current_user_reaction(channel, message, &reaction_type(&emoji))
        .await?;
    Ok(())
}

fn reaction_type(emoji: &DiscordEmoji) -> RequestReactionType<'_> {
    match emoji {
        DiscordEmoji::Unicode(name) => RequestReactionType::Unicode { name },
        DiscordEmoji::Custom { name, id } => RequestReactionType::Custom {
            id: Id::<EmojiMarker>::new(*id),
            name: Some(name),
        },
    }
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

async fn delivery_loop(state: DiscordState, http: Arc<DiscordHttp>) -> Result<()> {
    loop {
        let delivery = match state.core.claim_delivery().await {
            Ok(Some(delivery)) => delivery,
            Ok(None) => {
                sleep(Duration::from_millis(500)).await;
                continue;
            }
            // A briefly unreachable core must not cost the gateway session,
            // so transient claim failures wait in place.
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, "waiting to claim Discord delivery");
                reconcile_after_claim_failure(&state, &http).await?;
                continue;
            }
            Err(failure) => bail!("delivery claim rejected: {}", failure.message),
        };
        let (outcome, native_ref, feedback) = attempt_delivery(&http, &delivery).await;
        // The Discord outcome is known at this point. Abandoning the
        // acknowledgement would let a restart downgrade it to uncertain, so
        // transient core failures are waited out while the claim is held.
        loop {
            match state
                .core
                .acknowledge_delivery(delivery.id, outcome, native_ref.clone())
                .await
            {
                Ok(()) => break,
                Err(failure) if failure.is_transient() => {
                    warn!(
                        error = %failure.message,
                        delivery_id = delivery.id,
                        "waiting to acknowledge Discord delivery"
                    );
                    sleep(Duration::from_secs(1)).await;
                }
                Err(failure) => bail!("delivery acknowledgement rejected: {}", failure.message),
            }
        }
        terminal_feedback(
            &http,
            &state.discord,
            &delivery.feedback_target_id,
            feedback,
        )
        .await;
    }
}

/// A failed claim response is ambiguous: core may have committed the claim
/// before disconnecting. Reconciliation preserves this instance's claim so
/// polling can recover it, while surfacing claims abandoned by older instances.
async fn reconcile_after_claim_failure(state: &DiscordState, http: &DiscordHttp) -> Result<()> {
    loop {
        match state.core.reconcile().await {
            Ok(reconciled) => {
                for target in reconciled.feedback_target_ids {
                    terminal_feedback(http, &state.discord, &target, Feedback::Uncertain).await;
                }
                return Ok(());
            }
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, "waiting to reconcile Discord delivery claims");
                sleep(Duration::from_secs(1)).await;
            }
            Err(failure) => bail!("delivery reconciliation rejected: {}", failure.message),
        }
    }
}

async fn attempt_delivery(
    http: &DiscordHttp,
    delivery: &Delivery,
) -> (DeliveryOutcome, Option<String>, Feedback) {
    let channel = match delivery.target_id.parse::<Id<ChannelMarker>>() {
        Ok(channel) => channel,
        Err(failure) => {
            warn!(error = %failure, "invalid Discord delivery target");
            return (DeliveryOutcome::Uncertain, None, Feedback::Uncertain);
        }
    };
    match send_discord_message(http, channel, &delivery.content).await {
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

async fn send_discord_message(
    http: &DiscordHttp,
    channel_id: Id<ChannelMarker>,
    content: &str,
) -> Result<String> {
    let chunks = discord_chunks(content);
    if chunks.is_empty() {
        bail!("Discord delivery content is empty");
    }
    let mut ids = Vec::with_capacity(chunks.len());
    for chunk in chunks {
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

async fn submit_event_with_retry(
    core: &CoreClient,
    event: &ExternalEvent,
) -> Result<EventReceipt, ServiceError> {
    loop {
        match core.submit_event(event).await {
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, event_id = %event.event_id, "waiting to submit Discord event");
                sleep(Duration::from_millis(250)).await;
            }
            outcome => return outcome,
        }
    }
}

/// Fail-closed identity admission: the configured guild and an allowlisted
/// user, before any further handling.
fn admit_identity(
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

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use anyhow::{Context as _, anyhow};
    use axum::{
        Json, Router,
        body::Body,
        http::{Method, Request, StatusCode, Uri},
        response::IntoResponse as _,
        routing::{get, post},
    };
    use serde_json::Value;
    use tower::ServiceExt as _;
    use twilight_http::response::StatusCode as DiscordStatusCode;

    use super::{
        CoreClient, DiscordState, MessagePlan, Participation, ParticipationState, ServiceError,
        delivery_loop, discord_chunks, discord_router, discord_status_is_terminal, resolve_message,
        resolved_message, send_discord_message, submit_event_with_retry, thread_title,
    };
    use crate::{
        config::{DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute},
        model::{
            DeliveryReconciliation, EventReceipt, ExternalEvent, Health, SimulateRequest, TurnState,
        },
    };

    #[test]
    fn participation_persistence_retries_without_bypassing_activation() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("participation.json");
        let participation = Participation::load(path.clone())?;
        if let Err(failure) = participation.adopt("400", "300") {
            eprintln!("adoption persistence failed: {failure}");
            assert_eq!(participation.parent_route("400"), None);
            participation.adopt("400", "300")?;
        }
        if let Err(failure) = participation.remember_created("500", "500", "300") {
            eprintln!("creation persistence failed: {failure}");
            assert_eq!(participation.created_thread("500"), None);
            assert_eq!(participation.parent_route("500"), None);
            assert_eq!(participation.parent_route("400").as_deref(), Some("300"));
            participation.remember_created("500", "500", "300")?;
        }
        let recovered = Participation::load(path)?;
        assert_eq!(recovered.created_thread("500").as_deref(), Some("500"));
        let config = discord_config(DiscordActivation::Mention, DiscordConversation::Thread);
        let plan = resolve_message(
            &config,
            SimulateRequest {
                event_id: "unmentioned".to_owned(),
                guild_id: Some("100".to_owned()),
                user_id: "200".to_owned(),
                channel_id: "400".to_owned(),
                parent_channel_id: recovered.parent_route("400"),
                content: "continue without a mention".to_owned(),
            },
            serde_json::json!({}),
            &recovered.lock(),
        )
        .map_err(|error| anyhow!(error.message))?
        .context("lost adopted-thread activation")?;
        assert!(!plan.adopt_thread);
        assert!(!plan.create_thread);
        Ok(())
    }

    #[test]
    fn failed_participation_write_leaves_memory_and_disk_unchanged() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("participation.json");
        let participation = Participation::load(path.clone())?;
        participation.adopt("400", "300")?;
        let original = std::fs::read(&path)?;
        let temporary = path.with_extension("json.tmp");
        std::fs::create_dir(&temporary)?;
        assert!(participation.adopt("401", "300").is_err());
        assert!(participation.remember_created("500", "500", "300").is_err());
        assert_eq!(participation.parent_route("401"), None);
        assert_eq!(participation.created_thread("500"), None);
        assert_eq!(std::fs::read(&path)?, original);
        std::fs::remove_dir(temporary)?;
        participation.adopt("401", "300")?;
        participation.remember_created("500", "500", "300")?;
        let recovered = Participation::load(path)?;
        assert_eq!(recovered.parent_route("401").as_deref(), Some("300"));
        assert_eq!(recovered.created_thread("500").as_deref(), Some("500"));
        Ok(())
    }

    fn core_client(base_url: String) -> CoreClient {
        CoreClient {
            http: reqwest::Client::new(),
            base_url,
            token: "token".to_owned(),
            provider_id: "discord".to_owned(),
            instance_id: "instance".to_owned(),
        }
    }

    fn discord_state(core: CoreClient) -> DiscordState {
        DiscordState {
            discord: discord_config(DiscordActivation::Always, DiscordConversation::Channel),
            health: Health {
                id: "discord".to_owned(),
                role: "interface".to_owned(),
                version: "test".to_owned(),
                status: "ready".to_owned(),
            },
            core,
            participation: Arc::new(Participation {
                path: PathBuf::from("unused"),
                state: Mutex::new(ParticipationState::default()),
            }),
        }
    }

    #[tokio::test]
    async fn ambiguous_thread_creation_recovers_native_thread_before_retrying() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let mut state = discord_state(core_client("http://127.0.0.1:1".to_owned()));
        state.participation = Arc::new(Participation::load(
            directory
                .path()
                .join("components/custom-discord/participation.json"),
        )?);
        let plan = MessagePlan {
            message_id: "200".to_owned(),
            guild_id: "1".to_owned(),
            user_id: "2".to_owned(),
            agent_id: "agent".to_owned(),
            channel_id: "100".to_owned(),
            route_channel_id: "100".to_owned(),
            content: "hello".to_owned(),
            create_thread: true,
            adopt_thread: false,
            native: Value::Null,
        };
        let creates = Arc::new(AtomicUsize::new(0));
        let get_creates = creates.clone();
        let post_creates = creates.clone();
        let app = Router::new()
            .route("/api/v10/channels/200", get(move || {
                let exists = get_creates.load(Ordering::SeqCst) > 0;
                async move {
                    if exists {
                        Json(serde_json::json!({"id":"200", "type":11, "parent_id":"100"})).into_response()
                    } else {
                        (StatusCode::NOT_FOUND, Json(serde_json::json!({"code":10003, "message":"Unknown Channel"}))).into_response()
                    }
                }
            }))
            .route("/api/v10/channels/100/messages/200/threads", post(move || {
                let attempt = post_creates.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        // Discord committed the thread, but its successful response is undecodable.
                        (StatusCode::CREATED, "{broken response").into_response()
                    } else {
                        (StatusCode::BAD_REQUEST, Json(serde_json::json!({"code":160_004, "message":"A thread has already been created for this message"}))).into_response()
                    }
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let http = twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(listener.local_addr()?.to_string(), true)
            .ratelimiter(None)
            .build();
        let mut services = tokio::task::JoinSet::new();
        services.spawn(async move { axum::serve(listener, app).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            let failure = super::prepare_discord_conversation(&state, &http, plan.clone())
                .await
                .err()
                .context("undecodable create should fail")?;
            assert!(!super::discord_failure_is_terminal(&failure));
            assert_eq!(state.participation.created_thread("200"), None);
            let recovered =
                super::prepare_discord_conversation(&state, &http, plan.clone()).await?;
            assert_eq!(recovered.event.event_id, "200");
            assert_eq!(recovered.event.delivery_target_id, "200");
            assert_eq!(recovered.event.audience_id, "discord-1-200");
            state.participation = Arc::new(Participation::load(state.participation.path.clone())?);
            assert_eq!(
                state.participation.created_thread("200").as_deref(),
                Some("200")
            );
            assert_eq!(
                state.participation.parent_route("200").as_deref(),
                Some("100")
            );
            super::prepare_discord_conversation(&state, &http, plan).await?;
            assert_eq!(creates.load(Ordering::SeqCst), 1);
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        services.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn thread_lookup_failures_do_not_speculatively_create() -> anyhow::Result<()> {
        for status in [StatusCode::FORBIDDEN, StatusCode::SERVICE_UNAVAILABLE] {
            let creates = Arc::new(AtomicUsize::new(0));
            let seen = creates.clone();
            let app = Router::new().fallback(move |method: Method| {
                if method == Method::POST {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
                async move {
                    (
                        status,
                        Json(serde_json::json!({"code":50001, "message":"Unavailable"})),
                    )
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let http = twilight_http::Client::builder()
                .token("test-only".to_owned())
                .proxy(listener.local_addr()?.to_string(), true)
                .ratelimiter(None)
                .build();
            let mut services = tokio::task::JoinSet::new();
            services.spawn(async move { axum::serve(listener, app).await });
            let plan = MessagePlan {
                message_id: "200".to_owned(),
                guild_id: "1".to_owned(),
                user_id: "2".to_owned(),
                agent_id: "agent".to_owned(),
                channel_id: "100".to_owned(),
                route_channel_id: "100".to_owned(),
                content: "hello".to_owned(),
                create_thread: true,
                adopt_thread: false,
                native: Value::Null,
            };
            let failure = tokio::time::timeout(
                Duration::from_secs(3),
                super::create_conversation_thread(&http, &plan),
            )
            .await?
            .err()
            .context("lookup should fail")?;
            assert_eq!(
                super::discord_failure_is_terminal(&failure),
                status == StatusCode::FORBIDDEN
            );
            assert_eq!(creates.load(Ordering::SeqCst), 0);
            services.shutdown().await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn production_discord_does_not_expose_simulation() -> anyhow::Result<()> {
        let state = discord_state(core_client("http://127.0.0.1:1".to_owned()));
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::POST;
        *request.uri_mut() = Uri::from_static("/v1/simulate");
        let response = discord_router(state, false).oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[tokio::test]
    async fn discord_retries_ambiguous_submission_with_the_same_event_id() -> anyhow::Result<()> {
        let attempts = Arc::new(AtomicUsize::new(0));
        let event_ids = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let server_attempts = attempts.clone();
        let server_event_ids = event_ids.clone();
        let app = Router::new().route(
            "/v1/events",
            post(move |Json(event): Json<ExternalEvent>| {
                let attempts = server_attempts.clone();
                let event_ids = server_event_ids.clone();
                async move {
                    event_ids.lock().await.push(event.event_id.clone());
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    } else {
                        Json(EventReceipt {
                            event_id: event.event_id,
                            turn_id: "turn".to_owned(),
                            state: TurnState::Queued,
                            duplicate: false,
                        })
                        .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });

        let core = core_client(format!("http://{address}"));
        let event = ExternalEvent {
            event_id: "event".to_owned(),
            agent_id: "default".to_owned(),
            user_id: "user".to_owned(),
            audience_id: "audience".to_owned(),
            delivery_target_id: "target".to_owned(),
            feedback_target_id: "feedback".to_owned(),
            content: "hello".to_owned(),
            native: Value::Null,
        };

        let receipt = tokio::time::timeout(
            Duration::from_secs(2),
            submit_event_with_retry(&core, &event),
        )
        .await?
        .map_err(|failure| anyhow!(failure.message))?;
        server.abort();

        assert_eq!(receipt.event_id, event.event_id);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(*event_ids.lock().await, vec!["event", "event"]);
        Ok(())
    }

    #[tokio::test]
    async fn interrupted_success_receipt_recovers_the_committed_turn() -> anyhow::Result<()> {
        use crate::{db::Database, model::ResourceSnapshot};
        use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

        let directory = tempfile::tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        let server_database = database.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let core = core_client(format!("http://{}", listener.local_addr()?));
        let event = ExternalEvent {
            event_id: "lost-receipt".to_owned(),
            agent_id: "agent".to_owned(),
            user_id: "user".to_owned(),
            audience_id: "conversation".to_owned(),
            delivery_target_id: "target".to_owned(),
            feedback_target_id: "feedback".to_owned(),
            content: "hello".to_owned(),
            native: Value::Null,
        };
        let server = tokio::spawn(async move {
            let resources = ResourceSnapshot {
                id: "snapshot".to_owned(),
                system_sha256: String::new(),
                context_sha256: String::new(),
                settings_sha256: String::new(),
                extensions_sha256: String::new(),
                skills_sha256: String::new(),
            };
            for attempt in 0..2 {
                let (socket, _) = listener.accept().await?;
                let mut stream = BufReader::new(socket);
                let mut length = None;
                loop {
                    let mut line = String::new();
                    anyhow::ensure!(
                        stream.read_line(&mut line).await? > 0,
                        "missing request headers"
                    );
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = Some(value.trim().parse::<usize>()?);
                    }
                }
                let mut body = vec![0; length.context("missing request length")?];
                stream.read_exact(&mut body).await?;
                let submitted: ExternalEvent = serde_json::from_slice(&body)?;
                assert_eq!(submitted.event_id, "lost-receipt");
                let receipt = server_database.accept_event(
                    "discord",
                    "runtime",
                    &submitted,
                    &Value::Null,
                    &resources,
                )?;
                assert_eq!(receipt.duplicate, attempt == 1);
                let body = serde_json::to_vec(&receipt)?;
                stream.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len(),
                ).as_bytes()).await?;
                // Core committed. Deliver headers and an incomplete body, then close TCP.
                stream
                    .write_all(if attempt == 0 { &body[..1] } else { &body })
                    .await?;
                stream.shutdown().await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let outcome = tokio::time::timeout(
            Duration::from_secs(3),
            submit_event_with_retry(&core, &event),
        )
        .await;
        if !matches!(&outcome, Ok(Ok(_))) {
            server.abort();
        }
        let receipt = outcome?.map_err(|failure| anyhow!(failure.message))?;
        tokio::time::timeout(Duration::from_secs(3), server).await???;
        assert!(receipt.duplicate);
        assert_eq!(receipt.state, TurnState::Queued);
        let turns = database.recent_turns(None, 10)?;
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].turn_id, receipt.turn_id);
        Ok(())
    }

    #[tokio::test]
    async fn configuration_rejection_does_not_block_the_next_discord_message() -> anyhow::Result<()>
    {
        let (trace, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let core_trace = trace.clone();
        let app = Router::new()
            .route(
                "/v1/events",
                post(move |Json(event): Json<ExternalEvent>| {
                    let trace = core_trace.clone();
                    async move {
                        let _ = trace.send(format!("submit {}", event.event_id));
                        if event.event_id == "400" {
                            (
                                StatusCode::CONFLICT,
                                "conversation runtime provider is disabled",
                            )
                                .into_response()
                        } else {
                            Json(EventReceipt {
                                event_id: event.event_id,
                                turn_id: "accepted".to_owned(),
                                duplicate: false,
                                state: TurnState::Queued,
                            })
                            .into_response()
                        }
                    }
                }),
            )
            .fallback(move |method: Method, uri: Uri| {
                let trace = trace.clone();
                async move {
                    let _ = trace.send(format!("{method} {uri}"));
                    StatusCode::NO_CONTENT
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let http = Arc::new(
            twilight_http::Client::builder()
                .token("test-only".to_owned())
                .proxy(address.to_string(), true)
                .ratelimiter(None)
                .build(),
        );
        let mut state = discord_state(core_client(format!("http://{address}")));
        state.discord.typing = false;
        let mut other = state.discord.routes[0].clone();
        other.channel_id = "301".to_owned();
        state.discord.routes.push(other);
        state.discord.failed_emoji = "❌".to_owned();
        let mut services = tokio::task::JoinSet::new();
        services.spawn(async move { axum::serve(listener, app).await });
        let (send, receive) = tokio::sync::mpsc::channel(2);
        for (event, channel) in [("400", "300"), ("401", "301")] {
            send.send(super::InboundMessage {
                request: SimulateRequest {
                    event_id: event.to_owned(),
                    guild_id: Some("100".to_owned()),
                    user_id: "200".to_owned(),
                    channel_id: channel.to_owned(),
                    parent_channel_id: None,
                    content: "hello".to_owned(),
                },
                native: Value::Null,
            })
            .await?;
        }
        drop(send);
        tokio::time::timeout(
            Duration::from_secs(3),
            super::message_worker(state, http, receive),
        )
        .await?;
        let mut requests = Vec::new();
        while let Ok(request) = observed.try_recv() {
            requests.push(request);
        }
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("submit "))
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["submit 400", "submit 401"]
        );
        // Observe actual failure feedback before the unrelated event's admission.
        let failure = requests
            .iter()
            .position(|request| {
                request.starts_with("PUT /api/v10/channels/300/messages/400/reactions/")
                    && request.contains("%E2%9D%8C")
            })
            .context("rejected message had no failure feedback")?;
        let next = requests
            .iter()
            .position(|request| request == "submit 401")
            .context("unrelated message not submitted")?;
        assert!(failure < next);
        Ok(())
    }

    #[tokio::test]
    async fn complete_invalid_receipt_is_not_retried_forever() -> anyhow::Result<()> {
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = attempts.clone();
        let app = Router::new().route(
            "/v1/events",
            post(move || {
                server_attempts.fetch_add(1, Ordering::SeqCst);
                async { (StatusCode::OK, "{invalid json}") }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let core = core_client(format!("http://{}", listener.local_addr()?));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let event = ExternalEvent {
            event_id: "invalid-receipt".to_owned(),
            agent_id: "agent".to_owned(),
            user_id: "user".to_owned(),
            audience_id: "conversation".to_owned(),
            delivery_target_id: "target".to_owned(),
            feedback_target_id: "feedback".to_owned(),
            content: "hello".to_owned(),
            native: Value::Null,
        };
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            submit_event_with_retry(&core, &event),
        )
        .await;
        server.abort();
        let failure = outcome?.err().context("invalid receipt was accepted")?;
        assert!(!failure.is_transient());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        Ok(())
    }

    fn completed_delivery(
        database: &crate::db::Database,
    ) -> anyhow::Result<crate::db::ClaimedTurn> {
        use crate::model::{ResourceSnapshot, RuntimeTurnResponse};
        use anyhow::Context as _;
        let event = ExternalEvent {
            event_id: "one".to_owned(),
            agent_id: "agent".to_owned(),
            user_id: "200".to_owned(),
            audience_id: "300".to_owned(),
            delivery_target_id: "300".to_owned(),
            feedback_target_id: "300:400".to_owned(),
            content: "question".to_owned(),
            native: Value::Null,
        };
        let resources = ResourceSnapshot {
            id: "snapshot".to_owned(),
            system_sha256: String::new(),
            context_sha256: String::new(),
            settings_sha256: String::new(),
            extensions_sha256: String::new(),
            skills_sha256: String::new(),
        };
        database.accept_event("discord", "runtime", &event, &Value::Null, &resources)?;
        let turn = database
            .claim_turn("runtime")?
            .context("turn was not queued")?;
        database.complete_turn(
            &turn,
            &RuntimeTurnResponse {
                correlation_id: turn.turn_id.clone(),
                output: "must not be sent twice".to_owned(),
                continuation_ref: "native".to_owned(),
            },
        )?;
        Ok(turn)
    }

    fn reconnect_router(
        database: crate::db::Database,
        trace: Arc<tokio::sync::Mutex<Vec<String>>>,
    ) -> Router {
        let reconcile_trace = trace.clone();
        let reconcile_database = database.clone();
        let poll_trace = trace.clone();
        let poll_database = database;
        let discord_trace = trace.clone();
        let unexpected_trace = trace;
        Router::new()
            .route(
                "/v1/deliveries/reconcile",
                post(move || {
                    let database = reconcile_database.clone();
                    let trace = reconcile_trace.clone();
                    async move {
                        trace.lock().await.push("reconcile".to_owned());
                        Ok::<_, StatusCode>(Json(DeliveryReconciliation {
                            feedback_target_ids: database
                                .reconcile_delivery_claims("discord", "instance")
                                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                        }))
                    }
                }),
            )
            .route(
                "/v1/deliveries",
                get(move || {
                    let database = poll_database.clone();
                    let trace = poll_trace.clone();
                    async move {
                        trace.lock().await.push("poll".to_owned());
                        if database
                            .claim_delivery("discord", "instance")
                            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                            .is_some()
                        {
                            trace
                                .lock()
                                .await
                                .push("unexpected duplicate delivery".to_owned());
                        }
                        // Terminate the infinite production loop after observing resumed polling.
                        Ok::<_, StatusCode>(StatusCode::FORBIDDEN)
                    }
                }),
            )
            .route(
                "/api/v10/channels/{channel}/messages/{message}/reactions/{emoji}/@me",
                axum::routing::any(
                    move |method: Method,
                          axum::extract::Path((channel, message, emoji)): axum::extract::Path<
                        (String, String, String),
                    >| {
                        let trace = discord_trace.clone();
                        async move {
                            trace
                                .lock()
                                .await
                                .push(format!("{method} {channel}:{message} {emoji}"));
                            StatusCode::NO_CONTENT
                        }
                    },
                ),
            )
            .fallback(move |method: Method, uri: Uri| {
                let trace = unexpected_trace.clone();
                async move {
                    trace
                        .lock()
                        .await
                        .push(format!("unexpected {method} {uri}"));
                    StatusCode::NOT_FOUND
                }
            })
    }

    #[tokio::test]
    async fn replacement_interface_reconciles_an_abandoned_delivery_claim() -> anyhow::Result<()> {
        use crate::{db::Database, model::DeliveryState};
        use anyhow::Context as _;
        use tokio::io::AsyncReadExt as _;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state.sqlite3");
        let database = Database::create(&path)?;
        let turn = completed_delivery(&database)?;

        let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let app = reconnect_router(database.clone(), trace.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server_trace = trace.clone();
        let server = tokio::spawn(async move {
            // Commit a real claim, but close the TCP connection without its response.
            let (mut connection, _) = listener.accept().await?;
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let length = connection.read(&mut buffer).await?;
                anyhow::ensure!(length > 0, "claim request disconnected early");
                request.extend_from_slice(&buffer[..length]);
            }
            assert!(request.starts_with(b"GET /v1/deliveries HTTP/1.1\r\n"));
            assert!(
                database
                    .claim_delivery("discord", "previous-instance")?
                    .is_some()
            );
            server_trace.lock().await.push("claim committed".to_owned());
            let restarted = Database::open(&path)?;
            restarted.recover_running_turns()?;
            server_trace.lock().await.push("recovered".to_owned());
            drop(connection);
            axum::serve(listener, app).await?;
            Ok::<_, anyhow::Error>(())
        });
        let mut state = discord_state(core_client(format!("http://{address}")));
        state.discord.reactions = true;
        let expected_uncertain = state.discord.uncertain_emoji.clone();
        let http = Arc::new(
            twilight_http::Client::builder()
                .proxy(address.to_string(), true)
                .ratelimiter(None)
                .build(),
        );
        // A deadline bounds test failure; progression is driven by actual requests, not sleeps.
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            let mut previous = core_client(format!("http://{address}"));
            previous.instance_id = "previous-instance".to_owned();
            assert!(previous.claim_delivery().await.is_err());
            // A replacement interface must reconcile its predecessor before polling.
            super::reconcile_after_claim_failure(&state, &http).await?;
            delivery_loop(state, http).await
        })
        .await;
        server.abort();
        let _ = server.await;
        assert!(
            outcome?
                .err()
                .context("delivery loop unexpectedly succeeded")?
                .to_string()
                .contains("delivery claim rejected")
        );
        let requests = trace.lock().await;
        // Recovery must precede uncertainty feedback and resumed polling.
        // Processing-reaction cleanup is not part of this recovery contract.
        let required = [
            "claim committed".to_owned(),
            "recovered".to_owned(),
            "reconcile".to_owned(),
            format!("PUT 300:400 {expected_uncertain}"),
            "poll".to_owned(),
        ];
        let positions = required
            .iter()
            .map(|event| {
                requests
                    .iter()
                    .position(|request| request == event)
                    .with_context(|| format!("missing {event}: {requests:?}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            requests
                .iter()
                .all(|request| !request.starts_with("unexpected"))
        );
        drop(requests);
        let inspected = Database::open(&directory.path().join("state.sqlite3"))?
            .inspect_turn(&turn.turn_id)?
            .context("turn disappeared")?;
        assert_eq!(inspected.delivery_state, Some(DeliveryState::Uncertain));
        Ok(())
    }

    #[test]
    fn discord_route_errors_distinguish_terminal_responses() {
        assert!(discord_status_is_terminal(DiscordStatusCode::FORBIDDEN));
        assert!(discord_status_is_terminal(DiscordStatusCode::NOT_FOUND));
        assert!(!discord_status_is_terminal(
            DiscordStatusCode::TOO_MANY_REQUESTS
        ));
        assert!(!discord_status_is_terminal(
            DiscordStatusCode::GATEWAY_UNAVAILABLE
        ));
    }

    #[tokio::test]
    async fn empty_discord_delivery_is_not_successful() -> anyhow::Result<()> {
        let failure = send_discord_message(
            &twilight_http::Client::new("unused".to_owned()),
            twilight_model::id::Id::new(1),
            " \n ",
        )
        .await
        .err()
        .context("empty content was accepted for delivery")?;
        assert_eq!(failure.to_string(), "Discord delivery content is empty");
        Ok(())
    }

    #[test]
    fn discord_messages_split_cleanly_at_the_platform_limit() {
        let content = format!("{}\n\n{}", "a".repeat(1_990), "b".repeat(30));
        let chunks = discord_chunks(&content);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.join("\n\n"), content);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 2_000));

        let content = "🦀".repeat(2_001);
        let unbroken = discord_chunks(&content);
        assert!(unbroken.len() > 1);
        assert!(
            unbroken
                .iter()
                .all(|chunk| !chunk.is_empty() && chunk.chars().count() <= 2_000)
        );
        assert_eq!(unbroken.concat(), content);
    }

    #[test]
    fn thread_titles_preserve_short_text_and_fit_discord() {
        assert_eq!(thread_title("  Plan   the weekend  "), "Plan the weekend");
        for content in ["a long discussion ".repeat(20), "🦀".repeat(150)] {
            let title = thread_title(&content);
            assert!(!title.trim().is_empty());
            assert!(title.chars().count() <= 100);
            assert_eq!(title, thread_title(&content));
        }
    }

    #[test]
    fn mention_activation_strips_only_the_configured_bot() -> anyhow::Result<()> {
        let config = discord_config(DiscordActivation::Mention, DiscordConversation::Thread);
        let ignored = resolve_message(
            &config,
            request("300", None, "hello"),
            Value::Null,
            &ParticipationState::default(),
        )
        .map_err(|failure| anyhow!(failure.message))?;
        assert!(ignored.is_none());

        let plan = accepted(resolve_message(
            &config,
            request("300", None, "<@201> hello <@999>"),
            Value::Null,
            &ParticipationState::default(),
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
            &ParticipationState::default(),
        ))?;
        assert!(adoption.adopt_thread);

        let participation = ParticipationState {
            threads: BTreeMap::from([("400".to_owned(), "300".to_owned())]),
            ..ParticipationState::default()
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
            &ParticipationState::default(),
        ))?;
        assert!(!thread_message.create_thread);

        let channel = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
        assert!(
            resolve_message(
                &channel,
                request("400", Some("300"), "must reject"),
                Value::Null,
                &ParticipationState::default(),
            )
            .is_err()
        );
        let first = accepted(resolve_message(
            &channel,
            request("300", None, "first"),
            Value::Null,
            &ParticipationState::default(),
        ))?;
        let second = accepted(resolve_message(
            &channel,
            request("300", None, "second"),
            Value::Null,
            &ParticipationState::default(),
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
