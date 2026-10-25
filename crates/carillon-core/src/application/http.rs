//! The core service: admission, the turn lifecycle worker, and the delivery
//! outbox, served over the loopback provider API.

mod controls;
mod maintenance;
mod registry;
mod runtime;
#[cfg(test)]
mod tests;
mod workers;

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use carillon_installation::{
    config::{LoadedConfig, Provider, ProviderRole, RELEASE_VERSION, token_matches},
    loopback,
};
use carillon_protocol::model::{
    Delivery, DeliveryAck, DeliveryReconciliation, EventReceipt, EventStatus, ExternalEvent,
    Health, INSTANCE_HEADER, JobPage, ModelQuery, ModelUpdate, PROVIDER_HEADER, RuntimeModelQuery,
    RuntimeModelUpdate, RuntimeTurnRequest, RuntimeTurnResponse, SelectedProvider, StatusReport,
    TurnActivity, TurnInspection, TurnState, TurnSummary,
};
use reqwest::{Client, RequestBuilder, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    AgentOptions, Core, CoreError, CoreOptions, Lane, RuntimeClient, TurnFailure, WorkerStep,
};
pub(super) use maintenance::{Drain, Lease};
pub(super) use registry::{Applied, FileState, RegistryStatus, ServingRuntime};
use registry::{Refusal, Registry, runtime_facts};
use workers::{Census, Pause};

#[derive(Clone)]
struct AppState {
    /// The registry serving now, owned by the application.
    registry: Arc<Registry>,
    core: Core,
    client: Client,
    /// How the runtime workers stand, as their supervisor last arranged them.
    workers: Arc<watch::Sender<Census>>,
    /// The operator's pause of the workers for maintenance, if one was
    /// taken: written by its routes alone, acted on by the supervisor alone.
    pause: Arc<watch::Sender<Option<Pause>>>,
}

impl AppState {
    fn config(&self) -> Arc<LoadedConfig> {
        self.registry.current()
    }
}

