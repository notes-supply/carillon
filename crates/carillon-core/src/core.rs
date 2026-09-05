//! The core service: admission, the turn lifecycle worker, and the delivery
//! outbox, served over the loopback provider API.

#[cfg(test)]
mod tests;

use std::{path::PathBuf, time::Duration};

use anyhow::{Result, anyhow};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::{
    config::{LoadedConfig, Provider, ProviderRole, RELEASE_VERSION, token_matches},
    db::{ClaimedTurn, Database},
    model::{
        Delivery, DeliveryAck, DeliveryReconciliation, EventReceipt, ExternalEvent, Health,
        INSTANCE_HEADER, PROVIDER_HEADER, RuntimeTurnRequest, RuntimeTurnResponse,
        SelectedProvider, StatusReport, TurnInspection, TurnSummary,
    },
    resources,
};

#[derive(Clone)]
struct AppState {
    config: LoadedConfig,
    database: Database,
    client: Client,
}

impl AppState {
    fn snapshot_store(&self) -> PathBuf {
        self.config.value.data_dir.join("resource-snapshots")
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

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

pub async fn serve(config: LoadedConfig) -> Result<()> {
    // A failed bind must not recover state or start accepted work.
    let listener = TcpListener::bind(config.value.core.listen).await?;
    let database = Database::open(&config.value.data_dir.join("carillon.sqlite3"))?;
    let recovered_turns = database.recover_running_turns()?;
    // Interface processes may outlive core. Their claims remain recoverable;
    // interface-instance reconciliation handles abandoned claims instead.
    if recovered_turns > 0 {
        warn!(
            recovered_turns,
            "running turns became uncertain after restart"
        );
    }

    let state = AppState {
        config: config.clone(),
        database,
        client: Client::new(),
    };
    let mut workers = tokio::task::JoinSet::new();
    for runtime in config
        .value
        .providers
        .iter()
        .filter(|provider| provider.role == ProviderRole::Runtime && provider.enabled)
    {
        let runtime_id = runtime.id.clone();
        let worker = state.clone();
        workers.spawn(async move { runtime_worker(worker, runtime_id).await });
    }

    let router = Router::new()
        .route("/health", get(health))
        .route("/v1/events", post(accept_event))
        .route("/v1/deliveries", get(claim_delivery))
        .route("/v1/deliveries/reconcile", post(reconcile_delivery_claims))
        .route("/v1/deliveries/{id}/ack", post(acknowledge_delivery))
        .route("/v1/turns", get(recent_turns))
        .route("/v1/turns/{id}", get(inspect_turn))
        .route("/v1/status", get(status))
        .with_state(state);
    info!(listen = %config.value.core.listen, "Carillon core ready");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await?;
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
    let interface = interface_provider(&headers, &state.config)?.clone();
    admit(&event, &state.config)?;
    if let Some(receipt) = state
        .database
        .event_receipt(&interface.id, &event.event_id)?
    {
        return Ok(Json(receipt));
    }
    let owner = state.database.conversation_runtime(&interface.id, &event)?;
    let runtime_id = owner
        .as_deref()
        .unwrap_or(&state.config.value.agent.runtime_provider);
    let runtime = state
        .config
        .provider(runtime_id, ProviderRole::Runtime)?
        .clone();
    if !runtime.enabled {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "conversation runtime provider is disabled",
        ));
    }
    let resources = resources::materialize(
        &state.config.value.agent.resource_root,
        &state.snapshot_store(),
    )?;
    let providers =
        serde_json::to_value([selected_provider(&interface), selected_provider(&runtime)])
            .map_err(anyhow::Error::from)?;
    let receipt =
        state
            .database
            .accept_event(&interface.id, &runtime.id, &event, &providers, &resources)?;
    Ok(Json(receipt))
}

#[derive(Debug, PartialEq, Eq)]
enum WorkerStep {
    Idle,
    Advanced,
    Backoff,
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
    let token = state.config.credential(runtime_id)?;
    let Some(turn) = state.database.claim_turn(runtime_id)? else {
        return Ok(WorkerStep::Idle);
    };
    let outcome = async {
        let request = runtime_request(state, &turn).map_err(TurnFailure::Retry)?;
        let response = submit(runtime, &token, request).await?;
        state
            .database
            .complete_turn(&turn, &response)
            .map_err(TurnFailure::Uncertain)
    }
    .await;
    match outcome {
        Ok(()) => {}
        Err(TurnFailure::Retry(failure)) => {
            persist_outcome(&turn, "turn requeue", || state.database.requeue_turn(&turn)).await;
            warn!(turn_id = %turn.turn_id, error = %failure, "runtime was unavailable; turn requeued");
            return Ok(WorkerStep::Backoff);
        }
        Err(TurnFailure::Uncertain(failure)) => {
            let message = failure.to_string();
            persist_outcome(&turn, "uncertain turn record", || {
                state.database.mark_uncertain(&turn, &message)
            })
            .await;
            warn!(turn_id = %turn.turn_id, error = %message, "runtime outcome is uncertain");
        }
    }
    Ok(WorkerStep::Advanced)
}

