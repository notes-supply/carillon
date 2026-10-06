//! Gateway and operator HTTP assembly. Policies live in the component modules.
use crate::conversations::{
    AdmittedInteraction, InteractionSource, SourceAttachment, admit_interaction,
};
use crate::{
    attachments::Downloads,
    error::discord_failure_is_terminal,
    inbound::{Ingress, PREPARATION_DEADLINE, QueuedMessage, known_receipt, lanes},
};

use crate::{
    InterfaceError,
    activity::{CardRecords, activity_monitor},
    client::CoreClient,
    commands::{
        Change, ChangeError, CommandFiles, Commands, DiscordApp, MODULE_LIMIT, ModelCalls, Refusal,
        Status,
    },
    config::{DiscordConfig, state_directory},
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
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use carillon_discord_commands::contract::{InteractionKind, Reply};
use carillon_installation::config::{
    LoadedConfig, ProviderRole, RELEASE_VERSION, attachment_staging, read_token_file, token_matches,
};
use carillon_protocol::model::{
    Delivery, DeliveryOutcome, DeliveryReconciliation, EventReceipt, Health, ModelQuery, TurnState,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::{Semaphore, mpsc},
    time::{Instant, sleep, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;
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
    /// The provider's attachment downloads and staging, in either mode.
    downloads: Arc<Downloads>,
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
    // Disabled on purpose, the provider ends successfully, so its service is
    // not restarted, before it reads credentials or touches any state.
    if !provider.enabled {
        info!(provider = %provider.id, "Discord interface provider is disabled; not serving");
        return Ok(());
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
    let participation = Arc::new(Participation::load(
        state_directory(&config, &provider.id).join("participation.json"),
    )?);
    let commands = Arc::new(
        Commands::load(CommandFiles::of(
            &config,
            &provider.id,
            options.commands.to_path_buf(),
        ))
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
    // This instance owns the provider's state: staging left by an earlier
    // one is abandoned and removed.
    let downloads = Arc::new(Downloads::open(
        attachment_staging(&config.value.data_dir, &provider.id),
        discord.attachments,
    )?);
    let state = DiscordState {
        discord,
        health,
        core,
        participation,
        commands,
        app: None,
        downloads,
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
    // Cosmetic: records that cannot be read turn cards off, not the provider.
    let cards =
        CardRecords::open(state_directory(&config, &provider.id).join("activity-cards.json"));
    connect(
        state,
        listener,
        read_token_file(discord_token_path)?,
        reconciled,
        cards,
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
    cards: Option<CardRecords>,
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
    state.commands.reconcile(&app).await?;
    state.app = Some(app);
    for target in reconciled.feedback_target_ids {
        terminal_feedback(&http, &state.discord, &target, Feedback::Uncertain).await;
    }
    let (messages, receiver) = mpsc::channel(lanes::MAX_PENDING);
    let delivery_state = state.clone();
    let stopping = CancellationToken::new();
    let ingress = Arc::new(Ingress {
        core: state.core.clone(),
        config: state.discord.clone(),
        participation: state.participation.clone(),
        http: http.clone(),
        downloads: state.downloads.clone(),
        refusals: Arc::new(Semaphore::new(lanes::REFUSALS)),
        stopping: stopping.clone(),
    });
    let dispatcher = tokio::spawn(lanes::dispatch(ingress.clone(), receiver));
    let mut terminate = signal(SignalKind::terminate()).context("handle termination")?;
    let router = discord_router(state.clone(), false);
    let (outcomes, observed) = mpsc::unbounded_channel();
    let monitor = activity_monitor(
        state.core.clone(),
        http.clone(),
        state.discord.clone(),
        cards.map(Arc::new),
        observed,
    );
    // Deliveries outlast whatever ends the provider: a claimed one is still
    // sent and its outcome recorded.
    let mut deliveries = tokio::spawn(delivery_loop(
        delivery_state.core,
        delivery_state.discord,
        http,
        outcomes,
        stopping.clone(),
    ));
    let mut delivering = true;
    let outcome = tokio::select! {
        result = axum::serve(listener, router).into_future() => result.map_err(Into::into),
        result = gateway_loop(state, discord_token, messages, ingress) => {
            result.context("Discord gateway stopped")
        },
        result = &mut deliveries => {
            delivering = false;
            result.map_err(Into::into).flatten().context("Discord delivery worker stopped")
        },
        () = monitor => Err(anyhow::anyhow!("Discord activity monitor stopped")),
        interrupted = tokio::signal::ctrl_c() => interrupted.context("handle interruption"),
        _ = terminate.recv() => Ok(()),
    };
    // No new message or delivery is taken. Preparation not yet handed to
    // core ends; a submission core may have received is settled, so its
    // staged files stay until core no longer needs them; a claimed delivery
    // is sent and acknowledged.
    stopping.cancel();
    let prepared = dispatcher.await;
    if delivering {
        deliveries
            .await
            .map_err(Into::into)
            .flatten()
            .context("Discord delivery worker stopped")?;
    }
    prepared.context("Discord message preparation failed")?;
    outcome
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
        .route("/v1/operations", post(discord_operation))
        .route(
            "/v1/commands",
            get(command_status)
                .put(install_commands)
                .delete(reset_commands),
        );
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

/// Operator actions need the operator credential, not the interface's.
fn authorize_operator(state: &DiscordState, headers: &HeaderMap) -> Result<(), InterfaceError> {
    if !token_matches(&state.operator_token_hash, bearer(headers)) {
        return Err(InterfaceError {
            status: StatusCode::FORBIDDEN,
            message: "Discord operator endpoints require the operator credential".to_owned(),
        });
    }
    Ok(())
}

async fn discord_operation(
    State(state): State<DiscordState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, InterfaceError> {
    authorize_operator(&state, &headers)?;
    let bytes = axum::body::to_bytes(body, 6 * 1024 * 1024)
        .await
        .map_err(|_| InterfaceError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "Discord operation exceeds 6 MiB; nothing was changed".to_owned(),
        })?;
    let operation: crate::operations::Operation =
        serde_json::from_slice(&bytes).map_err(|_| InterfaceError {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid Discord operation; check the action and its fields".to_owned(),
        })?;
    let Some(app) = &state.app else {
        return Ok((StatusCode::SERVICE_UNAVAILABLE, Json(json!({"outcome":"unavailable","message":"Discord operations require a connected provider; deterministic mode does not call Discord."}))).into_response());
    };
    Ok(match operation.execute(&app.http, app.guild_id).await {
        Ok(result) => Json(result).into_response(),
        Err(failure) => {
            let status = match failure.outcome {
                "rejected" => StatusCode::BAD_REQUEST,
                "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::BAD_GATEWAY,
            };
            (status, Json(failure)).into_response()
        }
    })
}

async fn command_status(
    State(state): State<DiscordState>,
    headers: HeaderMap,
) -> Result<Json<Status>, InterfaceError> {
    authorize_operator(&state, &headers)?;
    Ok(Json(state.commands.status(state.app.as_deref())))
}

/// Serves the uploaded module and saves it as the installed module. When the
/// provider has a Discord application, definitions the record does not show
/// registered are registered first; the module serves only once Discord
/// confirmed them. The body is read only after authentication.
async fn install_commands(
    State(state): State<DiscordState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, InterfaceError> {
    authorize_operator(&state, &headers)?;
    let bytes = axum::body::to_bytes(body, MODULE_LIMIT)
        .await
        .map_err(|_| InterfaceError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!(
                "a command module may have at most {MODULE_LIMIT} bytes (16 MiB); nothing was changed"
            ),
        })?;
    let (commands, app) = (state.commands.clone(), state.app.clone());
    let change = tokio::spawn(async move { commands.install(&bytes, app.as_deref()).await });
    Ok(changed(&state, change.await))
}

/// Serves the release's module again and removes the installed one.
async fn reset_commands(
    State(state): State<DiscordState>,
    headers: HeaderMap,
) -> Result<Response, InterfaceError> {
    authorize_operator(&state, &headers)?;
    let (commands, app) = (state.commands.clone(), state.app.clone());
    let change = tokio::spawn(async move { commands.reset(app.as_deref()).await });
    Ok(changed(&state, change.await))
}

/// A change, or why the serving module stayed in place and what the
/// operator can do about it. Changes run as their own task, so an operator
/// who stops waiting cannot cancel one between registration and activation.
fn changed(
    state: &DiscordState,
    result: Result<Result<Change, ChangeError>, tokio::task::JoinError>,
) -> Response {
    let failure = match result {
        Ok(Ok(change)) => return Json(change).into_response(),
        Ok(Err(failure)) => failure,
        Err(failure) => {
            error!(error = %failure, "command module change failed");
            return InterfaceError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!(
                    "the command module change failed unexpectedly ({failure}); check \
                     `carillon discord commands` and run the same command again"
                ),
            }
            .into_response();
        }
    };
    error!(error = %failure, "command module change refused");
    let (status, error) = match failure {
        ChangeError::Busy(installation) => (
            StatusCode::CONFLICT,
            format!(
                "another Carillon maintenance operation is running for {}; nothing was changed. \
                 Run the same command again once it finishes",
                installation.display()
            ),
        ),
        ChangeError::Lock(failure) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not take the maintenance lock: {failure:#}; nothing was changed"),
        ),
        ChangeError::Unusable(failure) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("not a usable command module: {failure:#}; nothing was changed"),
        ),
        ChangeError::Record(failure) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "could not clear the command registration record ({failure}), so Discord was \
                 not contacted and nothing was changed. Check the provider's state directory, \
                 then run the same command again"
            ),
        ),
        ChangeError::Register(failure) if discord_failure_is_terminal(&failure) => (
            StatusCode::BAD_GATEWAY,
            format!(
                "Discord rejected the commands: {failure:#}. The serving and saved modules are \
                 unchanged; correct the module's command definitions and try again"
            ),
        ),
        ChangeError::Register(failure) => (
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "Discord did not confirm the commands: {failure:#}. It may have applied them; \
                 the serving and saved modules are unchanged. Run the same command again once \
                 Discord answers"
            ),
        ),
    };
    (
        status,
        Json(Refusal {
            error,
            serving: state.commands.serving(),
        }),
    )
        .into_response()
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
    // Files of an admitted message follow the gateway's rules: checked
    // against the provider's policy, then fetched from Discord's CDN,
    // unless core already holds the message. Without a Discord session, an
    // expired address cannot be refreshed.
    let refused = |refusal: crate::attachments::Refusal| InterfaceError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        message: refusal.0,
    };
    let mut known = None;
    let mut staged = None;
    if !plan.attachments.is_empty() {
        state.downloads.admit(&plan.attachments).map_err(refused)?;
        let deadline = Instant::now() + PREPARATION_DEADLINE;
        // A message core holds or is still capturing is not fetched again.
        known = timeout_at(deadline, known_receipt(&state.core, &plan.message_id))
            .await
            .map_err(|_| InterfaceError {
                status: StatusCode::GATEWAY_TIMEOUT,
                message: "core did not say whether it holds this message in time".to_owned(),
            })?;
        if known.is_none() {
            staged = Some(
                state
                    .downloads
                    .stage(None, &plan, deadline)
                    .await
                    .map_err(refused)?,
            );
        }
    }
    let mut resolved = prepare_deterministic_conversation(&state.participation, plan)?;
    resolved.event.attachments = staged.as_ref().map(|staged| staged.manifest.clone());
    let mut feedback = Vec::new();
    if state.discord.reactions {
        feedback.push("processing".to_owned());
    }
    let receipt = if let Some(receipt) = known {
        receipt
    } else {
        let settled = match state.core.submit_event(&resolved.event).await {
            // Core may still be capturing the staged files: they stay until
            // core settles the submission.
            Err(failure) if staged.is_some() && failure.is_transient() => {
                known_receipt(&state.core, &resolved.event.event_id)
                    .await
                    .ok_or(failure)
            }
            outcome => outcome,
        };
        // Core holds its own copy once it has answered; the staged files go.
        drop(staged);
        settled?
    };
    // Simulation settles its own turn's delivery and no other. A turn still
    // queued or running is waited for; a finished one, whose simulation may
    // have stopped waiting before its answer, is settled if still pending.
    let turn = &receipt.turn_id;
    let delivery = if matches!(receipt.state, TurnState::Queued | TurnState::Running) {
        let delivery = timeout(Duration::from_secs(10), async {
            loop {
                if let Some(delivery) = state.core.claim_turn_delivery(turn).await? {
                    return Ok::<_, InterfaceError>(delivery);
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| InterfaceError {
            status: StatusCode::GATEWAY_TIMEOUT,
            message: "timed out waiting for deterministic delivery".to_owned(),
        })??;
        Some(delivery)
    } else {
        state.core.claim_turn_delivery(turn).await?
    };
    if let Some(delivery) = &delivery {
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
    }
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
    ingress: Arc<Ingress>,
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
                };
                let native = native_message(&message);
                queue_or_refuse(&messages, &ingress, QueuedMessage { request, native })?;
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

/// The message as Discord sent it, for the runtime's native context, without
/// the signed addresses of its attachments: those expire, and accepted
/// files are core's captures, not CDN links.
fn native_message(message: &twilight_model::channel::Message) -> Value {
    let mut native = serde_json::to_value(message).unwrap_or(Value::Null);
    strip_attachment_addresses(&mut native);
    native
}

/// Remove signed addresses from every attachment list, including those of
/// referenced and forwarded messages nested in the payload, and the signature
/// of any other attachment address, such as an embed's.
fn strip_attachment_addresses(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, nested) in object.iter_mut() {
                if key == "attachments"
                    && let Value::Array(attachments) = nested
                {
                    for attachment in attachments.iter_mut().filter_map(Value::as_object_mut) {
                        attachment.remove("url");
                        attachment.remove("proxy_url");
                    }
                }
                strip_attachment_addresses(nested);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_attachment_addresses),
        Value::String(text) => {
            if let Ok(mut url) = reqwest::Url::parse(text)
                && matches!(
                    url.host_str(),
                    Some("cdn.discordapp.com" | "media.discordapp.net")
                )
                && url.path().starts_with("/attachments/")
                && url.query().is_some()
            {
                url.set_query(None);
                *text = url.to_string();
            }
        }
        _ => {}
    }
}

/// Hands a message to the worker without waiting for room. The gateway must
/// keep reading so heartbeats and interactions go on while the worker is
/// retrying; a message that finds the queue full is returned refused, and
/// the person who sent it is told it failed rather than left waiting.
/// Hands a gateway message to the lanes. One that finds their queue full is
/// refused as overload, as one beyond a lane's bound is: only a message
/// admission would activate is answered, within the shared refusal bound.
pub(crate) fn queue_or_refuse(
    messages: &mpsc::Sender<QueuedMessage>,
    ingress: &Arc<Ingress>,
    inbound: QueuedMessage,
) -> Result<()> {
    if let Err(refused) = queue_message(messages, inbound)? {
        lanes::refuse_overloaded(ingress, refused.request);
    }
    Ok(())
}

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
