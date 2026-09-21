//! The Discord interface provider: platform authentication, guild/user/route
//! admission, thread routing, feedback, message delivery, and slash-command
//! interactions. Core knows none of this; the provider talks to core only
//! through [`CoreClient`].

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
use carillon_discord_commands::contract::{Interaction, InteractionKind, Reply};
use carillon_protocol::{
    config::{
        LoadedConfig, ProviderRole, RELEASE_VERSION, hash_token, read_token_file, token_matches,
    },
    filesystem::sync_ancestors,
    model::{
        Delivery, DeliveryAck, DeliveryOutcome, DeliveryReconciliation, EventReceipt,
        ExternalEvent, Health, INSTANCE_HEADER, ModelQuery, ModelUpdate, PROVIDER_HEADER,
    },
};
use reqwest::{Client, IntoUrl, Method, RequestBuilder, Url};
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
    application::interaction::{
        Interaction as DiscordInteraction, InteractionData, InteractionType,
    },
    channel::{
        ChannelType,
        message::{Component, MessageFlags, MessageType, component::SelectMenuType},
    },
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{
        Id,
        marker::{ChannelMarker, EmojiMarker, MessageMarker},
    },
};
use twilight_validate::component::{COMPONENT_COUNT, component_v1};
use uuid::Uuid;

use crate::{
    commands::{Commands, DiscordApp, ModelCalls, ReloadError, ReloadReport},
    config::{DiscordActivation, DiscordConfig, DiscordConversation, DiscordEmoji},
};

/// A message as the deterministic `/v1/simulate` endpoint accepts it and as
/// the gateway presents real messages internally.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SimulateRequest {
    pub event_id: String,
    pub guild_id: Option<String>,
    pub user_id: String,
    pub channel_id: String,
    #[serde(default)]
    pub parent_channel_id: Option<String>,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SimulateResponse {
    pub receipt: EventReceipt,
    pub delivery: Option<Delivery>,
    pub conversation_id: String,
    pub feedback: Vec<String>,
}

/// An interaction as the deterministic `/v1/interactions` endpoint accepts
/// it and as the gateway presents real interactions internally: only what
/// admission and the command module need.
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
pub struct CoreClient {
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

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn request(&self, method: Method, url: impl IntoUrl) -> RequestBuilder {
        self.http
            .request(method, url)
            // These are local admission/delivery endpoints, not model calls.
            // Bound headers and body reads so ambiguous claims can reconcile.
            .timeout(Duration::from_secs(10))
            .bearer_auth(&self.token)
            .header(PROVIDER_HEADER, &self.provider_id)
            .header(INSTANCE_HEADER, &self.instance_id)
    }

    async fn submit_event(&self, event: &ExternalEvent) -> Result<EventReceipt, ServiceError> {
        let request = self
            .request(Method::POST, self.url("/v1/events"))
            .json(event);
        core_json(request, "submit event to core").await
    }

    async fn claim_delivery(&self) -> Result<Option<Delivery>, ServiceError> {
        let request = self.request(Method::GET, self.url("/v1/deliveries"));
        core_json(request, "claim core delivery").await
    }

    async fn acknowledge_delivery(
        &self,
        id: i64,
        outcome: DeliveryOutcome,
        native_ref: Option<String>,
    ) -> Result<(), ServiceError> {
        let request = self
            .request(Method::POST, self.url(&format!("/v1/deliveries/{id}/ack")))
            .json(&DeliveryAck {
                native_ref,
                outcome,
            });
        core_send(request, "acknowledge core delivery").await?;
        Ok(())
    }

    async fn reconcile(&self) -> Result<DeliveryReconciliation, ServiceError> {
        let request = self.request(Method::POST, self.url("/v1/deliveries/reconcile"));
        core_json(request, "reconcile abandoned delivery claims").await
    }

    /// The runtime's model state for an agent and, when given, a
    /// conversation, relayed by core as opaque JSON.
    pub async fn model_state(&self, query: &ModelQuery) -> Result<Value, String> {
        let mut url = Url::parse(&self.url("/v1/model")).map_err(|error| error.to_string())?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("agent_id", &query.agent_id);
            if let Some(audience_id) = &query.audience_id {
                pairs.append_pair("audience_id", audience_id);
            }
        }
        let request = self.request(Method::GET, url).timeout(MODEL_CALL_TIMEOUT);
        model_json(request).await
    }

    /// Changes a model selection and returns the resulting state.
    pub async fn set_model(&self, update: &ModelUpdate) -> Result<Value, String> {
        let request = self
            .request(Method::PUT, self.url("/v1/model"))
            .timeout(MODEL_CALL_TIMEOUT)
            .json(update);
        model_json(request).await
    }
}

/// Model calls answer a waiting user and run inside the command module's
/// deadline, so they give up well before it.
const MODEL_CALL_TIMEOUT: Duration = Duration::from_secs(6);

/// Performs a model call and turns every failure into words for the user.
/// Core and the runtime phrase their rejections for people; transport
/// failures are described here.
async fn model_json(request: RequestBuilder) -> Result<Value, String> {
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return Err("The runtime did not answer in time.".to_owned());
        }
        Err(error) => {
            warn!(%error, "model call failed to reach core");
            return Err("The runtime is not reachable right now.".to_owned());
        }
    };
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    if status.is_success() {
        return serde_json::from_slice(&body)
            .map_err(|_| "The runtime reported an unusable model state.".to_owned());
    }
    let detail = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| value.get("error")?.as_str().map(str::to_owned))
        .filter(|detail| !detail.is_empty());
    warn!(%status, detail, "model call was rejected");
    Err(detail.unwrap_or_else(|| "The runtime refused the request.".to_owned()))
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
        sync_ancestors(
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
    commands: Arc<Commands>,
    /// Absent in deterministic mode, where no Discord application exists.
    app: Option<Arc<DiscordApp>>,
    operator_token_hash: String,
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

/// Everything `serve` needs besides the registry.
pub struct ServeOptions<'a> {
    pub provider_id: &'a str,
    pub token_file: &'a Path,
    pub discord_token_file: Option<&'a Path>,
    pub commands: &'a Path,
    pub deterministic: bool,
}