fn core_options(config: &LoadedConfig) -> CoreOptions {
    CoreOptions {
        data_directory: config.value.data_dir.clone(),
        agents: config
            .value
            .agents
            .iter()
            .map(|agent| AgentOptions {
                id: agent.id.clone(),
                runtime: agent.runtime.clone(),
                resources: agent.resources.clone(),
            })
            .collect(),
        attachments: config.value.core.attachments,
        runtimes: runtime_facts(config),
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        error!(error = %error, "request failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

impl From<CoreError> for ApiError {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::InvalidInput(message) => Self::new(StatusCode::BAD_REQUEST, message),
            CoreError::Conflict(message) => Self::new(StatusCode::CONFLICT, message),
            CoreError::Unavailable(message) => Self::new(StatusCode::SERVICE_UNAVAILABLE, message),
            CoreError::InsufficientStorage(message) => {
                Self::new(StatusCode::INSUFFICIENT_STORAGE, message)
            }
            CoreError::Storage(error) => error.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

/// Open the installation's core exactly as [`serve`] does, without listening,
/// recovering turns or writing: the registry and the current core schema.
pub(super) fn check(config: &LoadedConfig) -> Result<()> {
    Core::open(core_options(config))?;
    Ok(())
}

/// Serves the installation whose registry `config` was loaded from `path`,
/// which [`Registry::apply`] reads again.
pub(super) async fn serve(path: PathBuf, config: LoadedConfig) -> Result<()> {
    // A failed bind must not recover state or start accepted work.
    let listener = TcpListener::bind(config.value.core.listen).await?;
    let core = Core::open(core_options(&config))?;
    let recovered_turns = core.recover_interrupted()?;
    // Interface processes may outlive core. Their claims remain recoverable;
    // interface-instance reconciliation handles abandoned claims instead.
    if recovered_turns > 0 {
        warn!(
            recovered_turns,
            "turns were interrupted by restart or agent changes"
        );
    }
    // Exclusive: the service lock is held and nothing is accepted yet.
    let reconciled = core.reconcile_attachments()?;
    if reconciled.partial + reconciled.unreferenced > 0 {
        warn!(
            partial = reconciled.partial,
            unreferenced = reconciled.unreferenced,
            "removed attachment captures no accepted turn references"
        );
    }

    let listen = config.value.core.listen;
    let state = AppState {
        registry: Arc::new(Registry::new(path, config)),
        core,
        client: loopback::client().build()?,
        workers: Arc::new(watch::Sender::new(Census::default())),
        pause: Arc::new(watch::Sender::new(None)),
    };
    // A runtime configured for concurrent conversations gets that many
    // message workers. Each claim is one transaction that skips
    // conversations with an earlier message queued or running, so workers
    // never share a conversation and each conversation keeps its order.
    // Observations wait on native work that may take hours; a fixed few run
    // per runtime beside its messages, never in their place. Their
    // supervisor follows the serving registry; none claims a turn once
    // `stopping` is cancelled, as shutdown begins.
    let stopping = CancellationToken::new();
    let supervisor = tokio::spawn(workers::supervise(
        state.clone(),
        state.workers.clone(),
        stopping.clone(),
    ));

    let router = router(state.clone());
    info!(%listen, "Carillon core ready");
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown(state.core.clone(), stopping.clone()))
        .await;
    // The caller releases the service lock when this returns, however the
    // server ended: no copy may still be publishing then, no worker may
    // still hold a turn, which settles rather than being dropped, and no
    // storage work may still be writing.
    stopping.cancel();
    state.core.stop_capturing();
    state.core.captures_settled().await;
    if let Err(error) = supervisor.await {
        error!(%error, "runtime worker supervisor ended abnormally");
    }
    state.core.storage_settled().await;
    served?;
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/events", post(accept_event))
        .route("/v1/events/{event_id}", get(event_status))
        .route("/v1/events/{event_id}/control", post(controls::control))
        .route(
            "/v1/runtime/turns",
            get(runtime::running).post(runtime::open),
        )
        .route("/v1/runtime/turns/{id}/outcome", post(runtime::outcome))
        .route("/v1/jobs", get(jobs))
        .route("/v1/deliveries", get(claim_delivery))
        .route("/v1/deliveries/reconcile", post(reconcile_delivery_claims))
        .route("/v1/deliveries/{id}/ack", post(acknowledge_delivery))
        .route("/v1/turns", get(recent_turns))
        .route("/v1/turns/{id}", get(inspect_turn))
        .route("/v1/model", get(model_state).put(set_model))
        .route("/v1/commands", get(commands))
        .route("/v1/status", get(status))
        .route("/v1/registry", get(registry_status).post(apply_registry))
        .route("/v1/pause", post(maintenance::begin))
        .route(
            "/v1/pause/{holder}",
            put(maintenance::renew).delete(maintenance::end),
        )
        .with_state(state)
}

async fn health() -> Json<Health> {
    Json(core_health())
}

fn core_health() -> Health {
    Health {
        id: "core".to_owned(),
        role: "core".to_owned(),
        version: RELEASE_VERSION.to_owned(),
        status: "ready".to_owned(),
    }
}

async fn accept_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(event): Json<ExternalEvent>,
) -> Result<Json<EventReceipt>, ApiError> {
    let config = state.config();
    let interface = interface_provider(&headers, &config)?;
    Ok(Json(
        state
            .core
            .accept(&selected_provider(interface), &event)
            .await?,
    ))
}

async fn event_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(event_id): AxumPath<String>,
) -> Result<Json<EventStatus>, ApiError> {
    let config = state.config();
    let interface = interface_provider(&headers, &config)?;
    Ok(Json(state.core.event_status(&interface.id, &event_id)?))
}

/// Model and command calls answer a waiting user; they give up well before
/// the interface's own deadline does.
const MODEL_CALL_TIMEOUT: Duration = Duration::from_secs(5);

async fn model_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ModelQuery>,
) -> Result<Response, ApiError> {
    relay_query(&state, &headers, "/v1/model", query).await
}