/// Retries a local outcome write until it lands. Giving up would either lose
/// the uncertainty marker or rerun claimed work after a restart, so the
/// worker waits out transient storage failures instead. The operations behind
/// this are idempotent, so a commit whose acknowledgement was lost is safe.
async fn persist_outcome(turn: &ClaimedTurn, action: &str, mut write: impl FnMut() -> Result<()>) {
    while let Err(failure) = write() {
        warn!(
            turn_id = %turn.turn_id,
            action,
            error = %failure,
            "retrying turn outcome persistence"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// How a turn execution attempt failed.
#[derive(Debug)]
enum TurnFailure {
    /// The runtime provably never received the turn; running it again is safe.
    Retry(anyhow::Error),
    /// Model work may have run; only an operator may cause it to run again.
    Uncertain(anyhow::Error),
}

fn runtime_request(state: &AppState, turn: &ClaimedTurn) -> Result<RuntimeTurnRequest> {
    let continuation_ref = state.database.continuation(turn)?;
    Ok(RuntimeTurnRequest {
        correlation_id: turn.turn_id.clone(),
        interface_id: turn.interface_id.clone(),
        agent_id: turn.agent_id.clone(),
        audience_id: turn.audience_id.clone(),
        input: turn.input.clone(),
        continuation_ref,
        resources: resources::runtime_view(&turn.resources, &state.snapshot_store()),
    })
}

async fn call_runtime(
    client: &Client,
    runtime: &Provider,
    token: &str,
    request: RuntimeTurnRequest,
) -> Result<RuntimeTurnResponse, TurnFailure> {
    let response = client
        .post(format!("{}/v1/turns", runtime.url))
        .bearer_auth(token)
        .timeout(Duration::from_mins(30))
        .json(&request)
        .send()
        .await
        .map_err(|error| {
            // A connect failure means the request never reached the runtime.
            // Any later failure — timeout, broken transfer — may have left
            // model work running, so the outcome is ambiguous.
            let retry = error.is_connect();
            let failure = anyhow!(error).context("contact runtime provider");
            if retry {
                TurnFailure::Retry(failure)
            } else {
                TurnFailure::Uncertain(failure)
            }
        })?
        .error_for_status()
        .map_err(|error| {
            TurnFailure::Uncertain(anyhow!(error).context("runtime provider rejected turn"))
        })?
        .json::<RuntimeTurnResponse>()
        .await
        .map_err(|error| {
            TurnFailure::Uncertain(anyhow!(error).context("decode runtime response"))
        })?;
    if response.correlation_id != request.correlation_id {
        return Err(TurnFailure::Uncertain(anyhow!(
            "runtime returned a mismatched correlation ID"
        )));
    }
    if response.continuation_ref.is_empty() {
        return Err(TurnFailure::Uncertain(anyhow!(
            "runtime returned an empty continuation reference"
        )));
    }
    Ok(response)
}

async fn claim_delivery(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Option<Delivery>>, ApiError> {
    let interface = interface_provider(&headers, &state.config)?;
    let owner = interface_instance(&headers)?;
    Ok(Json(state.database.claim_delivery(&interface.id, owner)?))
}

async fn reconcile_delivery_claims(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DeliveryReconciliation>, ApiError> {
    let interface = interface_provider(&headers, &state.config)?;
    let owner = interface_instance(&headers)?;
    let feedback_target_ids = state
        .database
        .reconcile_delivery_claims(&interface.id, owner)?;
    Ok(Json(DeliveryReconciliation {
        feedback_target_ids,
    }))
}

async fn acknowledge_delivery(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<i64>,
    Json(ack): Json<DeliveryAck>,
) -> Result<StatusCode, ApiError> {
    let interface = interface_provider(&headers, &state.config)?;
    let owner = interface_instance(&headers)?;
    let recorded = state.database.acknowledge_delivery(
        &interface.id,
        id,
        owner,
        ack.outcome,
        ack.native_ref.as_deref(),
    )?;
    if !recorded {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("delivery {id} is not claimed by this interface instance"),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn inspect_turn(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<TurnInspection>, ApiError> {
    authorize(&headers, &state.config.value.core.operator_token_hash)?;
    state
        .database
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
    Ok(Json(state.database.recent_turns(
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
        release: state.config.value.release.clone(),
        agent_id: state.config.value.agent.id.clone(),
        providers,
        turns: state.database.turn_counts()?,
        deliveries: state.database.delivery_counts()?,
        recent_turns: state.database.recent_turns(None, 10)?,
    }))
}

async fn provider_health(client: &Client, config: &LoadedConfig) -> Vec<Health> {
    let mut reports = vec![core_health()];
    for provider in &config.value.providers {
        if !provider.enabled {
            reports.push(provider_status(provider, "disabled"));
            continue;
        }
        let token = config.credential(&provider.id).ok();
        reports.push(fetch_health(client, token.as_deref(), provider).await);
    }
    reports
}

async fn fetch_health(client: &Client, token: Option<&str>, provider: &Provider) -> Health {
    let mut request = client
        .get(format!("{}/health", provider.url))
        .timeout(Duration::from_secs(2));
    if let Some(value) = token {
        request = request.bearer_auth(value);
    }
    match request.send().await {
        Ok(response) => match response.error_for_status() {
            Ok(response) => response
                .json::<Health>()
                .await
                .unwrap_or_else(|_| provider_status(provider, "unavailable")),
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

fn admit(event: &ExternalEvent, config: &LoadedConfig) -> Result<(), ApiError> {
    if event.event_id.is_empty()
        || event.agent_id.is_empty()
        || event.user_id.is_empty()
        || event.audience_id.is_empty()
        || event.delivery_target_id.is_empty()
        || event.feedback_target_id.is_empty()
        || event.content.trim().is_empty()
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "event identity, agent, audience, targets, user, and content are required",
        ));
    }
    if event.agent_id != config.value.agent.id {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "unknown agent"));
    }
    Ok(())
}

fn selected_provider(provider: &Provider) -> SelectedProvider {
    SelectedProvider {
        id: provider.id.clone(),
        role: provider.role.as_str().to_owned(),
        version: provider.version.clone(),
        url: provider.url.clone(),
    }
}

async fn shutdown() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        error!(%error, "failed to install shutdown signal handler");
    }
}