pub async fn serve(config: LoadedConfig, options: ServeOptions<'_>) -> Result<()> {
    let provider = config
        .provider(options.provider_id, ProviderRole::Interface)?
        .clone();
    if !provider.enabled {
        bail!("Discord interface provider is disabled");
    }
    let discord = DiscordConfig::load(&config, &provider.id)?;
    let token = read_token_file(options.token_file)?;
    if !token_matches(&provider.token_hash, &token) {
        bail!("Discord interface provider token does not match configuration");
    }
    // Establish instance exclusivity before touching provider state or claims.
    let address = provider.listen_address()?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("bind Discord interface provider to {address}"))?;
    let component_dir = config.value.data_dir.join("components").join(&provider.id);
    let participation = Arc::new(Participation::load(
        component_dir.join("participation.json"),
    )?);
    let commands = Arc::new(
        Commands::load(
            options.commands.to_path_buf(),
            component_dir.join("commands.json"),
        )
        .await?,
    );
    let core = CoreClient::new(&config, provider.id.clone(), token);
    let reconciled = loop {
        match core.reconcile().await {
            Ok(reconciled) => break reconciled,
            Err(failure) if failure.is_transient() => {
                warn!(error = %failure.message, "waiting to reconcile Discord delivery claims");
                sleep(Duration::from_millis(250)).await;
            }
            Err(failure) => bail!("delivery reconciliation rejected: {}", failure.message),
        }
    };
    let health = Health {
        id: provider.id.clone(),
        role: provider.role.as_str().to_owned(),
        version: RELEASE_VERSION.to_owned(),
        status: "ready".to_owned(),
    };
    let state = DiscordState {
        discord,
        health,
        core,
        participation,
        commands,
        app: None,
        operator_token_hash: config.value.core.operator_token_hash.clone(),
    };
    info!(url = %provider.url, deterministic = options.deterministic, "Discord interface provider ready");
    if options.deterministic {
        axum::serve(listener, discord_router(state, true)).await?;
        return Ok(());
    }
    let discord_token_path = options
        .discord_token_file
        .context("--discord-token-file is required outside deterministic mode")?;
    connect(
        state,
        listener,
        read_token_file(discord_token_path)?,
        reconciled,
    )
    .await
}

/// Runs the provider against Discord: authenticates the bot, registers the
/// command module's commands when they changed, then serves the gateway,
/// deliveries, and the operator endpoints together.
async fn connect(
    mut state: DiscordState,
    listener: TcpListener,
    discord_token: String,
    reconciled: DeliveryReconciliation,
) -> Result<()> {
    let http = Arc::new(DiscordHttp::new(discord_token.clone()));
    let current_user = http.current_user().await?.model().await?;
    if current_user.id.to_string() != state.discord.bot_user_id {
        bail!("configured Discord bot user ID does not match the token");
    }
    let application_id = http
        .current_user_application()
        .await
        .context("resolve Discord application")?
        .model()
        .await
        .context("decode Discord application")?
        .id;
    let app = Arc::new(DiscordApp {
        http: http.clone(),
        application_id,
        guild_id: guild(&state.discord)?,
    });
    let definitions = state.commands.definitions();
    if !state.commands.registered(&app, &definitions) {
        state.commands.register(&app, &definitions).await?;
    }
    state.app = Some(app);
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
    let router = discord_router(state.clone(), false);
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

fn guild(discord: &DiscordConfig) -> Result<Id<twilight_model::id::marker::GuildMarker>> {
    discord
        .guild_id
        .parse()
        .context("configured Discord guild ID is invalid")
}

fn discord_router(state: DiscordState, deterministic: bool) -> Router {
    let router = Router::new()
        .route("/health", get(discord_health))
        .route("/v1/commands/reload", post(reload_commands));
    if deterministic {
        router
            .route("/v1/simulate", post(simulate))
            .route("/v1/interactions", post(simulate_interaction))
    } else {
        router
    }
    .with_state(state)
}

async fn discord_health(State(state): State<DiscordState>) -> Json<Health> {
    Json(state.health)
}

fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default()
}

/// Deterministic endpoints act for the interface itself and require its
/// credential.
fn authorize_interface(state: &DiscordState, headers: &HeaderMap) -> Result<(), ServiceError> {
    if !token_matches(&hash_token(&state.core.token), bearer(headers)) {
        return Err(ServiceError {
            status: StatusCode::FORBIDDEN,
            message: "this endpoint requires the interface provider credential".to_owned(),
        });
    }
    Ok(())
}

/// Operator-requested replacement of the command module from its configured
/// path. When the provider has a Discord application, definitions Discord
/// does not hold yet are registered first; the replacement serves only once
/// Discord confirmed them, so a refused or unanswered registration leaves the
/// previous module in place.
async fn reload_commands(
    State(state): State<DiscordState>,
    headers: HeaderMap,
) -> Result<Json<ReloadReport>, ServiceError> {
    if !token_matches(&state.operator_token_hash, bearer(&headers)) {
        return Err(ServiceError {
            status: StatusCode::FORBIDDEN,
            message: "command reload requires the operator credential".to_owned(),
        });
    }
    state
        .commands
        .reload(state.app.as_deref())
        .await
        .map(Json)
        .map_err(|failure| match failure {
            ReloadError::Prepare(failure) => failure.into(),
            ReloadError::Register(failure) => {
                error!(error = %failure, "command registration failed");
                ServiceError {
                    status: StatusCode::BAD_GATEWAY,
                    message: registration_failure(&failure),
                }
            }
        })
}

/// Why a reload stopped before activating the candidate, for the operator.
fn registration_failure(failure: &anyhow::Error) -> String {
    let outcome = if discord_failure_is_terminal(failure) {
        "Discord rejected the commands"
    } else {
        "Discord did not confirm the commands; whether it applied them is unknown, so reload again once it answers"
    };
    format!("{outcome}: {failure:#}; the previous command module keeps serving")
}