/// The runtime's own commands for the conversation, relayed like a model read.
async fn commands(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ModelQuery>,
) -> Result<Response, ApiError> {
    relay_query(&state, &headers, "/v1/commands", query).await
}

/// Relays a read about the agent or one conversation to the runtime that
/// owns the conversation, naming the calling interface.
async fn relay_query(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    query: ModelQuery,
) -> Result<Response, ApiError> {
    let config = state.config();
    let interface = interface_provider(headers, &config)?;
    let runtime = model_runtime(&state.core, &config, &interface.id, &query)?;
    let token = runtime_token(&config, runtime)?;
    let query = RuntimeModelQuery {
        interface_id: interface.id.clone(),
        query,
    };
    let mut url = Url::parse(&format!("{}{path}", runtime.url))
        .with_context(|| format!("runtime provider {} has an invalid URL", runtime.id))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("interface_id", &query.interface_id);
        pairs.append_pair("agent_id", &query.query.agent_id);
        if let Some(audience_id) = &query.query.audience_id {
            pairs.append_pair("audience_id", audience_id);
        }
    }
    relay_model_call(state.client.get(url).bearer_auth(token)).await
}

async fn set_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(update): Json<ModelUpdate>,
) -> Result<Response, ApiError> {
    let config = state.config();
    let interface = interface_provider(&headers, &config)?;
    let runtime = model_runtime(
        &state.core,
        &config,
        &interface.id,
        &ModelQuery {
            agent_id: update.agent_id.clone(),
            audience_id: update.audience_id.clone(),
        },
    )?;
    let token = runtime_token(&config, runtime)?;
    let update = RuntimeModelUpdate {
        interface_id: interface.id.clone(),
        update,
    };
    relay_model_call(
        state
            .client
            .put(format!("{}/v1/model", runtime.url))
            .bearer_auth(token)
            .json(&update),
    )
    .await
}

/// Refuses stranded conversations before reading a runtime credential or
/// issuing a model or command request.
fn model_runtime<'a>(
    core: &Core,
    config: &'a LoadedConfig,
    interface_id: &str,
    query: &ModelQuery,
) -> Result<&'a Provider, ApiError> {
    let runtime = core.query_runtime(interface_id, query)?;
    Ok(config.provider(&runtime.id, ProviderRole::Runtime)?)
}

/// Forwards a model call and relays the runtime's answer. Runtimes phrase
/// their rejections for people, so status and body pass through unchanged;
/// only transport failures are described here.
async fn relay_model_call(request: RequestBuilder) -> Result<Response, ApiError> {
    let response = request
        .timeout(MODEL_CALL_TIMEOUT)
        .send()
        .await
        .map_err(|error| {
            warn!(%error, "model call failed to reach the runtime");
            if error.is_timeout() {
                ApiError::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "The runtime did not answer in time.",
                )
            } else {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The runtime is not reachable right now.",
                )
            }
        })?;
    let status = response.status();
    let unusable = |error: &dyn std::fmt::Display| {
        warn!(%error, %status, "runtime model, command or control response was not JSON");
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "The runtime gave an unusable answer.",
        )
    };
    let body = response.bytes().await.map_err(|error| unusable(&error))?;
    if body.is_empty() {
        return Ok(status.into_response());
    }
    let body = serde_json::from_slice::<Value>(&body).map_err(|error| unusable(&error))?;
    Ok((status, Json(body)).into_response())
}

/// A snapshot is cosmetic: a runtime that is slow to describe a turn costs
/// the snapshot, not a slow list.
const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(2);

/// Which jobs a read asks for: those `tracked`, by origin turn IDs separated
/// by commas, or else a page of those at work, `after` the previous page.
#[derive(Deserialize)]
struct JobQuery {
    tracked: Option<String>,
    after: Option<String>,
}

