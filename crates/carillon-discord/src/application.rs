//! Gateway and operator HTTP assembly. Policies live in the component modules.
use crate::conversations::{AdmittedInteraction, InteractionSource, admit_interaction};
use crate::{
    error::discord_failure_is_terminal,
    inbound::{QueuedMessage, message_worker},
};

use crate::{
    InterfaceError,
    client::CoreClient,
    commands::{Commands, DiscordApp, ModelCalls, ReloadError, ReloadReport},
    config::DiscordConfig,
    conversations::{
        IncomingMessage, Participation, admit_identity, admit_message,
        prepare_deterministic_conversation,
    },
    delivery::{Feedback, delivery_loop, terminal_feedback},
};
#[cfg(test)]
use crate::{
    conversations::{MessagePlan, prepare_conversation},
    error::discord_status_is_terminal,
};
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use carillon_discord_commands::contract::{InteractionKind, Reply};
use carillon_installation::config::{
    LoadedConfig, ProviderRole, RELEASE_VERSION, read_token_file, token_matches,
};
use carillon_protocol::model::{
    Delivery, DeliveryOutcome, DeliveryReconciliation, EventReceipt, Health, ModelQuery,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::mpsc,
    time::{sleep, timeout},
};
use tracing::{error, info, warn};
use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use twilight_http::Client as DiscordHttp;
use twilight_model::channel::ChannelType;
use twilight_model::{
    application::interaction::{
        Interaction as DiscordInteraction, InteractionData, InteractionType,
    },
    channel::message::{Component, MessageFlags, MessageType, component::SelectMenuType},
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::Id,
};
use twilight_validate::component::{COMPONENT_COUNT, component_v1};

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SimulateResponse {
    pub receipt: EventReceipt,
    pub delivery: Option<Delivery>,
    pub conversation_id: String,
    pub feedback: Vec<String>,
}

impl IntoResponse for InterfaceError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
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

/// Everything `serve` needs besides the registry.
pub(crate) struct ServeOptions<'a> {
    pub provider_id: &'a str,
    pub token_file: &'a Path,
    pub discord_token_file: Option<&'a Path>,
    pub commands: &'a Path,
    pub deterministic: bool,
}

pub(crate) async fn serve(config: LoadedConfig, options: ServeOptions<'_>) -> Result<()> {
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
    let core = CoreClient::new(
        format!("http://{}", config.value.core.listen),
        provider.id.clone(),
        token,
    );
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
        message_worker(
            worker_state.core,
            worker_state.discord,
            worker_state.participation,
            worker_http,
            receiver,
        )
        .await;
    });
    let router = discord_router(state.clone(), false);
    tokio::select! {
        result = axum::serve(listener, router).into_future() => result.map_err(Into::into),
        result = gateway_loop(state, discord_token, messages) => {
            result.context("Discord gateway stopped")
        },
        result = delivery_loop(delivery_state.core, delivery_state.discord, http) => {
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
fn authorize_interface(state: &DiscordState, headers: &HeaderMap) -> Result<(), InterfaceError> {
    if !state.core.authenticates(bearer(headers)) {
        return Err(InterfaceError {
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
) -> Result<Json<ReloadReport>, InterfaceError> {
    if !token_matches(&state.operator_token_hash, bearer(&headers)) {
        return Err(InterfaceError {
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
                InterfaceError {
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
    Json(request): Json<IncomingMessage>,
) -> Result<Response, InterfaceError> {
    authorize_interface(&state, &headers)?;
    let Some(plan) = admit_message(
        &state.discord,
        &state.participation,
        request,
        Value::String("deterministic-local-discord".to_owned()),
    )?
    else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let resolved = prepare_deterministic_conversation(&state.participation, plan)?;
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
                        return Err(InterfaceError {
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
        .map_err(|_| InterfaceError {
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
) -> Result<Json<Reply>, InterfaceError> {
    authorize_interface(&state, &headers)?;
    let admitted = admit_interaction(&state.discord, source)?;
    let reply = run_interaction(&state, admitted)
        .await
        .map_err(|error| InterfaceError {
            status: StatusCode::BAD_GATEWAY,
            message: error.to_string(),
        })?;
    validate_reply(&reply).map_err(|error| InterfaceError {
        status: StatusCode::BAD_GATEWAY,
        message: error.to_string(),
    })?;
    Ok(Json(reply))
}

async fn gateway_loop(
    state: DiscordState,
    token: String,
    messages: mpsc::Sender<QueuedMessage>,
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
                let request = IncomingMessage {
                    event_id: message.id.to_string(),
                    guild_id: guild,
                    user_id: message.author.id.to_string(),
                    channel_id: message.channel_id.to_string(),
                    parent_channel_id: None,
                    content: message.content.clone(),
                };
                let native = serde_json::to_value(&*message).unwrap_or(Value::Null);
                if let Err(refused) = queue_message(&messages, QueuedMessage { request, native })? {
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
    messages: &mpsc::Sender<QueuedMessage>,
    inbound: QueuedMessage,
) -> Result<Result<(), QueuedMessage>> {
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

#[cfg(test)]
pub(crate) mod tests;

/// Run the installed Discord command-line application.
///
/// # Errors
/// Returns configuration, startup, or service failures.
pub async fn run() -> Result<()> {
    crate::cli::run().await
}
