//! The core service: admission, the turn lifecycle worker, and the delivery
//! outbox, served over the loopback provider API.

#[cfg(test)]
mod tests;

#[cfg(test)]
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use carillon_installation::config::{
    LoadedConfig, Provider, ProviderRole, RELEASE_VERSION, token_matches,
};
use carillon_protocol::model::{
    ActiveTurn, Delivery, DeliveryAck, DeliveryReconciliation, EventReceipt, EventStatus,
    ExternalEvent, Health, INSTANCE_HEADER, ModelQuery, ModelUpdate, PROVIDER_HEADER,
    RuntimeModelQuery, RuntimeModelUpdate, RuntimeTurnRequest, RuntimeTurnResponse,
    SelectedProvider, StatusReport, TurnActivity, TurnInspection, TurnState, TurnSummary,
};
use reqwest::{Client, RequestBuilder, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};

use crate::{
    Core, CoreError, CoreOptions, RuntimeClient, RuntimeProvider, TurnFailure, WorkerStep,
};

#[derive(Clone)]
struct AppState {
    config: LoadedConfig,
    core: Core,
    client: Client,
}

#[cfg(test)]
impl AppState {
    fn snapshot_store(&self) -> PathBuf {
        self.core.snapshot_store()
    }
}

fn core_options(config: &LoadedConfig) -> CoreOptions {
    CoreOptions {
        data_directory: config.value.data_dir.clone(),
        agent_id: config.value.agent.id.clone(),
        resource_root: config.value.agent.resource_root.clone(),
        default_runtime: config.value.agent.runtime_provider.clone(),
        attachments: config.value.core.attachments,
        runtimes: config
            .value
            .providers
            .iter()
            .filter(|provider| provider.role == ProviderRole::Runtime)
            .map(|provider| RuntimeProvider {
                id: provider.id.clone(),
                version: provider.version.clone(),
                url: provider.url.clone(),
                enabled: provider.enabled,
            })
            .collect(),
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
pub fn check(config: &LoadedConfig) -> Result<()> {
    Core::open(core_options(config))?;
    Ok(())
}

pub async fn serve(config: LoadedConfig) -> Result<()> {
    // A failed bind must not recover state or start accepted work.
    let listener = TcpListener::bind(config.value.core.listen).await?;
    let core = Core::open(core_options(&config))?;
    let recovered_turns = core.recover_interrupted()?;
    // Interface processes may outlive core. Their claims remain recoverable;
    // interface-instance reconciliation handles abandoned claims instead.
    if recovered_turns > 0 {
        warn!(recovered_turns, "running turns were interrupted by restart");
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

    let state = AppState {
        config: config.clone(),
        core,
        client: Client::new(),
    };
    // A runtime configured for concurrent conversations gets that many
    // workers. Each claim is one transaction that skips conversations with an
    // earlier queued or running turn, so workers never share a conversation
    // and each conversation keeps its order.
    let mut workers = tokio::task::JoinSet::new();
    for runtime in config
        .value
        .providers
        .iter()
        .filter(|provider| provider.role == ProviderRole::Runtime && provider.enabled)
    {
        for _ in 0..runtime.parallel_turns() {
            let runtime_id = runtime.id.clone();
            let worker = state.clone();
            workers.spawn(async move { runtime_worker(worker, runtime_id).await });
        }
    }

    let router = Router::new()
        .route("/health", get(health))
        .route("/v1/events", post(accept_event))
        .route("/v1/events/{event_id}", get(event_status))
        .route("/v1/activity", get(active_turns))
        .route("/v1/deliveries", get(claim_delivery))
        .route("/v1/deliveries/reconcile", post(reconcile_delivery_claims))
        .route("/v1/deliveries/{id}/ack", post(acknowledge_delivery))
        .route("/v1/turns", get(recent_turns))
        .route("/v1/turns/{id}", get(inspect_turn))
        .route("/v1/model", get(model_state).put(set_model))
        .route("/v1/status", get(status))
        .with_state(state.clone());
    info!(listen = %config.value.core.listen, "Carillon core ready");
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown(state.core.clone()))
        .await;
    // The caller releases the service lock when this returns, however the
    // server ended; no copy may still be publishing then.
    state.core.stop_capturing();
    state.core.captures_settled().await;
    served?;
    Ok(())
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
    let interface = interface_provider(&headers, &state.config)?;
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
    let interface = interface_provider(&headers, &state.config)?;
    Ok(Json(state.core.event_status(&interface.id, &event_id)?))
}

/// Model calls answer a waiting user; they give up well before the
/// interface's own deadline does.
const MODEL_CALL_TIMEOUT: Duration = Duration::from_secs(5);

async fn model_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ModelQuery>,
) -> Result<Response, ApiError> {
    let interface = interface_provider(&headers, &state.config)?;
    let runtime = model_runtime(&state, &interface.id, &query)?;
    let token = runtime_token(&state.config, &runtime)?;
    let query = RuntimeModelQuery {
        interface_id: interface.id.clone(),
        query,
    };
    let mut url = Url::parse(&format!("{}/v1/model", runtime.url))
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
    let interface = interface_provider(&headers, &state.config)?;
    let runtime = model_runtime(
        &state,
        &interface.id,
        &ModelQuery {
            agent_id: update.agent_id.clone(),
            audience_id: update.audience_id.clone(),
        },
    )?;
    let token = runtime_token(&state.config, &runtime)?;
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

/// Model calls address the runtime that owns the conversation, so a
/// selection follows the conversation rather than the current default.
fn model_runtime(
    state: &AppState,
    interface_id: &str,
    query: &ModelQuery,
) -> Result<Provider, ApiError> {
    let runtime = state.core.runtime_for(interface_id, query)?;
    Ok(state
        .config
        .provider(&runtime.id, ProviderRole::Runtime)?
        .clone())
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
    let body = response.json::<Value>().await.map_err(|error| {
        warn!(%error, %status, "runtime model response was not JSON");
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "The runtime reported an unusable model state.",
        )
    })?;
    Ok((status, Json(body)).into_response())
}

/// A snapshot is cosmetic: a runtime that is slow to describe a turn costs
/// the snapshot, not a slow list.
const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(2);

/// The caller's queued and running turns, each running turn with its
/// runtime's latest snapshot when the runtime supplies one. The durable
/// states come from storage; `activity: null` never means the turn ended.
/// Runtimes are asked at once, so the list takes one snapshot timeout at
/// most however many runtimes cannot answer. A runtime runs at most its
/// configured number of conversations at once, so the requests are bounded
/// by the configured workers.
async fn active_turns(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ActiveTurn>>, ApiError> {
    let interface = interface_provider(&headers, &state.config)?;
    let listed = state.core.active_turns(&interface.id)?;
    let mut snapshots = tokio::task::JoinSet::new();
    for (index, (runtime_id, turn)) in listed.iter().enumerate() {
        if turn.state == TurnState::Running {
            let (state, runtime_id) = (state.clone(), runtime_id.clone());
            let (interface_id, turn_id) = (interface.id.clone(), turn.turn_id.clone());
            snapshots.spawn(async move {
                let activity = runtime_activity(&state, &runtime_id, &interface_id, &turn_id);
                (index, activity.await)
            });
        }
    }
    let mut turns = listed.into_iter().map(|(_, turn)| turn).collect::<Vec<_>>();
    while let Some(snapshot) = snapshots.join_next().await {
        if let Ok((index, activity)) = snapshot {
            turns[index].activity = activity;
        }
    }
    Ok(Json(turns))
}

async fn runtime_activity(
    state: &AppState,
    runtime_id: &str,
    interface_id: &str,
    turn_id: &str,
) -> Option<TurnActivity> {
    let snapshot = async {
        let runtime = state.config.provider(runtime_id, ProviderRole::Runtime)?;
        let token = runtime_token(&state.config, runtime)?;
        let mut url = Url::parse(&runtime.url)?;
        url.path_segments_mut()
            .map_err(|()| anyhow!("runtime provider URL cannot have a path"))?
            .extend(["v1", "turns", turn_id, "activity"]);
        url.query_pairs_mut()
            .append_pair("interface_id", interface_id);
        let response = state
            .client
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

async fn runtime_worker(state: AppState, runtime_id: String) {
    loop {
        let delay = match run_next_turn(&state, &runtime_id, async |runtime, token, request| {
            call_runtime(&state.client, runtime, token, request).await
        })
        .await
        {
            Ok(WorkerStep::Advanced) => continue,
            Ok(WorkerStep::Idle) => Duration::from_millis(100),
            Ok(WorkerStep::Backoff) => Duration::from_secs(1),
            Err(failure) => {
                warn!(runtime_id, error = %failure, "runtime worker failed");
                Duration::from_secs(1)
            }
        };
        tokio::time::sleep(delay).await;
    }
}

/// One worker iteration without idle or availability waits. Only the runtime
/// call is substitutable; claims and outcome writes use the real database.
async fn run_next_turn(
    state: &AppState,
    runtime_id: &str,
    submit: impl AsyncFnOnce(
        &Provider,
        &str,
        RuntimeTurnRequest,
    ) -> Result<RuntimeTurnResponse, TurnFailure>,
) -> Result<WorkerStep> {
    let runtime = state.config.provider(runtime_id, ProviderRole::Runtime)?;
    if !runtime.enabled {
        return Ok(WorkerStep::Backoff);
    }
    let token = runtime_token(&state.config, runtime)?;
    state
        .core
        .execute_next(runtime_id, async |request| {
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

async fn claim_delivery(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Option<Delivery>>, ApiError> {
    let interface = interface_provider(&headers, &state.config)?;
    let owner = interface_instance(&headers)?;
    Ok(Json(state.core.claim_delivery(&interface.id, owner)?))
}

async fn reconcile_delivery_claims(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DeliveryReconciliation>, ApiError> {
    let interface = interface_provider(&headers, &state.config)?;
    let owner = interface_instance(&headers)?;
    Ok(Json(state.core.reconcile_deliveries(&interface.id, owner)?))
}

async fn acknowledge_delivery(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<i64>,
    Json(ack): Json<DeliveryAck>,
) -> Result<StatusCode, ApiError> {
    let interface = interface_provider(&headers, &state.config)?;
    let owner = interface_instance(&headers)?;
    state
        .core
        .acknowledge_delivery(&interface.id, owner, id, &ack)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn inspect_turn(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<TurnInspection>, ApiError> {
    authorize(&headers, &state.config.value.core.operator_token_hash)?;
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
    authorize(&headers, &state.config.value.core.operator_token_hash)?;
    Ok(Json(state.core.recent_turns(
        query.event_id.as_deref(),
        query.limit.unwrap_or(20),
    )?))
}

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<StatusReport>, ApiError> {
    authorize(&headers, &state.config.value.core.operator_token_hash)?;
    let providers = provider_health(&state.client, &state.config).await;
    Ok(Json(StatusReport {
        release: RELEASE_VERSION.to_owned(),
        agent_id: state.config.value.agent.id.clone(),
        providers,
        turns: state.core.turn_counts()?,
        deliveries: state.core.delivery_counts()?,
        recent_turns: state.core.recent_turns(None, 10)?,
    }))
}

async fn provider_health(client: &Client, config: &LoadedConfig) -> Vec<Health> {
    let mut reports = vec![core_health()];
    for provider in &config.value.providers {
        if !provider.enabled {
            reports.push(provider_status(provider, "disabled"));
            continue;
        }
        reports.push(fetch_health(client, provider).await);
    }
    reports
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
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
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
async fn shutdown(core: Core) {
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
    // Waiting submissions then answer promptly; interfaces retry them.
    core.stop_capturing();
}