/// The caller's jobs, as core recorded them: a page of those at work, or
/// those it names, each running turn with its runtime's latest snapshot when
/// the runtime supplies one. The durable states come from storage; a running
/// turn without `activity` has not ended. Runtimes are asked at once, so the
/// read takes one snapshot timeout at most however many runtimes cannot
/// answer, and a page holds at most
/// [`JOB_READ_LIMIT`](carillon_protocol::model::JOB_READ_LIMIT) running turns.
async fn jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<JobQuery>,
) -> Result<Json<JobPage>, ApiError> {
    let config = state.config();
    let interface = interface_provider(&headers, &config)?;
    let (listed, next) = match (query.tracked, query.after) {
        (Some(tracked), None) => {
            let mut named = Vec::new();
            for name in tracked.split(',') {
                if name.is_empty() {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "a tracked job is named by its turn ID",
                    ));
                }
                if !named.iter().any(|known| known == name) {
                    named.push(name.to_owned());
                }
            }
            (state.core.named_jobs(&interface.id, &named)?, None)
        }
        (None, after) => {
            let page = state.core.jobs_at_work(&interface.id, after.as_deref())?;
            (page.jobs, page.next)
        }
        (Some(_), Some(_)) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "a job read names jobs or pages through those at work, not both",
            ));
        }
    };
    let mut snapshots = tokio::task::JoinSet::new();
    for (index, (runtime_id, job)) in listed.iter().enumerate() {
        let Some(turn) = job
            .turns
            .last()
            .filter(|turn| turn.state == TurnState::Running)
        else {
            continue;
        };
        let (client, config, runtime_id) =
            (state.client.clone(), config.clone(), runtime_id.clone());
        let (interface_id, turn_id) = (interface.id.clone(), turn.turn_id.clone());
        snapshots.spawn(async move {
            let activity = runtime_activity(&client, &config, &runtime_id, &interface_id, &turn_id);
            (index, activity.await)
        });
    }
    let mut jobs = listed.into_iter().map(|(_, job)| job).collect::<Vec<_>>();
    while let Some(snapshot) = snapshots.join_next().await {
        if let Ok((index, activity)) = snapshot
            && let Some(turn) = jobs[index].turns.last_mut()
        {
            turn.activity = activity;
        }
    }
    Ok(Json(JobPage { jobs, next }))
}

async fn runtime_activity(
    client: &Client,
    config: &LoadedConfig,
    runtime_id: &str,
    interface_id: &str,
    turn_id: &str,
) -> Option<TurnActivity> {
    let snapshot = async {
        let runtime = config.provider(runtime_id, ProviderRole::Runtime)?;
        let token = runtime_token(config, runtime)?;
        let mut url = Url::parse(&runtime.url)?;
        url.path_segments_mut()
            .map_err(|()| anyhow!("runtime provider URL cannot have a path"))?
            .extend(["v1", "turns", turn_id, "activity"]);
        url.query_pairs_mut()
            .append_pair("interface_id", interface_id);
        let response = client
            .get(url)
            .bearer_auth(token)
            .timeout(ACTIVITY_TIMEOUT)
            .send()
            .await?;
        // Not yet or no longer executing in the runtime.
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok::<_, anyhow::Error>(Some(
            response.error_for_status()?.json::<TurnActivity>().await?,
        ))
    };
    // Interfaces ask every few seconds; an unavailable snapshot is routine.
    snapshot.await.unwrap_or_else(|error| {
        debug!(runtime_id, turn_id, %error, "runtime activity is unavailable");
        None
    })
}