async fn simulate(
    State(state): State<DiscordState>,
    headers: HeaderMap,
    Json(request): Json<SimulateRequest>,
) -> Result<Response, ServiceError> {
    authorize_interface(&state, &headers)?;
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

/// Runs an interaction exactly as the gateway would, returning the reply the
/// user would see. Replies are checked against Discord's component model so
/// a module producing unusable components fails here, not in production.
async fn simulate_interaction(
    State(state): State<DiscordState>,
    headers: HeaderMap,
    Json(source): Json<InteractionSource>,
) -> Result<Json<Reply>, ServiceError> {
    authorize_interface(&state, &headers)?;
    let admitted = admit_interaction(&state.discord, source)?;
    let reply = run_interaction(&state, admitted)
        .await
        .map_err(|error| ServiceError {
            status: StatusCode::BAD_GATEWAY,
            message: error.to_string(),
        })?;
    validate_reply(&reply).map_err(|error| ServiceError {
        status: StatusCode::BAD_GATEWAY,
        message: error.to_string(),
    })?;
    Ok(Json(reply))
}

async fn gateway_loop(
    state: DiscordState,
    token: String,
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
                if let Err(refused) = queue_message(&messages, InboundMessage { request, native })?
                {
                    warn!(
                        channel = %refused.request.channel_id,
                        message = %refused.request.event_id,
                        "Discord message refused: the message queue is full"
                    );
                    if let Some(app) = state.app.clone() {
                        let discord = state.discord.clone();
                        tokio::spawn(async move {
                            let source = format!(
                                "{}:{}",
                                refused.request.channel_id, refused.request.event_id
                            );
                            terminal_feedback(&app.http, &discord, &source, Feedback::Failed).await;
                        });
                    }
                }
            }
            Event::InteractionCreate(interaction) => {
                // Interactions are independent of message order and of each
                // other; each answers within Discord's own deadline.
                tokio::spawn(handle_interaction(state.clone(), interaction.0));
            }
            _ => {}
        }
    }
    bail!("Discord gateway event stream ended")
}

/// Hands a message to the worker without waiting for room. The gateway must
/// keep reading so heartbeats and interactions go on while the worker is
/// retrying; a message that finds the queue full is returned refused, and
/// the person who sent it is told it failed rather than left waiting.
fn queue_message(
    messages: &mpsc::Sender<InboundMessage>,
    inbound: InboundMessage,
) -> Result<Result<(), InboundMessage>> {
    match messages.try_send(inbound) {
        Ok(()) => Ok(Ok(())),
        Err(mpsc::error::TrySendError::Full(inbound)) => Ok(Err(inbound)),
        Err(mpsc::error::TrySendError::Closed(_)) => bail!("Discord message worker stopped"),
    }
}

/// The provider's view of a gateway interaction, or `None` for kinds the
/// provider does not serve (pings, autocomplete, modals).
fn interaction_source(interaction: &DiscordInteraction) -> Option<InteractionSource> {
    let (kind, name, values, options) = match (&interaction.kind, interaction.data.as_ref()?) {
        (InteractionType::ApplicationCommand, InteractionData::ApplicationCommand(data)) => (
            InteractionKind::Command,
            data.name.clone(),
            Vec::new(),
            serde_json::to_value(&data.options).unwrap_or(Value::Null),
        ),
        (InteractionType::MessageComponent, InteractionData::MessageComponent(data)) => (
            InteractionKind::Component,
            data.custom_id.clone(),
            data.values.clone(),
            Value::Null,
        ),
        _ => return None,
    };
    let channel = interaction.channel.as_ref()?;
    let parent_channel_id = matches!(
        channel.kind,
        ChannelType::AnnouncementThread | ChannelType::PrivateThread | ChannelType::PublicThread
    )
    .then(|| channel.parent_id.map(|id| id.to_string()))
    .flatten();
    Some(InteractionSource {
        guild_id: interaction.guild_id.map(|id| id.to_string()),
        user_id: interaction.author_id()?.to_string(),
        channel_id: channel.id.to_string(),
        parent_channel_id,
        kind,
        name,
        values,
        options,
    })
}

/// An admitted interaction: what the module sees, and the conversation it
/// happened in when the agent participates there.
struct AdmittedInteraction {
    interaction: Interaction,
    audience_id: Option<String>,
}

