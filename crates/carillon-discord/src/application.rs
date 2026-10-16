//! Gateway and operator HTTP assembly. Policies live in the component modules.
mod interactions;

#[cfg(test)]
use interactions::interaction_source;
use interactions::{handle_interaction, run_interaction};

use crate::conversations::{InteractionSource, SourceAttachment, admit_interaction};
use crate::{
    attachments::Downloads,
    error::discord_failure_is_terminal,
    gateway::{Dispatch, Gateway, Identity, record_path, take_record},
    inbound::{
        Ingress, PREPARATION_DEADLINE, QueuedMessage, Settlements, known_receipt,
        lanes::{self, Lanes, Queued},
    },
};

use crate::{
    InterfaceError,
    activity::{CardRecords, activity_monitor},
    client::CoreClient,
    config::{self, DiscordConfig, Settings, state_directory},
    conversations::{
        IncomingMessage, Participation, admit_identity, admit_message,
        prepare_deterministic_conversation,
    },
    delivery::{Feedback, PROMPT_FEEDBACK, Reactions, delivery_loop},
    module::{
        ChangeError, Declared, DiscordApp, Module, ModuleFiles, Refusal, Status, answer::answer,
    },
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
use carillon_discord_module::contract::{CardRequest, Reply};
use carillon_installation::config::{
    LoadedConfig, ProviderRole, RELEASE_VERSION, attachment_staging, read_token_file, token_matches,
};
use carillon_protocol::model::{
    Delivery, DeliveryOutcome, DeliveryReconciliation, EventReceipt, Health, Job, TurnState,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::{Semaphore, mpsc},
    task::{JoinError, JoinSet},
    time::{Instant, sleep, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use twilight_gateway::Event;
use twilight_http::Client as DiscordHttp;
use twilight_model::{
    channel::message::{Embed, MessageType},
    id::Id,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SimulateResponse {
    pub receipt: EventReceipt,
    pub delivery: Option<Delivery>,
    /// The card the monitor shows of the message's job, from what core
    /// recorded of it, when the job earned a summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<Embed>,
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
    /// The settings serving. Each message or interaction keeps the snapshot
    /// it was admitted with.
    settings: Settings,
    /// The registry the provider started with: its settings file is read
    /// and validated against it, and its operator credential authorizes the
    /// operator endpoints.
    installation: Arc<LoadedConfig>,
    health: Health,
    core: CoreClient,
    participation: Arc<Participation>,
    module: Arc<Module>,
    /// Absent in deterministic mode, where no Discord application exists.
    connection: Option<Connection>,
    downloads: Arc<Downloads>,
}

#[derive(Clone)]
struct Connection {
    app: Arc<DiscordApp>,
    reactions: Reactions,
    /// The conversation lanes `/command` submissions take their place in.
    lanes: Lanes,
}

impl DiscordState {
    fn app(&self) -> Option<&DiscordApp> {
        self.connection.as_ref().map(|connection| &*connection.app)
    }
}

pub(crate) struct ServeOptions<'a> {
    pub provider_id: &'a str,
    pub token_file: &'a Path,
    pub module: &'a Path,
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
    let bot_token_file = discord.bot_token_path(&config.root);
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
    let module = Arc::new(
        Module::load(
            ModuleFiles::of(&config, &provider.id, options.module.to_path_buf()),
            discord.module_source(&config.root),
        )
        .await?,
    );
    let core = CoreClient::new(
        format!("http://{}", config.value.core.listen),
        provider.id.clone(),
        token,
    )?;
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
        settings: Settings::new(discord),
        installation: Arc::new(config),
        health,
        core,
        participation,
        module,
        connection: None,
        downloads,
    };
    info!(url = %provider.url, deterministic = options.deterministic, "Discord interface provider ready");
    if options.deterministic {
        axum::serve(listener, discord_router(state, true)).await?;
        return Ok(());
    }
    // Cosmetic: records that cannot be read turn cards off, not the provider.
    let cards = CardRecords::open(
        state_directory(&state.installation, &provider.id).join("activity-cards.json"),
    );
    let discord_token = read_token_file(&bot_token_file)?;
    let http = Arc::new(DiscordHttp::new(discord_token.clone()));
    connect(state, listener, http, discord_token, reconciled, cards).await
}

async fn connect(
    mut state: DiscordState,
    listener: TcpListener,
    http: Arc<DiscordHttp>,
    discord_token: String,
    reconciled: DeliveryReconciliation,
    cards: Option<CardRecords>,
) -> Result<()> {
    // Identity applies at a restart only, so the settings serving now say it.
    let identity = state.settings.current();
    let app = Arc::new(application(&http, &identity).await?);
    state.module.reconcile(&app).await?;
    let reactions = Reactions::new(http.clone(), state.settings.clone());
    for target in reconciled.feedback_target_ids {
        reactions.outcome(&target, Feedback::Uncertain).await;
    }
    let (messages, receiver) = mpsc::channel(lanes::MAX_PENDING);
    state.connection = Some(Connection {
        app,
        reactions: reactions.clone(),
        lanes: Lanes::new(messages.clone()),
    });
    let delivery_state = state.clone();
    let stopping = CancellationToken::new();
    let (settled, settlements) = mpsc::unbounded_channel();
    let ingress = Arc::new(Ingress {
        core: state.core.clone(),
        participation: state.participation.clone(),
        http: http.clone(),
        reactions: reactions.clone(),
        downloads: state.downloads.clone(),
        refusals: Arc::new(Semaphore::new(lanes::REFUSALS)),
        stopping: stopping.clone(),
        settlements: Settlements::new(settled),
    });
    let dispatcher = tokio::spawn(lanes::dispatch(ingress.clone(), receiver));
    let mut terminate = signal(SignalKind::terminate()).context("handle termination")?;
    let router = discord_router(state.clone(), false);
    let (delivering, heard) = mpsc::unbounded_channel();
    // The monitor hears of every delivery the loop takes up, through the
    // last a stop lets it finish, and then finishes the cards those settle.
    let mut monitoring = tokio::spawn(activity_monitor(
        state.core.clone(),
        http.clone(),
        state.module.clone(),
        state.settings.clone(),
        cards,
        heard,
        stopping.clone(),
    ));
    let mut watching = true;
    // Deliveries outlast whatever ends the provider: a claimed one is still
    // sent and its outcome recorded.
    let mut deliveries = tokio::spawn(delivery_loop(
        delivery_state.core,
        reactions.clone(),
        http,
        delivering,
        stopping.clone(),
    ));
    let mut delivering = true;
    // Interactions are this function's tasks, not the gateway's, so ending
    // the gateway does not cut short one it already took.
    let mut interactions = JoinSet::new();
    // A session recorded by a graceful stop is resumed only by the same
    // provider, bot, guild and token, and only once. It is consumed only now,
    // when nothing that can still fail stands before the gateway serving it.
    let record = record_path(&state_directory(&state.installation, &state.health.id));
    let owner = Identity::new(
        &state.health.id,
        &identity.bot_user_id,
        &identity.guild_id,
        &discord_token,
    );
    let mut gateway = Gateway::new(discord_token, take_record(&record, &owner), settlements);
    let outcome = tokio::select! {
        result = axum::serve(listener, router).into_future() => result.map_err(Into::into),
        result = gateway_loop(&state, &mut gateway, &messages, &ingress, &mut interactions) => {
            result.context("Discord gateway stopped")
        },
        result = &mut deliveries => {
            delivering = false;
            result.map_err(Into::into).flatten().context("Discord delivery worker stopped")
        },
        _ = &mut monitoring => {
            watching = false;
            Err(anyhow::anyhow!("Discord activity monitor stopped"))
        },
        interrupted = tokio::signal::ctrl_c() => interrupted.context("handle interruption"),
        _ = terminate.recv() => Ok(()),
    };
    // No new message, interaction or delivery is taken: the gateway closes
    // first, keeping its session resumable. Preparation not yet handed to
    // core ends; a submission core may have received is settled, so its
    // staged files stay until core no longer needs them; an interaction
    // already acknowledged finishes its invocation and attempts its reply; a
    // claimed delivery is sent and acknowledged, and the card its outcome
    // settles shows its summary. Reactions still under way then get a moment
    // to end before the rest are cancelled. Only then is the session recorded,
    // with a replay floor below every message that did not settle.
    gateway.close().await;
    stopping.cancel();
    answered(&mut interactions).await;
    let prepared = dispatcher.await;
    let delivered = if delivering {
        deliveries.await.map_err(Into::into).flatten()
    } else {
        Ok(())
    };
    // Its outcomes now closed, the monitor ends within its bound.
    let watched = if watching { monitoring.await } else { Ok(()) };
    reactions.stop(PROMPT_FEEDBACK).await;
    gateway.record(&record, &owner);
    prepared.context("Discord message preparation failed")?;
    delivered.context("Discord delivery worker stopped")?;
    watched.context("Discord activity monitor failed")?;
    outcome
}

/// The Discord application of the bot the token authenticates, which must
/// be the configured bot.
async fn application(http: &Arc<DiscordHttp>, identity: &DiscordConfig) -> Result<DiscordApp> {
    let current_user = http.current_user().await?.model().await?;
    if current_user.id.to_string() != identity.bot_user_id {
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
    Ok(DiscordApp {
        http: http.clone(),
        application_id,
        guild_id: guild(identity)?,
    })
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
        .route("/v1/module", get(module_status))
        .route("/v1/settings", get(settings_status).post(apply_settings));
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
    if !token_matches(
        &state.installation.value.core.operator_token_hash,
        bearer(headers),
    ) {
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
    let Some(app) = state.app() else {
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

/// The provider's settings file, read and validated as at startup.
fn read_settings(state: &DiscordState) -> Result<DiscordConfig> {
    DiscordConfig::load(&state.installation, &state.health.id)
}

async fn settings_status(
    State(state): State<DiscordState>,
    headers: HeaderMap,
) -> Result<Json<config::Status>, InterfaceError> {
    authorize_operator(&state, &headers)?;
    let root = &state.installation.root;
    Ok(Json(state.settings.status(
        || read_settings(&state),
        |file| state.module.declared(file.module_source(root)) != Declared::Serving,
    )))
}

/// Serves the provider's settings file in place of the serving settings,
/// whole, when everything it changes applies live, the module it declares
/// included; otherwise the serving settings and module stay, and the answer
/// says why. The apply runs as its own task, so an operator who stops
/// waiting cannot cancel it between Discord's confirmation and activation.
async fn apply_settings(
    State(state): State<DiscordState>,
    headers: HeaderMap,
) -> Result<Response, InterfaceError> {
    authorize_operator(&state, &headers)?;
    let applying = state.clone();
    let applied = tokio::spawn(async move {
        let state = &applying;
        state
            .settings
            .apply(
                || read_settings(state),
                |file| {
                    let source = file.module_source(&state.installation.root);
                    state.module.apply(source, state.app())
                },
            )
            .await
    })
    .await;
    let refusal = match applied {
        Ok(Ok(applied)) => {
            if !applied.changed.is_empty() {
                info!(
                    changed = applied.changed.join(", "),
                    "Discord settings applied"
                );
            }
            return Ok(Json(applied).into_response());
        }
        Ok(Err(refusal)) => refusal,
        Err(failure) => {
            error!(error = %failure, "Discord settings apply failed");
            return Err(InterfaceError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!(
                    "applying the settings failed unexpectedly ({failure}); check \
                     `carillon discord settings` and apply again"
                ),
            });
        }
    };
    let (status, error, restart) = match refusal {
        config::Refusal::Invalid(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("the settings file is not valid: {error:#}; nothing was applied"),
            Vec::new(),
        ),
        config::Refusal::Restart(difference) => (
            StatusCode::CONFLICT,
            format!(
                "the settings file changes what only a restart applies ({}); nothing was applied",
                difference.restart.join(", ")
            ),
            difference.restart,
        ),
        config::Refusal::Module(failure) => {
            let (status, error) = module_refusal(failure);
            (status, error, Vec::new())
        }
    };
    Ok((
        status,
        Json(Refusal {
            error,
            serving: state.module.serving(),
            restart,
        }),
    )
        .into_response())
}

fn module_refusal(failure: ChangeError) -> (StatusCode, String) {
    error!(error = %failure, "Discord module change refused");
    match failure {
        ChangeError::Busy(installation) => (
            StatusCode::CONFLICT,
            format!(
                "another Carillon maintenance operation is running for {}; nothing was applied. \
                 Apply again once it finishes",
                installation.display()
            ),
        ),
        ChangeError::Lock(failure) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not take the maintenance lock: {failure:#}; nothing was applied"),
        ),
        ChangeError::Unusable(failure) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("not a usable Discord module: {failure:#}; nothing was applied"),
        ),
        ChangeError::Record(failure) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "could not clear the command registration record ({failure}), so Discord was \
                 not contacted and nothing was applied. Check the provider's state directory, \
                 then apply again"
            ),
        ),
        ChangeError::Register(failure) if discord_failure_is_terminal(&failure) => (
            StatusCode::BAD_GATEWAY,
            format!(
                "Discord rejected the module's commands: {failure:#}. Nothing was applied; \
                 correct the module's command definitions and apply again"
            ),
        ),
        ChangeError::Register(failure) => (
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "Discord did not confirm the module's commands: {failure:#}. It may have \
                 applied them; nothing else was applied. Apply again once Discord answers"
            ),
        ),
    }
}

async fn module_status(
    State(state): State<DiscordState>,
    headers: HeaderMap,
) -> Result<Json<Status>, InterfaceError> {
    authorize_operator(&state, &headers)?;
    let declared = match read_settings(&state) {
        Ok(file) => state
            .module
            .declared(file.module_source(&state.installation.root)),
        Err(error) => Declared::Unknown {
            error: format!("the settings file is not valid: {error:#}"),
        },
    };
    Ok(Json(state.module.status(state.app(), declared)))
}

async fn simulate(
    State(state): State<DiscordState>,
    headers: HeaderMap,
    Json(request): Json<IncomingMessage>,
) -> Result<Response, InterfaceError> {
    authorize_interface(&state, &headers)?;
    let settings = state.settings.current();
    let Some(plan) = admit_message(
        &settings,
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
    if settings.reactions {
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
    let native_ref = format!("local:{}", resolved.event.event_id);
    let delivery = settle_own_delivery(&state.core, &receipt, native_ref).await?;
    if delivery.is_some() && settings.reactions {
        feedback.push("delivered".to_owned());
    }
    let card = if delivery.is_some() && settings.activity {
        let jobs = state
            .core
            .named_jobs(std::slice::from_ref(&receipt.turn_id))
            .await?;
        match jobs.into_iter().next() {
            Some(job) => summary(&state, job).await,
            None => None,
        }
    } else {
        None
    };
    Ok(Json(SimulateResponse {
        receipt,
        delivery,
        card,
        conversation_id: resolved.conversation_id,
        feedback,
    })
    .into_response())
}

/// The summary card the monitor would keep of a settled job, rendered by the
/// module serving now; none when the job earned none or the module gave no
/// usable card, which is logged as the monitor logs it.
async fn summary(state: &DiscordState, job: Job) -> Option<Embed> {
    let request = CardRequest {
        job,
        seen: None,
        markers: state.settings.current().activity_markers.clone(),
    };
    state
        .module
        .render_card(&request)
        .await
        .unwrap_or_else(|error| {
            warn!(
                error = format!("{error:#}"),
                "the Discord module gave no usable card"
            );
            None
        })
}

/// Settles the simulated message's own turn's delivery and no other, as the
/// delivery loop does once Discord has the answer. A turn still queued or
/// running is waited for; a finished one, whose simulation may have stopped
/// waiting before its answer, is settled if still pending.
async fn settle_own_delivery(
    core: &CoreClient,
    receipt: &EventReceipt,
    native_ref: String,
) -> Result<Option<Delivery>, InterfaceError> {
    let turn = &receipt.turn_id;
    let delivery = if matches!(receipt.state, TurnState::Queued | TurnState::Running) {
        let delivery = timeout(Duration::from_secs(10), async {
            loop {
                if let Some(delivery) = core.claim_turn_delivery(turn).await? {
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
        core.claim_turn_delivery(turn).await?
    };
    if let Some(delivery) = &delivery {
        core.acknowledge_delivery(delivery.id, DeliveryOutcome::Delivered, Some(native_ref))
            .await?;
    }
    Ok(delivery)
}

/// Runs an interaction exactly as the gateway would, returning the reply the
/// user would see. Replies are checked as the gateway checks them, so a
/// module producing an unusable reply fails here, not in production.
async fn simulate_interaction(
    State(state): State<DiscordState>,
    headers: HeaderMap,
    Json(source): Json<InteractionSource>,
) -> Result<Json<Reply>, InterfaceError> {
    authorize_interface(&state, &headers)?;
    let settings = state.settings.current();
    let admitted = admit_interaction(&settings, source)?;
    let reply = run_interaction(&state, admitted, settings)
        .await
        .map_err(|error| InterfaceError {
            status: StatusCode::BAD_GATEWAY,
            message: error.to_string(),
        })?;
    answer(&reply).map_err(|error| InterfaceError {
        status: StatusCode::BAD_GATEWAY,
        message: error.to_string(),
    })?;
    Ok(Json(reply))
}

async fn gateway_loop(
    state: &DiscordState,
    gateway: &mut Gateway,
    messages: &mpsc::Sender<Queued>,
    ingress: &Arc<Ingress>,
    interactions: &mut JoinSet<()>,
) -> Result<()> {
    while let Some(event) = gateway.next_event().await {
        let (event, dispatch) = event.context("receive Discord gateway event")?;
        if let Some(message) = take_event(state, event, dispatch, interactions) {
            // Unsettled before the handoff, which fails once the lanes have
            // stopped: a message they never took keeps the floor below it.
            gateway.taken(dispatch);
            queue_or_refuse(messages, ingress, message)?;
        }
    }
    bail!("Discord gateway event stream ended")
}

/// One gateway event: an interaction becomes a task in `interactions`, whose
/// finished tasks are reaped here, and an admitted message is returned for
/// the lanes, where it stays unsettled until its preparation settles it.
fn take_event(
    state: &DiscordState,
    event: Event,
    dispatch: Option<Dispatch>,
    interactions: &mut JoinSet<()>,
) -> Option<QueuedMessage> {
    while let Some(finished) = interactions.try_join_next() {
        reap(finished);
    }
    match event {
        Event::MessageCreate(message) => {
            if message.author.bot || message.kind == MessageType::ThreadStarterMessage {
                return None;
            }
            let guild = message.guild_id.map(|id| id.to_string());
            // The message keeps the settings it is admitted with.
            let settings = state.settings.current();
            if admit_identity(&settings, guild.as_deref(), &message.author.id.to_string()).is_err()
            {
                return None;
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
            Some(QueuedMessage {
                request,
                native: native_message(&message),
                settings,
                dispatch,
            })
        }
        Event::InteractionCreate(interaction) => {
            // Interactions are independent of message order and of each
            // other; each answers within Discord's own deadline. One taken
            // here is answered before the provider stops, and Discord takes
            // its acknowledgement only once, so a replay never runs it again.
            interactions.spawn(handle_interaction(state.clone(), interaction.0));
            None
        }
        _ => None,
    }
}

async fn answered(interactions: &mut JoinSet<()>) {
    while let Some(finished) = interactions.join_next().await {
        reap(finished);
    }
}

fn reap(finished: Result<(), JoinError>) {
    if let Err(error) = finished {
        error!(%error, "a Discord interaction task failed");
    }
}

/// The message as Discord sent it, for the runtime's native context, without
/// the signed addresses of its attachments: those expire, and accepted
/// files are core's captures, not CDN links.
fn native_message(message: &twilight_model::channel::Message) -> Value {
    let mut native = serde_json::to_value(message).unwrap_or(Value::Null);
    crate::operations::strip_attachment_addresses(&mut native);
    native
}

/// Hands a gateway message to the lanes. One that finds their queue full is
/// refused as overload, as one beyond a lane's bound is: only a message
/// admission would activate is answered, within the shared refusal bound.
pub(crate) fn queue_or_refuse(
    messages: &mpsc::Sender<Queued>,
    ingress: &Arc<Ingress>,
    inbound: QueuedMessage,
) -> Result<()> {
    if let Err(refused) = queue_message(messages, inbound)? {
        lanes::refuse_overloaded(ingress, refused);
    }
    Ok(())
}

/// Hands a message to the lanes without waiting for room. The gateway must
/// keep reading so heartbeats and interactions go on while the lanes are
/// busy; a message that finds the queue full is returned refused.
fn queue_message(
    messages: &mpsc::Sender<Queued>,
    inbound: QueuedMessage,
) -> Result<Result<(), QueuedMessage>> {
    match messages.try_reserve() {
        Ok(room) => {
            room.send(inbound.into());
            Ok(Ok(()))
        }
        Err(mpsc::error::TrySendError::Full(())) => Ok(Err(inbound)),
        Err(mpsc::error::TrySendError::Closed(())) => bail!("Discord message worker stopped"),
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