/// Runs a runtime's turns of one lane until `stopping` is cancelled. An idle
/// worker waits for core to signal new work instead of polling. A message
/// already claimed runs to its outcome; none is claimed after the
/// cancellation. An observation only reads, and may wait hours on native
/// work: once `set_aside` is cancelled, for maintenance or shutdown, core
/// stops waiting for its answer and asks again later. A durable runtime's
/// one worker is its dispatcher instead.
async fn runtime_worker(
    state: AppState,
    runtime_id: String,
    lane: Lane,
    stopping: CancellationToken,
    set_aside: CancellationToken,
) {
    // Restart-only, so as it was when the worker started.
    if state
        .config()
        .provider(&runtime_id, ProviderRole::Runtime)
        .is_ok_and(|runtime| runtime.durable)
    {
        return runtime::dispatcher(state, runtime_id, stopping).await;
    }
    // A runtime that gave an observation no answer is asked again after a
    // pause, not in a tight loop.
    let backoff = match lane {
        Lane::Messages => Duration::from_secs(1),
        Lane::Observations => Duration::from_secs(5),
    };
    let mut work = state.core.work(lane);
    while !stopping.is_cancelled() {
        work.mark_unchanged();
        match run_next(
            &state,
            &runtime_id,
            lane,
            &stopping,
            async |runtime, token, request| {
                let observation = request.follow_up.is_some();
                let call = call_runtime(&state.client, runtime, token, request);
                if !observation {
                    return call.await;
                }
                set_aside
                    .run_until_cancelled(call)
                    .await
                    .unwrap_or_else(|| {
                        Err(TurnFailure::Unanswered(anyhow!(
                            "observation set aside: core paused its workers or is stopping"
                        )))
                    })
            },
        )
        .await
        {
            Ok(WorkerStep::Advanced) => {}
            Ok(WorkerStep::Idle) => {
                // The sender lives as long as `state`, which this task holds.
                let _ = stopping.run_until_cancelled(work.changed()).await;
            }
            Ok(WorkerStep::Backoff) => {
                stopping
                    .run_until_cancelled(tokio::time::sleep(backoff))
                    .await;
            }
            Err(failure) => {
                warn!(runtime_id, ?lane, error = %failure, "runtime worker failed");
                stopping
                    .run_until_cancelled(tokio::time::sleep(backoff))
                    .await;
            }
        }
    }
}

/// One worker iteration without idle or availability waits. Only the runtime
/// call is substitutable; claims and outcome writes use the real database.
async fn run_next(
    state: &AppState,
    runtime_id: &str,
    lane: Lane,
    stopping: &CancellationToken,
    submit: impl AsyncFnOnce(
        &Provider,
        &str,
        RuntimeTurnRequest,
    ) -> Result<RuntimeTurnResponse, TurnFailure>,
) -> Result<WorkerStep> {
    let config = state.config();
    let runtime = config.provider(runtime_id, ProviderRole::Runtime)?;
    if !runtime.enabled {
        return Ok(WorkerStep::Backoff);
    }
    let token = runtime_token(&config, runtime)?;
    state
        .core
        .run_next(runtime_id, lane, stopping, async |request| {
            submit(runtime, &token, request).await
        })
        .await
}

/// The credential core presents to a runtime, checked against the registry
/// so a stale token file fails here rather than as a mysterious rejection.
fn runtime_token(config: &LoadedConfig, runtime: &Provider) -> Result<String> {
    let token = config.credential(&runtime.id)?;
    if !token_matches(&runtime.token_hash, &token) {
        return Err(anyhow!(
            "runtime provider {} credential does not match configuration",
            runtime.id
        ));
    }
    Ok(token)
}

async fn call_runtime(
    client: &Client,
    runtime: &Provider,
    token: &str,
    request: RuntimeTurnRequest,
) -> Result<RuntimeTurnResponse, TurnFailure> {
    RuntimeClient::new(client.clone(), runtime.url.clone(), token.to_owned())
        .execute(request)
        .await
}

#[derive(Deserialize)]
struct ClaimQuery {
    turn_id: Option<String>,
}

async fn claim_delivery(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ClaimQuery>,
) -> Result<Json<Option<Delivery>>, ApiError> {
    let interface = interface_provider(&headers, &state.config())?.id.clone();
    let owner = interface_instance(&headers)?.to_owned();
    let claimed = state
        .core
        .blocking(move |core| match query.turn_id {
            Some(turn) => core.claim_turn_delivery(&interface, &owner, &turn),
            None => core.claim_delivery(&interface, &owner),
        })
        .await?;
    Ok(Json(claimed))
}