/// Fail-closed admission for interactions: the configured guild, an
/// allowlisted user, and a routed channel or a thread under a thread route.
/// The conversation is the one messages there would belong to.
fn admit_interaction(
    config: &DiscordConfig,
    source: InteractionSource,
) -> Result<AdmittedInteraction, ServiceError> {
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
        return Err(ServiceError {
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

async fn run_interaction(state: &DiscordState, admitted: AdmittedInteraction) -> Result<Reply> {
    let calls = ModelCalls::new(
        state.core.clone(),
        ModelQuery {
            agent_id: state.discord.agent.clone(),
            audience_id: admitted.audience_id,
        },
    );
    state.commands.invoke(calls, admitted.interaction).await
}

/// Validate content and components before editing an acknowledgement.
/// The reply's components as Discord accepts them: at most five action rows
/// of buttons and select menus within Discord's limits, checked here so a
/// module that exceeds them is reported instead of discovered as a rejected
/// edit that leaves the acknowledgement hanging.
fn validate_reply(reply: &Reply) -> Result<Vec<Component>> {
    twilight_validate::message::content(&reply.content)
        .context("command module reply has unusable content")?;
    let components: Vec<Component> = serde_json::from_value(Value::Array(reply.components.clone()))
        .context("command module reply has invalid components")?;
    if components.len() > COMPONENT_COUNT {
        bail!(
            "command module reply has {} action rows; Discord allows {COMPONENT_COUNT}",
            components.len()
        );
    }
    for component in &components {
        component_v1(component).context("command module reply has an unusable component")?;
        // twilight checks the maxima; Discord also refuses an empty row or an
        // empty select menu.
        let Component::ActionRow(row) = component else {
            bail!("command module reply has a component outside an action row");
        };
        if row.components.is_empty() {
            bail!("command module reply has an empty action row");
        }
        for inner in &row.components {
            if let Component::SelectMenu(menu) = inner
                && menu.kind == SelectMenuType::Text
                && menu.options.as_ref().is_none_or(Vec::is_empty)
            {
                bail!("command module reply has a select menu without options");
            }
        }
    }
    Ok(components)
}

/// Answers one gateway interaction: acknowledge within Discord's window,
/// run the module, then edit the acknowledgement into the reply. Refusals
/// and failures are told to the user in general terms and logged in full.
async fn handle_interaction(state: DiscordState, interaction: DiscordInteraction) {
    let Some(app) = state.app.clone() else {
        return;
    };
    let Some(source) = interaction_source(&interaction) else {
        return;
    };
    let client = app.http.interaction(app.application_id);
    let deferred = InteractionResponse {
        kind: if source.kind == InteractionKind::Component {
            InteractionResponseType::DeferredUpdateMessage
        } else {
            InteractionResponseType::DeferredChannelMessageWithSource
        },
        data: Some(InteractionResponseData {
            flags: Some(MessageFlags::EPHEMERAL),
            ..InteractionResponseData::default()
        }),
    };
    if let Err(error) = client
        .create_response(interaction.id, &interaction.token, &deferred)
        .await
    {
        warn!(%error, "Discord interaction acknowledgement failed");
        return;
    }
    let (content, components) = match admit_interaction(&state.discord, source) {
        Ok(admitted) => match run_interaction(&state, admitted).await {
            Ok(reply) => match validate_reply(&reply) {
                Ok(components) => (reply.content, components),
                Err(error) => {
                    error!(%error, "command module reply was unusable");
                    (
                        "The command produced an unusable reply.".to_owned(),
                        Vec::new(),
                    )
                }
            },
            Err(error) => {
                error!(%error, "command module invocation failed");
                ("The command could not be completed.".to_owned(), Vec::new())
            }
        },
        Err(refusal) => {
            info!(reason = %refusal.message, "Discord interaction refused");
            ("This command is not available here.".to_owned(), Vec::new())
        }
    };
    if let Err(error) = client
        .update_response(&interaction.token)
        .content(Some(&content))
        .components(Some(&components))
        .await
    {
        warn!(%error, "Discord interaction reply failed");
    }
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
pub(crate) mod tests {
    use std::{
        collections::BTreeMap,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU16, AtomicUsize, Ordering},
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
    use carillon_discord_commands::contract::{InteractionKind, Reply};
    use carillon_protocol::{
        config::{LoadedConfig, hash_token},
        model::{
            DeliveryOutcome, DeliveryReconciliation, EventReceipt, ExternalEvent, Health, TurnState,
        },
    };
    use serde_json::{Value, json};
    use tower::ServiceExt as _;
    use twilight_http::response::StatusCode as DiscordStatusCode;

    use super::{
        CoreClient, DiscordState, InboundMessage, InteractionSource, MessagePlan, Participation,
        ParticipationState, ServeOptions, ServiceError, SimulateRequest, admit_interaction,
        delivery_loop, discord_chunks, discord_router, discord_status_is_terminal,
        interaction_source, queue_message, resolve_message, resolved_message, send_discord_message,
        submit_event_with_retry, thread_title,
    };
    use crate::{
        commands::{
            Commands,
            tests::{app, definition, describing, registration_server, replying, write_module},
        },
        config::{DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute},
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

    pub(crate) fn core_client(base_url: String) -> CoreClient {
        CoreClient {
            http: reqwest::Client::new(),
            base_url,
            token: "token".to_owned(),
            provider_id: "discord".to_owned(),
            instance_id: "instance".to_owned(),
        }
    }

    /// A command module in `directory` that defines `/model` and answers
    /// every interaction with `reply`.
    async fn stub_commands(directory: &Path, reply: &str) -> anyhow::Result<Arc<Commands>> {
        let module = directory.join("commands.wasm");
        write_module(&module, &describing(&[definition("model")]), reply)?;
        Ok(Arc::new(
            Commands::load(module, directory.join("commands.json")).await?,
        ))
    }

    async fn discord_state(core: CoreClient, directory: &Path) -> anyhow::Result<DiscordState> {
        Ok(DiscordState {
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
            commands: stub_commands(directory, &replying("stub reply", &[])).await?,
            app: None,
            operator_token_hash: hash_token("operator"),
        })
    }

    /// An installation whose interface points at `provider_address` and whose
    /// core is the fake at `core_address`, plus the token file and command
    /// module `serve` needs.
    fn served_installation(
        root: &Path,
        core_address: std::net::SocketAddr,
        provider_address: std::net::SocketAddr,
    ) -> anyhow::Result<(LoadedConfig, PathBuf, PathBuf)> {
        let mut config = crate::config::tests::installation(root, "discord-private", 45_200)?;
        config.value.core.listen = core_address;
        config
            .value
            .providers
            .iter_mut()
            .find(|provider| provider.id == "discord-private")
            .context("missing interface")?
            .url = format!("http://{provider_address}");
        let module = root.join("commands.wasm");
        write_module(
            &module,
            &describing(&[definition("model")]),
            &replying("unused", &[]),
        )?;
        let token_file = root.join("credentials").join("discord-private.token");
        Ok((config, token_file, module))
    }

    fn deterministic_options<'a>(token_file: &'a Path, module: &'a Path) -> ServeOptions<'a> {
        ServeOptions {
            provider_id: "discord-private",
            token_file,
            discord_token_file: None,
            commands: module,
            deterministic: true,
        }
    }

    #[tokio::test]
    async fn startup_reconciliation_refuses_permanent_errors_and_recovers_transient_errors()
    -> anyhow::Result<()> {
        for status in [
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let directory = tempfile::tempdir()?;
            let core_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let provider_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let provider_address = provider_listener.local_addr()?;
            drop(provider_listener);
            let (config, token_file, module) = served_installation(
                &directory.path().join("installation"),
                core_listener.local_addr()?,
                provider_address,
            )?;
            let attempts = Arc::new(AtomicUsize::new(0));
            let request_attempts = attempts.clone();
            let app = Router::new().route(
                "/v1/deliveries/reconcile",
                post(move || {
                    let attempt = request_attempts.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if attempt == 0 {
                            status.into_response()
                        } else {
                            Json(DeliveryReconciliation {
                                feedback_target_ids: Vec::new(),
                            })
                            .into_response()
                        }
                    }
                }),
            );
            let mut services = tokio::task::JoinSet::new();
            services
                .spawn(async move { axum::serve(core_listener, app).await.map_err(Into::into) });
            if status != StatusCode::SERVICE_UNAVAILABLE {
                let result = tokio::time::timeout(
                    Duration::from_secs(2),
                    super::serve(config.clone(), deterministic_options(&token_file, &module)),
                )
                .await?;
                assert!(
                    result
                        .err()
                        .context("permanent reconciliation failure accepted")?
                        .to_string()
                        .contains("delivery reconciliation rejected")
                );
                assert_eq!(attempts.load(Ordering::SeqCst), 1);
                // Failed startup releases the socket; repairing core permits the same config.
                let probe = tokio::net::TcpListener::bind(provider_address).await?;
                drop(probe);
            }
            services.spawn(async move {
                super::serve(config, deterministic_options(&token_file, &module)).await
            });
            // A connection can be refused before serve binds. Yield only on that
            // boundary; a connected request waits for reconciliation and health.
            let health = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match reqwest::get(format!("http://{provider_address}/health")).await {
                        Ok(response) => break response.json::<Health>().await,
                        Err(error) if error.is_connect() => tokio::task::yield_now().await,
                        Err(error) => break Err(error),
                    }
                }
            })
            .await??;
            assert_eq!(health.status, "ready");
            assert_eq!(attempts.load(Ordering::SeqCst), 2);
        }
        Ok(())
    }

    #[tokio::test]
    async fn occupied_listener_fails_before_touching_delivery_claims() -> anyhow::Result<()> {
        for deterministic in [true, false] {
            let directory = tempfile::tempdir()?;
            let core_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let occupant = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let (config, token_file, module) = served_installation(
                &directory.path().join("installation"),
                core_listener.local_addr()?,
                occupant.local_addr()?,
            )?;
            let reconciles = Arc::new(AtomicUsize::new(0));
            let counted = reconciles.clone();
            let app = Router::new().route(
                "/v1/deliveries/reconcile",
                post(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                    async {
                        Json(DeliveryReconciliation {
                            feedback_target_ids: Vec::new(),
                        })
                    }
                }),
            );
            let core = tokio::spawn(async move { axum::serve(core_listener, app).await });
            let discord_token_file = directory.path().join("missing-discord.token");
            let failure = tokio::time::timeout(
                Duration::from_secs(2),
                super::serve(
                    config,
                    ServeOptions {
                        provider_id: "discord-private",
                        token_file: &token_file,
                        discord_token_file: Some(&discord_token_file),
                        commands: &module,
                        deterministic,
                    },
                ),
            )
            .await?
            .err()
            .context("occupied listener was accepted")?;
            core.abort();
            assert!(
                failure.chain().any(|cause| cause
                    .to_string()
                    .contains("bind Discord interface provider")),
                "{failure:#}"
            );
            assert_eq!(reconciles.load(Ordering::SeqCst), 0);
        }
        Ok(())
    }

    #[tokio::test]
    async fn core_requests_timeout_on_stalled_headers_and_bodies() -> anyhow::Result<()> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut cases = tokio::task::JoinSet::new();
        for operation in 0..4 {
            for partial_body in [false, true] {
                // Acknowledgements need only a success status, not a response body.
                if operation == 3 && partial_body {
                    continue;
                }
                cases.spawn(async move {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                    let client = core_client(format!("http://{}", listener.local_addr()?));
                    let mut services = tokio::task::JoinSet::new();
                    services.spawn(async move {
                        let (mut connection, _) = listener.accept().await?;
                        let mut request = Vec::new();
                        let mut buffer = [0; 1024];
                        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                            let length = connection.read(&mut buffer).await?;
                            anyhow::ensure!(length > 0, "request disconnected early");
                            request.extend_from_slice(&buffer[..length]);
                        }
                        if partial_body {
                            connection
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                                .await?;
                        }
                        std::future::pending::<()>().await;
                        Ok::<_, anyhow::Error>(())
                    });
                    let started = tokio::time::Instant::now();
                    let result = tokio::time::timeout(Duration::from_secs(15), async {
                        match operation {
                            0 => client
                                .submit_event(&ExternalEvent {
                                    event_id: "timeout".to_owned(),
                                    agent_id: "agent".to_owned(),
                                    user_id: "user".to_owned(),
                                    audience_id: "conversation".to_owned(),
                                    delivery_target_id: "target".to_owned(),
                                    feedback_target_id: "feedback".to_owned(),
                                    content: "hello".to_owned(),
                                    native: Value::Null,
                                })
                                .await
                                .map(|_| ()),
                            1 => client.claim_delivery().await.map(|_| ()),
                            2 => client.reconcile().await.map(|_| ()),
                            _ => {
                                client
                                    .acknowledge_delivery(1, DeliveryOutcome::Delivered, None)
                                    .await
                            }
                        }
                    })
                    .await?;
                    let failure = result.err().context("stalled response was accepted")?;
                    assert!(failure.is_transient());
                    assert!(started.elapsed() >= Duration::from_secs(10));
                    assert!(
                        services.try_join_next().is_none(),
                        "server failed instead of stalling"
                    );
                    Ok::<_, anyhow::Error>(())
                });
            }
        }
        let mut count = 0;
        while let Some(result) = cases.join_next().await {
            result??;
            count += 1;
        }
        assert_eq!(count, 7);
        Ok(())
    }

    #[tokio::test]
    async fn model_calls_relay_rejections_in_the_runtime_words() -> anyhow::Result<()> {
        let app = Router::new().route(
            "/v1/model",
            get(|| async {
                (
                    StatusCode::CONFLICT,
                    Json(json!({"error": "The runtime provider is disabled."})),
                )
            })
            .put(|| async { (StatusCode::OK, "{not json") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let core = core_client(format!("http://{address}"));
        let query = carillon_protocol::model::ModelQuery {
            agent_id: "default".to_owned(),
            audience_id: None,
        };
        assert_eq!(
            core.model_state(&query).await.err().as_deref(),
            Some("The runtime provider is disabled.")
        );
        let update = carillon_protocol::model::ModelUpdate {
            agent_id: "default".to_owned(),
            audience_id: None,
            scope: carillon_protocol::model::ModelScope::Agent,
            selection: None,
            expected: None,
        };
        assert_eq!(
            core.set_model(&update).await.err().as_deref(),
            Some("The runtime reported an unusable model state.")
        );
        server.abort();
        let unreachable = core_client("http://127.0.0.1:1".to_owned());
        assert_eq!(
            unreachable.model_state(&query).await.err().as_deref(),
            Some("The runtime is not reachable right now.")
        );
        Ok(())
    }

    #[tokio::test]
    async fn ambiguous_thread_creation_recovers_native_thread_before_retrying() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let mut state = discord_state(
            core_client("http://127.0.0.1:1".to_owned()),
            directory.path(),
        )
        .await?;
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
    async fn production_discord_does_not_expose_deterministic_endpoints() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let state = discord_state(
            core_client("http://127.0.0.1:1".to_owned()),
            directory.path(),
        )
        .await?;
        for path in ["/v1/simulate", "/v1/interactions"] {
            let mut request = Request::new(Body::empty());
            *request.method_mut() = Method::POST;
            *request.uri_mut() = Uri::from_static(path);
            let response = discord_router(state.clone(), false)
                .oneshot(request)
                .await?;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
        Ok(())
    }

    fn interaction(channel_id: &str, parent_channel_id: Option<&str>) -> InteractionSource {
        InteractionSource {
            guild_id: Some("100".to_owned()),
            user_id: "200".to_owned(),
            channel_id: channel_id.to_owned(),
            parent_channel_id: parent_channel_id.map(str::to_owned),
            kind: InteractionKind::Command,
            name: "model".to_owned(),
            values: Vec::new(),
            options: Value::Null,
        }
    }

    #[test]
    fn interaction_admission_follows_routes_and_conversations() -> anyhow::Result<()> {
        let channel = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
        let admitted = admit_interaction(&channel, interaction("300", None))
            .map_err(|failure| anyhow!(failure.message))?;
        assert_eq!(admitted.audience_id.as_deref(), Some("discord-100-300"));
        assert!(admitted.interaction.conversation);
        // Threads under a channel route are not conversations the agent joins.
        assert!(admit_interaction(&channel, interaction("400", Some("300"))).is_err());

        let thread = discord_config(DiscordActivation::Mention, DiscordConversation::Thread);
        let in_channel = admit_interaction(&thread, interaction("300", None))
            .map_err(|failure| anyhow!(failure.message))?;
        assert_eq!(in_channel.audience_id, None);
        assert!(!in_channel.interaction.conversation);
        let in_thread = admit_interaction(&thread, interaction("400", Some("300")))
            .map_err(|failure| anyhow!(failure.message))?;
        assert_eq!(in_thread.audience_id.as_deref(), Some("discord-100-400"));
        assert!(in_thread.interaction.conversation);

        for refused in [
            interaction("301", None),
            interaction("400", Some("301")),
            InteractionSource {
                user_id: "999".to_owned(),
                ..interaction("300", None)
            },
            InteractionSource {
                guild_id: Some("101".to_owned()),
                ..interaction("300", None)
            },
            InteractionSource {
                guild_id: None,
                ..interaction("300", None)
            },
        ] {
            let failure = admit_interaction(&thread, refused.clone())
                .err()
                .with_context(|| format!("admitted {refused:?}"))?;
            assert_eq!(failure.status, StatusCode::FORBIDDEN);
        }
        Ok(())
    }

    #[test]
    fn gateway_interactions_map_to_their_source() -> anyhow::Result<()> {
        let base = json!({
            "id": "1",
            "application_id": "2",
            "authorizing_integration_owners": {},
            "token": "t",
            "version": 1,
            "guild_id": "100",
            "member": {
                "user": {"id": "200", "username": "u", "discriminator": "0", "avatar": null},
                "roles": [],
                "joined_at": null,
                "deaf": false,
                "mute": false,
                "flags": 0
            }
        });
        let mut command = base.clone();
        command["type"] = json!(2);
        command["channel"] = json!({"id": "400", "type": 11, "parent_id": "300"});
        command["data"] = json!({
            "id": "3",
            "name": "model",
            "type": 1,
            "options": [{"name": "scope", "type": 3, "value": "agent"}]
        });
        let source = interaction_source(&serde_json::from_value(command)?)
            .context("command was not mapped")?;
        assert_eq!(source.kind, InteractionKind::Command);
        assert_eq!(source.name, "model");
        assert_eq!(source.guild_id.as_deref(), Some("100"));
        assert_eq!(source.user_id, "200");
        assert_eq!(source.channel_id, "400");
        assert_eq!(source.parent_channel_id.as_deref(), Some("300"));
        assert_eq!(source.options[0]["name"], "scope");

        let mut component = base.clone();
        component["type"] = json!(3);
        // A text channel's parent is a category, not a route.
        component["channel"] = json!({"id": "300", "type": 0, "parent_id": "50"});
        component["data"] = json!({
            "custom_id": "model:pick",
            "component_type": 3,
            "values": ["provider/model"]
        });
        let source = interaction_source(&serde_json::from_value(component)?)
            .context("component was not mapped")?;
        assert_eq!(source.kind, InteractionKind::Component);
        assert_eq!(source.name, "model:pick");
        assert_eq!(source.values, vec!["provider/model"]);
        assert_eq!(source.parent_channel_id, None);

        let mut ping = base;
        ping["type"] = json!(1);
        assert!(interaction_source(&serde_json::from_value(ping)?).is_none());
        Ok(())
    }

    async fn interact(
        state: &DiscordState,
        bearer: Option<&str>,
        source: &InteractionSource,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/v1/interactions")
            .header("content-type", "application/json");
        if let Some(bearer) = bearer {
            request = request.header("authorization", format!("Bearer {bearer}"));
        }
        let response = discord_router(state.clone(), true)
            .oneshot(request.body(Body::from(serde_json::to_vec(source)?))?)
            .await?;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        Ok((status, serde_json::from_slice(&body)?))
    }

    #[test]
    fn oversized_reply_content_is_rejected_before_the_discord_edit() {
        let mut reply = carillon_discord_commands::contract::Reply {
            content: "🦀".repeat(2000),
            components: vec![],
        };
        assert!(super::validate_reply(&reply).is_ok());
        reply.content.push('x');
        assert!(super::validate_reply(&reply).is_err());
    }

    #[test]
    fn native_selects_need_no_string_options() -> anyhow::Result<()> {
        for kind in [5, 6, 7, 8] {
            let reply = carillon_discord_commands::contract::Reply {
                content: "Choose".to_owned(),
                components: vec![json!({
                    "type": 1,
                    "components": [{"type": kind, "custom_id": "native", "min_values": 1, "max_values": 1}]
                })],
            };
            assert_eq!(super::validate_reply(&reply)?.len(), 1, "type {kind}");
        }
        for options in [Value::Null, json!([])] {
            let reply = carillon_discord_commands::contract::Reply {
                content: "Choose".to_owned(),
                components: vec![json!({
                    "type": 1, "components": [{"type": 3, "custom_id": "string", "options": options}]
                })],
            };
            assert!(super::validate_reply(&reply).is_err());
        }
        Ok(())
    }

    #[tokio::test]
    async fn deterministic_interactions_run_the_module_and_check_replies() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let mut state = discord_state(
            core_client("http://127.0.0.1:1".to_owned()),
            directory.path(),
        )
        .await?;
        let button = json!({"type": 1, "components": [{"type": 2, "style": 2, "custom_id": "model:refresh", "label": "Refresh"}]});
        state.commands = stub_commands(
            directory.path(),
            &replying("Current model", std::slice::from_ref(&button)),
        )
        .await?;

        let (status, body) = interact(&state, Some("token"), &interaction("300", None)).await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        let reply: Reply = serde_json::from_value(body)?;
        assert_eq!(reply.content, "Current model");
        assert_eq!(reply.components, vec![button.clone()]);

        let (status, _) = interact(&state, None, &interaction("300", None)).await?;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = interact(&state, Some("token"), &interaction("301", None)).await?;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body["error"],
            "channel is not configured as a Discord route"
        );

        // Replies Discord would refuse are reported instead of sent.
        let option =
            |index: usize| json!({"label": format!("Option {index}"), "value": index.to_string()});
        let select = |options: Vec<Value>| json!({"type": 1, "components": [{"type": 3, "custom_id": "model:model", "options": options}]});
        for (components, complaint) in [
            (vec![json!("not a component")], "invalid components"),
            (vec![button.clone(); 6], "action rows"),
            (
                vec![select((0..26).map(option).collect())],
                "unusable component",
            ),
            (
                vec![
                    json!({"type": 1, "components": [{"type": 2, "style": 2, "custom_id": "x".repeat(101), "label": "X"}]}),
                ],
                "unusable component",
            ),
            (vec![select(Vec::new())], "without options"),
            (
                vec![json!({"type": 1, "components": []})],
                "empty action row",
            ),
        ] {
            state.commands = stub_commands(directory.path(), &replying("bad", &components)).await?;
            let (status, body) = interact(&state, Some("token"), &interaction("300", None)).await?;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
            assert!(
                body["error"]
                    .as_str()
                    .is_some_and(|error| error.contains(complaint)),
                "{body}"
            );
        }
        state.commands = stub_commands(
            directory.path(),
            &replying("full", &[select((0..25).map(option).collect())]),
        )
        .await?;
        let (status, body) = interact(&state, Some("token"), &interaction("300", None)).await?;
        assert_eq!(status, StatusCode::OK, "{body}");

        state.commands =
            stub_commands(directory.path(), r#"{"Failure":"no such command"}"#).await?;
        let (status, body) = interact(&state, Some("token"), &interaction("300", None)).await?;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains("no such command"))
        );
        Ok(())
    }

    async fn reload(
        state: &DiscordState,
        bearer: Option<&str>,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/v1/commands/reload");
        if let Some(bearer) = bearer {
            request = request.header("authorization", format!("Bearer {bearer}"));
        }
        let response = discord_router(state.clone(), false)
            .oneshot(request.body(Body::empty())?)
            .await?;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        Ok((status, serde_json::from_slice(&body)?))
    }

    #[tokio::test]
    async fn command_reload_requires_the_operator_credential() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let state = discord_state(
            core_client("http://127.0.0.1:1".to_owned()),
            directory.path(),
        )
        .await?;
        let module = directory.path().join("commands.wasm");
        for bearer in [None, Some("token"), Some("wrong")] {
            let (status, _) = reload(&state, bearer).await?;
            assert_eq!(status, StatusCode::FORBIDDEN, "{bearer:?}");
        }
        let (status, body) = reload(&state, Some("operator")).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"commands": 1, "changed": false, "registered": false})
        );

        write_module(
            &module,
            &describing(&[definition("model"), definition("other")]),
            &replying("unused", &[]),
        )?;
        let (status, body) = reload(&state, Some("operator")).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"commands": 2, "changed": true, "registered": false})
        );

        std::fs::write(&module, b"\0asm broken")?;
        let (status, body) = reload(&state, Some("operator")).await?;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains("load command module"))
        );
        assert_eq!(state.commands.definitions().len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn reloads_serve_the_replacement_only_once_discord_confirmed_it() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let status = Arc::new(AtomicU16::new(200));
        let (address, server) = registration_server(status.clone()).await?;
        let mut state = discord_state(
            core_client("http://127.0.0.1:1".to_owned()),
            directory.path(),
        )
        .await?;
        state.app = Some(Arc::new(app(&address.to_string(), 1, 100)));
        let module = directory.path().join("commands.wasm");

        // Nothing is recorded yet, so even unchanged definitions register.
        let (code, body) = reload(&state, Some("operator")).await?;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({"commands": 1, "changed": false, "registered": true})
        );
        let (_, body) = reload(&state, Some("operator")).await?;
        assert_eq!(
            body,
            json!({"commands": 1, "changed": false, "registered": false})
        );

        write_module(
            &module,
            &describing(&[definition("model"), definition("other")]),
            &replying("replacement", &[]),
        )?;
        for (code, expected) in [
            (400, "Discord rejected the commands"),
            (500, "Discord did not confirm the commands"),
        ] {
            status.store(code, Ordering::SeqCst);
            let (status, body) = reload(&state, Some("operator")).await?;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
            let error = body["error"].as_str().unwrap_or_default();
            assert!(error.starts_with(expected), "{error}");
            assert!(
                error.ends_with("the previous command module keeps serving"),
                "{error}"
            );
            assert_eq!(state.commands.definitions().len(), 1);
            let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
            assert_eq!(reply["content"], "stub reply");
        }

        status.store(200, Ordering::SeqCst);
        let (code, body) = reload(&state, Some("operator")).await?;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({"commands": 2, "changed": true, "registered": true})
        );
        let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
        assert_eq!(reply["content"], "replacement");
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn a_full_message_queue_refuses_instead_of_holding_the_gateway() -> anyhow::Result<()> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let inbound = InboundMessage {
            request: SimulateRequest {
                event_id: "1".to_owned(),
                guild_id: Some("100".to_owned()),
                user_id: "200".to_owned(),
                channel_id: "300".to_owned(),
                parent_channel_id: None,
                content: "hello".to_owned(),
            },
            native: Value::Null,
        };
        assert!(queue_message(&sender, inbound.clone())?.is_ok());
        let refused = queue_message(&sender, inbound.clone())?
            .err()
            .context("a second message should be refused while the first waits")?;
        assert_eq!(refused.request.event_id, "1");
        receiver.recv().await.context("queued message")?;
        assert!(queue_message(&sender, inbound.clone())?.is_ok());
        drop(receiver);
        assert!(queue_message(&sender, inbound).is_err());
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
        use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

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
                // Core commits the turn on the first attempt and recognises
                // the retry as the same event.
                let receipt = EventReceipt {
                    event_id: submitted.event_id,
                    turn_id: "committed".to_owned(),
                    state: TurnState::Queued,
                    duplicate: attempt == 1,
                };
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
        assert_eq!(receipt.turn_id, "committed");
        assert_eq!(receipt.state, TurnState::Queued);
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
        let directory = tempfile::tempdir()?;
        let mut state =
            discord_state(core_client(format!("http://{address}")), directory.path()).await?;
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

    /// A core whose first delivery poll fails ambiguously, whose
    /// reconciliation reports one abandoned claim, and whose second poll
    /// ends the loop; plus a Discord that records reaction requests.
    fn reconciling_core(trace: Arc<tokio::sync::Mutex<Vec<String>>>) -> Router {
        let polls = Arc::new(AtomicUsize::new(0));
        let reconcile_trace = trace.clone();
        let poll_trace = trace.clone();
        let discord_trace = trace.clone();
        Router::new()
            .route(
                "/v1/deliveries/reconcile",
                post(move || {
                    let trace = reconcile_trace.clone();
                    async move {
                        trace.lock().await.push("reconcile".to_owned());
                        Json(DeliveryReconciliation {
                            feedback_target_ids: vec!["300:400".to_owned()],
                        })
                    }
                }),
            )
            .route(
                "/v1/deliveries",
                get(move || {
                    let trace = poll_trace.clone();
                    let attempt = polls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        trace.lock().await.push("poll".to_owned());
                        if attempt == 0 {
                            // The claim may have committed before core failed.
                            StatusCode::INTERNAL_SERVER_ERROR
                        } else {
                            // Terminate the infinite production loop after observing resumed polling.
                            StatusCode::FORBIDDEN
                        }
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
                let trace = trace.clone();
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
    async fn ambiguous_claim_failures_reconcile_before_polling_again() -> anyhow::Result<()> {
        let trace = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
        let app = reconciling_core(trace.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let directory = tempfile::tempdir()?;
        let mut state =
            discord_state(core_client(format!("http://{address}")), directory.path()).await?;
        state.discord.reactions = true;
        let expected_uncertain = state.discord.uncertain_emoji.clone();
        let http = Arc::new(
            twilight_http::Client::builder()
                .proxy(address.to_string(), true)
                .ratelimiter(None)
                .build(),
        );
        let outcome =
            tokio::time::timeout(Duration::from_secs(5), delivery_loop(state, http)).await;
        server.abort();
        assert!(
            outcome?
                .err()
                .context("delivery loop unexpectedly succeeded")?
                .to_string()
                .contains("delivery claim rejected")
        );
        let requests = trace.lock().await;
        // Reconciliation and uncertainty feedback precede resumed polling.
        // Processing-reaction cleanup is not part of this recovery contract.
        let required = [
            "poll".to_owned(),
            "reconcile".to_owned(),
            format!("PUT 300:400 {expected_uncertain}"),
            "poll".to_owned(),
        ];
        let mut remaining = requests.iter();
        for event in &required {
            assert!(
                remaining.any(|request| request == event),
                "missing {event} in order: {requests:?}"
            );
        }
        assert_eq!(
            requests.iter().filter(|request| *request == "poll").count(),
            2
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.starts_with("unexpected"))
        );
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