async fn reconcile_delivery_claims(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DeliveryReconciliation>, ApiError> {
    let interface = interface_provider(&headers, &state.config())?.id.clone();
    let owner = interface_instance(&headers)?.to_owned();
    let reconciled = state
        .core
        .blocking(move |core| core.reconcile_deliveries(&interface, &owner))
        .await?;
    Ok(Json(reconciled))
}

async fn acknowledge_delivery(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<i64>,
    Json(ack): Json<DeliveryAck>,
) -> Result<StatusCode, ApiError> {
    let interface = interface_provider(&headers, &state.config())?.id.clone();
    let owner = interface_instance(&headers)?.to_owned();
    state
        .core
        .blocking(move |core| core.acknowledge_delivery(&interface, &owner, id, &ack))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn inspect_turn(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<TurnInspection>, ApiError> {
    authorize(&headers, &state.config().value.core.operator_token_hash)?;
    state
        .core
        .inspect_turn(&id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("turn {id} not found")))
}

#[derive(Deserialize)]
struct RecentQuery {
    event_id: Option<String>,
    limit: Option<u32>,
}

async fn recent_turns(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RecentQuery>,
) -> Result<Json<Vec<TurnSummary>>, ApiError> {
    authorize(&headers, &state.config().value.core.operator_token_hash)?;
    Ok(Json(state.core.recent_turns(
        query.event_id.as_deref(),
        query.limit.unwrap_or(20),
    )?))
}

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<StatusReport>, ApiError> {
    let config = state.config();
    authorize(&headers, &config.value.core.operator_token_hash)?;
    let providers = provider_health(&state.client, &config).await;
    Ok(Json(StatusReport {
        release: RELEASE_VERSION.to_owned(),
        agents: config
            .value
            .agents
            .iter()
            .map(|agent| agent.id.clone())
            .collect(),
        providers,
        turns: state.core.turn_counts()?,
        deliveries: state.core.delivery_counts()?,
        recent_turns: state.core.recent_turns(None, 10)?,
    }))
}

/// The registry serving, its runtimes' workers, and how the file differs.
async fn registry_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<RegistryStatus>, ApiError> {
    let config = state.config();
    authorize(&headers, &config.value.core.operator_token_hash)?;
    Ok(Json(RegistryStatus::of(
        &config,
        &state.workers.borrow(),
        state.registry.compare(&config),
    )))
}

/// How long an apply waits for the workers to be arranged for it before
/// answering with how they stand.
const ARRANGEMENT_WAIT: Duration = Duration::from_secs(5);

/// Serves the registry file when it differs from what serves only in what
/// applies live; otherwise names why it does not. The runtime programs are
/// not started or stopped: this changes which runtimes core admits work
/// for and runs it by, and how many of their conversations at once.
async fn apply_registry(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Applied>, ApiError> {
    authorize(&headers, &state.config().value.core.operator_token_hash)?;
    let changed = match state.registry.apply(&state.core).await {
        Ok(changed) => changed,
        Err(Refusal::Invalid(error)) => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("the registry file is not valid: {error:#}; nothing was applied"),
            ));
        }
        Err(Refusal::Restart(difference)) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                format!(
                    "the registry file changes what only a restart applies ({}); nothing was \
                     applied",
                    difference.restart.join(", ")
                ),
            ));
        }
        Err(Refusal::Facts(error)) => return Err(error.into()),
    };
    let config = state.config();
    let mut census = state.workers.subscribe();
    let arranged = tokio::time::timeout(
        ARRANGEMENT_WAIT,
        census.wait_for(|census| {
            census
                .registry
                .as_ref()
                .is_some_and(|arranged| Arc::ptr_eq(arranged, &config))
        }),
    )
    .await;
    if !matches!(arranged, Ok(Ok(_))) {
        warn!("runtime workers were not arranged for the applied registry in time");
    }
    let census = state.workers.borrow();
    Ok(Json(Applied {
        changed,
        runtimes: ServingRuntime::all(&config, &census),
        paused: census.paused.is_some(),
    }))
}

/// Core's health, then each provider's in registry order. Enabled providers
/// are asked at once, so the report takes one health timeout at most however
/// many cannot answer.
async fn provider_health(client: &Client, config: &LoadedConfig) -> Vec<Health> {
    let providers = &config.value.providers;
    let mut probes = tokio::task::JoinSet::new();
    let mut reports = Vec::with_capacity(providers.len());
    for (index, provider) in providers.iter().enumerate() {
        let status = if !provider.enabled {
            "disabled"
        } else if provider.role == ProviderRole::Runtime
            && !config
                .value
                .agents
                .iter()
                .any(|agent| agent.runtime == provider.id)
        {
            "idle"
        } else {
            let (client, provider) = (client.clone(), provider.clone());
            probes.spawn(async move { (index, fetch_health(&client, &provider).await) });
            "unavailable"
        };
        reports.push(provider_status(provider, status));
    }
    while let Some(probe) = probes.join_next().await {
        if let Ok((index, health)) = probe {
            reports[index] = health;
        }
    }
    std::iter::once(core_health()).chain(reports).collect()
}

async fn fetch_health(client: &Client, provider: &Provider) -> Health {
    // Public diagnostics must not disclose reusable provider credentials.
    let request = client
        .get(format!("{}/health", provider.url))
        .timeout(Duration::from_secs(2));
    match request.send().await {
        Ok(response) => match response.error_for_status() {
            Ok(response) => match response.json::<Health>().await {
                Ok(mut health)
                    if health.id == provider.id && health.role == provider.role.as_str() =>
                {
                    if health.version != provider.version {
                        "version_mismatch".clone_into(&mut health.status);
                    }
                    health
                }
                _ => provider_status(provider, "unavailable"),
            },
            Err(_) => provider_status(provider, "unavailable"),
        },
        Err(_) => provider_status(provider, "unavailable"),
    }
}

fn provider_status(provider: &Provider, status: &str) -> Health {
    Health {
        id: provider.id.clone(),
        role: provider.role.as_str().to_owned(),
        version: provider.version.clone(),
        status: status.to_owned(),
    }
}

fn interface_provider<'a>(
    headers: &HeaderMap,
    config: &'a LoadedConfig,
) -> Result<&'a Provider, ApiError> {
    let id = provider_identity(headers)?;
    let provider = config
        .provider(id, ProviderRole::Interface)
        .map_err(|_| ApiError::forbidden("unknown interface provider"))?;
    if !provider.enabled {
        return Err(ApiError::forbidden("interface provider is disabled"));
    }
    authorize(headers, &provider.token_hash)?;
    Ok(provider)
}

fn authorize(headers: &HeaderMap, expected_hash: &str) -> Result<(), ApiError> {
    let candidate =
        bearer(headers).ok_or_else(|| ApiError::forbidden("component authentication required"))?;
    if !token_matches(expected_hash, candidate) {
        return Err(ApiError::forbidden("component authentication failed"));
    }
    Ok(())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())?;
    value.strip_prefix("Bearer ")
}

fn provider_identity(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(PROVIDER_HEADER)
        .and_then(|header| header.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::forbidden("provider identity required"))
}

fn interface_instance(headers: &HeaderMap) -> Result<&str, ApiError> {
    let value = headers
        .get(INSTANCE_HEADER)
        .and_then(|header| header.to_str().ok())
        .ok_or_else(|| ApiError::forbidden("interface instance identity required"))?;
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ApiError::forbidden("invalid interface instance identity"));
    }
    Ok(value)
}

fn selected_provider(provider: &Provider) -> SelectedProvider {
    SelectedProvider {
        id: provider.id.clone(),
        role: provider.role.as_str().to_owned(),
        version: provider.version.clone(),
        url: provider.url.clone(),
    }
}

/// Interrupt or termination, as the service manager stops core.
async fn shutdown(core: Core, stopping: CancellationToken) {
    let terminated = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(error) => {
                error!(%error, "failed to install termination signal handler");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        interrupted = tokio::signal::ctrl_c() => {
            if let Err(error) = interrupted {
                error!(%error, "failed to install shutdown signal handler");
            }
        }
        () = terminated => {}
    }
    // No turn is claimed after this; waiting submissions answer promptly,
    // and interfaces retry them.
    stopping.cancel();
    core.stop_capturing();
}
