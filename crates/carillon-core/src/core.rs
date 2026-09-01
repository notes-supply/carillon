use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{net::TcpListener, sync::Mutex};
use tracing::{error, info, warn};

use crate::{
    config::{
        LoadedConfig, Provider, ProviderRole, RELEASE_VERSION, token_matches,
        validate_provider_identity,
    },
    db::{AcceptedTurn, Continuation, Database},
    model::{
        CapabilitySelection, Delivery, DeliveryAck, DeliveryOutcome, DeliveryReconciliation,
        EventReceipt, ExecutionProfileApplication, ExecutionProfileScope,
        ExecutionProfileSelection, ExtensionRequest, ExtensionResponse, ExternalEvent, Health,
        ResourceSnapshot, RuntimeEvidence, RuntimeExecutionProfileSelection,
        RuntimeExecutionProfiles, RuntimeTurnRequest, RuntimeTurnResponse, SelectedProvider,
        StatusReport, TurnInspection, TurnSummary,
    },
};

#[derive(Clone)]
struct AppState {
    config_path: PathBuf,
    database: Database,
    client: Client,
    turns: Arc<Mutex<()>>,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        error!(error = %error, "request failed");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

pub async fn serve(config: LoadedConfig) -> Result<()> {
    let database = Database::open(&config.value.data_dir.join("carillon.sqlite3"))?;
    let state = AppState {
        config_path: config.path.clone(),
        database,
        client: Client::new(),
        turns: Arc::new(Mutex::new(())),
    };
    let reconciliation = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(failure) = reconcile_pending_turns(&reconciliation).await {
                warn!(error = %failure, "runtime reconciliation retry failed");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    let router = Router::new()
        .route("/health", get(health))
        .route("/v1/events", post(accept_event))
        .route("/v1/deliveries", get(claim_delivery))
        .route("/v1/deliveries/reconcile", post(reconcile_delivery_claims))
        .route("/v1/deliveries/{id}/ack", post(acknowledge_delivery))
        .route("/v1/turns", get(recent_turns))
        .route("/v1/turns/{id}", get(inspect_turn))
        .route("/v1/status", get(status))
        .route("/v1/execution-profiles", get(execution_profiles))
        .route(
            "/v1/execution-profiles/refresh",
            post(refresh_execution_profiles),
        )
        .route("/v1/execution-profile", put(apply_execution_profile))
        .with_state(state);
    let listener = TcpListener::bind(config.value.core.listen).await?;
    info!(listen = %config.value.core.listen, "Carillon core ready");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn health() -> Json<Health> {
    Json(Health {
        id: "core".to_owned(),
        role: "core".to_owned(),
        version: RELEASE_VERSION.to_owned(),
        status: "ready".to_owned(),
    })
}

async fn accept_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(event): Json<ExternalEvent>,
) -> Result<Json<EventReceipt>, ApiError> {
    let config = LoadedConfig::load(&state.config_path)?;
    let interface_id = authorize_interface(&headers, &config)?;
    admit(&event)?;
    let _guard = state.turns.lock().await;
    Ok(Json(
        process_event(&state, &config, &interface_id, event).await?,
    ))
}

async fn process_event(
    state: &AppState,
    config: &LoadedConfig,
    interface_id: &str,
    event: ExternalEvent,
) -> Result<EventReceipt> {
    if event.agent_id != config.value.agent.id {
        bail!("agent {} is not configured", event.agent_id);
    }
    let interface = config.provider(interface_id, ProviderRole::Interface)?;
    let continuation =
        state
            .database
            .continuation(interface_id, &event.agent_id, &event.audience_id)?;
    let runtime_id = continuation.as_ref().map_or(
        config.value.agent.runtime_provider.as_str(),
        |continuation| continuation.runtime.as_str(),
    );
    let runtime = config.provider(runtime_id, ProviderRole::Runtime)?;
    let resources = materialize_resources(
        &config.value.agent.resource_root,
        &config.value.data_dir.join("resource-snapshots"),
    )?;
    let providers = provider_snapshot(config, interface, runtime)?;
    let accepted = state.database.accept_event(
        interface_id,
        &event.agent_id,
        runtime_id,
        &event,
        &providers,
        &resources,
    )?;
    if accepted.duplicate {
        return Ok(receipt(&event.event_id, accepted));
    }
    state.database.mark_processing(&accepted.turn_id)?;
    let extension =
        command_arguments(&event.content).zip(config.value.interface_extension(interface_id)?);
    let result = if let Some((arguments, extension)) = extension {
        run_extension(
            state,
            config,
            extension,
            &accepted.turn_id,
            &event,
            arguments,
        )
        .await
        .map_err(TurnFailure::Interrupted)
    } else {
        run_runtime(
            state,
            config,
            RuntimeSelection {
                interface_id,
                provider: runtime,
                continuation,
            },
            &accepted.turn_id,
            &event,
            resources,
        )
        .await
    };
    match result {
        Ok(response) => {
            state.database.complete_turn(
                &accepted.turn_id,
                interface_id,
                &event.delivery_target_id,
                &event.feedback_target_id,
                &response,
            )?;
            Ok(fresh_receipt(event.event_id, accepted.turn_id, "completed"))
        }
        Err(TurnFailure::RuntimeUncertain(failure)) => {
            warn!(
                turn_id = %accepted.turn_id,
                error = %failure,
                "runtime outcome uncertain; awaiting durable evidence"
            );
            Ok(fresh_receipt(
                event.event_id,
                accepted.turn_id,
                "processing",
            ))
        }
        Err(TurnFailure::Interrupted(failure)) => {
            let message = failure.to_string();
            state
                .database
                .fail_turn(&accepted.turn_id, "interrupted", &message)?;
            warn!(turn_id = %accepted.turn_id, error = %message, "turn interrupted");
            Ok(fresh_receipt(
                event.event_id,
                accepted.turn_id,
                "interrupted",
            ))
        }
    }
}

struct RuntimeSelection<'a> {
    interface_id: &'a str,
    provider: &'a Provider,
    continuation: Option<Continuation>,
}

enum TurnFailure {
    Interrupted(anyhow::Error),
    RuntimeUncertain(anyhow::Error),
}

impl From<anyhow::Error> for TurnFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::Interrupted(error)
    }
}

async fn run_runtime(
    state: &AppState,
    config: &LoadedConfig,
    selected: RuntimeSelection<'_>,
    turn_id: &str,
    event: &ExternalEvent,
    resources: ResourceSnapshot,
) -> std::result::Result<RuntimeTurnResponse, TurnFailure> {
    if !selected.provider.enabled {
        return Err(TurnFailure::Interrupted(anyhow!(
            "selected runtime provider is disabled"
        )));
    }
    let capability = config
        .value
        .agent
        .capability_provider
        .as_deref()
        .map(|id| config.provider(id, ProviderRole::Capability))
        .transpose()?
        .filter(|provider| provider.enabled)
        .map(|provider| -> Result<CapabilitySelection> {
            Ok(CapabilitySelection {
                provider: selected_provider(provider),
                token: config.credential(&provider.id)?,
            })
        })
        .transpose()?;
    let request = RuntimeTurnRequest {
        correlation_id: turn_id.to_owned(),
        interface_id: selected.interface_id.to_owned(),
        agent_id: event.agent_id.clone(),
        audience_id: event.audience_id.clone(),
        input: event.content.clone(),
        continuation_ref: selected.continuation.map(|value| value.native_ref),
        resources,
        capability,
    };
    let response = state
        .client
        .post(format!("{}/v1/turns", selected.provider.url))
        .bearer_auth(config.credential(&selected.provider.id)?)
        .json(&request)
        .send()
        .await
        .map_err(|error| {
            let interrupted = error.is_connect() || error.is_builder();
            let error = anyhow::Error::new(error).context("contact runtime provider");
            if interrupted {
                TurnFailure::Interrupted(error)
            } else {
                TurnFailure::RuntimeUncertain(error)
            }
        })?;
    let status = response.status();
    let response = response.error_for_status().map_err(|error| {
        let error = anyhow::Error::new(error).context("runtime provider rejected turn");
        if status.is_server_error() {
            TurnFailure::RuntimeUncertain(error)
        } else {
            TurnFailure::Interrupted(error)
        }
    })?;
    let response = response
        .json::<RuntimeTurnResponse>()
        .await
        .map_err(|error| {
            TurnFailure::RuntimeUncertain(
                anyhow::Error::new(error).context("decode runtime response"),
            )
        })?;
    if response.correlation_id != turn_id {
        return Err(TurnFailure::RuntimeUncertain(anyhow!(
            "runtime returned a mismatched correlation ID"
        )));
    }
    state.database.save_continuation(
        selected.interface_id,
        &event.agent_id,
        &event.audience_id,
        &selected.provider.id,
        &response.continuation_ref,
    )?;
    Ok(response)
}

async fn run_extension(
    state: &AppState,
    config: &LoadedConfig,
    extension: &Provider,
    turn_id: &str,
    event: &ExternalEvent,
    arguments: &str,
) -> Result<RuntimeTurnResponse> {
    if !extension.enabled {
        bail!("Carillon command extension is not active");
    }
    let request = ExtensionRequest {
        correlation_id: turn_id.to_owned(),
        command: "carillon".to_owned(),
        arguments: arguments.to_owned(),
        audience_id: event.audience_id.clone(),
        user_id: event.user_id.clone(),
    };
    let response = state
        .client
        .post(format!("{}/v1/invoke", extension.url))
        .bearer_auth(config.credential(&extension.id)?)
        .json(&request)
        .send()
        .await
        .context("contact interface extension")?
        .error_for_status()
        .context("interface extension rejected command")?
        .json::<ExtensionResponse>()
        .await
        .context("decode interface extension response")?;
    Ok(RuntimeTurnResponse {
        correlation_id: turn_id.to_owned(),
        output: response.content,
        continuation_ref: format!("extension:{turn_id}"),
        native: json!({ "owner": extension.id, "evidence": response.native }),
        capability_activity: Vec::new(),
    })
}

async fn claim_delivery(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Option<Delivery>>, ApiError> {
    let config = LoadedConfig::load(&state.config_path)?;
    let interface_id = authorize_interface(&headers, &config)?;
    let owner = adapter_instance(&headers)?;
    Ok(Json(state.database.claim_delivery(&interface_id, owner)?))
}

async fn reconcile_delivery_claims(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DeliveryReconciliation>, ApiError> {
    let config = LoadedConfig::load(&state.config_path)?;
    let interface_id = authorize_interface(&headers, &config)?;
    let owner = adapter_instance(&headers)?;
    let feedback_target_ids = state
        .database
        .reconcile_delivery_claims(&interface_id, owner)?;
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
    let config = LoadedConfig::load(&state.config_path)?;
    let interface_id = authorize_interface(&headers, &config)?;
    let owner = adapter_instance(&headers)?;
    let outcome = match ack.outcome {
        DeliveryOutcome::Delivered => "delivered",
        DeliveryOutcome::Uncertain => "uncertain",
    };
    state.database.acknowledge_delivery(
        id,
        &interface_id,
        owner,
        outcome,
        ack.native_ref.as_deref(),
    )?;
    Ok(StatusCode::NO_CONTENT)
}

async fn inspect_turn(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<TurnInspection>, ApiError> {
    let config = LoadedConfig::load(&state.config_path)?;
    authorize(&headers, &config.value.core.operator_token_hash)?;
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
    let config = LoadedConfig::load(&state.config_path)?;
    authorize(&headers, &config.value.core.operator_token_hash)?;
    Ok(Json(state.database.recent_turns(
        query.event_id.as_deref(),
        query.limit.unwrap_or(20),
    )?))
}

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<StatusReport>, ApiError> {
    let config = LoadedConfig::load(&state.config_path)?;
    authorize(&headers, &config.value.core.operator_token_hash)?;
    let providers = provider_health(&state.client, &config).await;
    let runtime = agent_runtime(&config)?;
    let execution_profile = if runtime.enabled {
        runtime_execution_profiles_request(&state.client, &config, runtime, false)
            .await
            .ok()
            .and_then(|profiles| profiles.selected)
    } else {
        None
    };
    Ok(Json(StatusReport {
        release: config.value.release.clone(),
        agent_id: config.value.agent.id.clone(),
        providers,
        turns: state.database.state_counts("turns")?,
        deliveries: state.database.state_counts("deliveries")?,
        latest_backup: state.database.latest_backup()?,
        recent_turns: state.database.recent_turns(None, 10)?,
        execution_profile,
    }))
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ExecutionProfilesQuery {
    interface_id: Option<String>,
    audience_id: Option<String>,
}

async fn execution_profiles(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ExecutionProfilesQuery>,
) -> Result<Json<RuntimeExecutionProfiles>, ApiError> {
    let config = LoadedConfig::load(&state.config_path)?;
    let access = authorize_model_access(&headers, &config)?;
    let runtime = profile_runtime(
        &state.database,
        &config,
        &access,
        query.interface_id.as_deref(),
        query.audience_id.as_deref(),
    )?;
    Ok(Json(
        runtime_execution_profiles_request(&state.client, &config, runtime, false).await?,
    ))
}

async fn refresh_execution_profiles(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ExecutionProfilesQuery>,
) -> Result<Json<RuntimeExecutionProfiles>, ApiError> {
    let config = LoadedConfig::load(&state.config_path)?;
    let access = authorize_model_access(&headers, &config)?;
    let runtime = profile_runtime(
        &state.database,
        &config,
        &access,
        query.interface_id.as_deref(),
        query.audience_id.as_deref(),
    )?;
    Ok(Json(
        runtime_execution_profiles_request(&state.client, &config, runtime, true).await?,
    ))
}

async fn apply_execution_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(selection): Json<ExecutionProfileSelection>,
) -> Result<Json<ExecutionProfileApplication>, ApiError> {
    let config = LoadedConfig::load(&state.config_path)?;
    let access = authorize_model_access(&headers, &config)?;
    let current = matches!(selection.scope, ExecutionProfileScope::CurrentConversation);
    let _turn_guard = if current {
        Some(state.turns.lock().await)
    } else {
        None
    };
    let audience_id = if current {
        Some(
            selection
                .audience_id
                .as_deref()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    ApiError::from(anyhow!("current conversation scope needs an audience"))
                })?
                .to_owned(),
        )
    } else {
        None
    };
    let interface_id = if current {
        Some(access.interface_id(&config, selection.interface_id.as_deref())?)
    } else {
        None
    };
    if let Some(id) = interface_id.as_deref() {
        config.provider(id, ProviderRole::Interface)?;
    }
    let continuation = match (interface_id.as_deref(), audience_id.as_deref()) {
        (Some(interface), Some(audience)) => {
            state
                .database
                .continuation(interface, &config.value.agent.id, audience)?
        }
        _ => None,
    };
    let runtime_id = continuation
        .as_ref()
        .map_or(config.value.agent.runtime_provider.as_str(), |value| {
            value.runtime.as_str()
        });
    let runtime = config.provider(runtime_id, ProviderRole::Runtime)?;
    let resources = if current {
        Some(materialize_resources(
            &config.value.agent.resource_root,
            &config.value.data_dir.join("resource-snapshots"),
        )?)
    } else {
        None
    };
    let runtime_selection = RuntimeExecutionProfileSelection {
        key: selection.key,
        reasoning: selection.reasoning,
        service_tier: selection.service_tier,
        scope: selection.scope,
        continuation_ref: continuation.map(|value| value.native_ref),
        interface_id: interface_id.clone(),
        agent_id: config.value.agent.id.clone(),
        audience_id: audience_id.clone(),
        resources,
    };
    let applied =
        runtime_execution_profile_selection(&state.client, &config, runtime, &runtime_selection)
            .await?;
    if let (Some(interface), Some(audience), Some(continuation)) = (
        interface_id.as_deref(),
        audience_id.as_deref(),
        applied.continuation_ref.as_deref(),
    ) {
        state.database.save_continuation(
            interface,
            &config.value.agent.id,
            audience,
            &runtime.id,
            continuation,
        )?;
    }
    Ok(Json(applied))
}

async fn runtime_execution_profiles_request(
    client: &Client,
    config: &LoadedConfig,
    runtime: &Provider,
    refresh: bool,
) -> Result<RuntimeExecutionProfiles> {
    if !runtime.enabled {
        bail!("selected runtime provider is disabled");
    }
    let endpoint = if refresh {
        "execution-profiles/refresh"
    } else {
        "execution-profiles"
    };
    client
        .request(
            if refresh {
                reqwest::Method::POST
            } else {
                reqwest::Method::GET
            },
            format!("{}/v1/{endpoint}", runtime.url),
        )
        .bearer_auth(config.credential(&runtime.id)?)
        .send()
        .await
        .context("contact runtime execution-profile service")?
        .error_for_status()
        .context("runtime provider rejected execution-profile catalog operation")?
        .json()
        .await
        .context("decode runtime execution profiles")
}

async fn runtime_execution_profile_selection(
    client: &Client,
    config: &LoadedConfig,
    runtime: &Provider,
    selection: &RuntimeExecutionProfileSelection,
) -> Result<ExecutionProfileApplication> {
    if !runtime.enabled {
        bail!("selected runtime provider is disabled");
    }
    client
        .put(format!("{}/v1/execution-profile", runtime.url))
        .bearer_auth(config.credential(&runtime.id)?)
        .json(selection)
        .send()
        .await
        .context("contact runtime execution-profile service")?
        .error_for_status()
        .context("runtime provider rejected execution-profile selection")?
        .json()
        .await
        .context("decode applied execution profile")
}

async fn provider_health(client: &Client, config: &LoadedConfig) -> Vec<Health> {
    let mut reports = vec![Health {
        id: "core".to_owned(),
        role: "core".to_owned(),
        version: RELEASE_VERSION.to_owned(),
        status: "ready".to_owned(),
    }];
    for provider in &config.value.providers {
        if provider.enabled {
            let token = config.credential(&provider.id).ok();
            reports.push(
                fetch_health(
                    client,
                    &format!("{}/health", provider.url),
                    token.as_deref(),
                    &provider.id,
                    provider.role.as_str(),
                    &provider.version,
                )
                .await,
            );
        } else {
            reports.push(Health {
                id: provider.id.clone(),
                role: provider.role.as_str().to_owned(),
                version: provider.version.clone(),
                status: "disabled".to_owned(),
            });
        }
    }
    reports
}

async fn fetch_health(
    client: &Client,
    url: &str,
    token: Option<&str>,
    id: &str,
    role: &str,
    version: &str,
) -> Health {
    let mut request = client.get(url);
    if let Some(value) = token {
        request = request.bearer_auth(value);
    }
    match request.send().await {
        Ok(response) => match response.error_for_status() {
            Ok(response) => response
                .json::<Health>()
                .await
                .unwrap_or_else(|_| unavailable(id, role, version)),
            Err(_) => unavailable(id, role, version),
        },
        Err(_) => unavailable(id, role, version),
    }
}

fn unavailable(id: &str, role: &str, version: &str) -> Health {
    Health {
        id: id.to_owned(),
        role: role.to_owned(),
        version: version.to_owned(),
        status: "unavailable".to_owned(),
    }
}

async fn reconcile_pending_turns(state: &AppState) -> Result<usize> {
    let config = LoadedConfig::load(&state.config_path)?;
    for pending in state.database.pending_turns()? {
        let runtime = match config.provider(&pending.runtime, ProviderRole::Runtime) {
            Ok(runtime) if runtime.enabled => runtime,
            Ok(_) => {
                state.database.fail_turn(
                    &pending.turn,
                    "interrupted",
                    "runtime provider disabled during recovery",
                )?;
                continue;
            }
            Err(failure) => {
                state.database.fail_turn(
                    &pending.turn,
                    "interrupted",
                    &format!("runtime provider unavailable during recovery: {failure}"),
                )?;
                continue;
            }
        };
        let evidence = state
            .client
            .get(format!("{}/v1/evidence/{}", runtime.url, pending.turn))
            .bearer_auth(config.credential(&runtime.id)?)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        let recovered = match evidence {
            Ok(response) if response.status() == StatusCode::OK => {
                response.json::<RuntimeEvidence>().await.ok()
            }
            Ok(response) if response.status() == StatusCode::NOT_FOUND => {
                state.database.fail_turn(
                    &pending.turn,
                    "interrupted",
                    "runtime has no durable correlation evidence",
                )?;
                None
            }
            _ => None,
        };
        match recovered {
            Some(RuntimeEvidence {
                state: evidence_state,
                result: Some(result),
                ..
            }) if evidence_state == "completed" => {
                state.database.save_continuation(
                    &pending.interface,
                    &pending.agent,
                    &pending.audience,
                    &pending.runtime,
                    &result.continuation_ref,
                )?;
                state.database.complete_turn(
                    &pending.turn,
                    &pending.interface,
                    &pending.target,
                    &pending.feedback_target,
                    &result,
                )?;
                info!(turn_id = %pending.turn, "recovered completed runtime work");
            }
            Some(RuntimeEvidence {
                state: evidence_state,
                error,
                ..
            }) if evidence_state == "interrupted" => {
                state.database.fail_turn(
                    &pending.turn,
                    "interrupted",
                    error
                        .as_deref()
                        .unwrap_or("runtime reported interrupted work"),
                )?;
            }
            Some(RuntimeEvidence {
                state: evidence_state,
                result: None,
                ..
            }) if evidence_state == "completed" => {
                state.database.fail_turn(
                    &pending.turn,
                    "interrupted",
                    "runtime completion evidence lacked a result",
                )?;
            }
            _ => {}
        }
    }
    Ok(state.database.pending_turns()?.len())
}

fn authorize(headers: &HeaderMap, expected_hash: &str) -> Result<(), ApiError> {
    let candidate = bearer_token(headers)?;
    if !token_matches(expected_hash, candidate) {
        return Err(ApiError::forbidden("provider authentication failed"));
    }
    Ok(())
}

fn authorize_interface(headers: &HeaderMap, config: &LoadedConfig) -> Result<String, ApiError> {
    let id = provider_instance(headers)?.to_owned();
    let provider = config
        .provider(&id, ProviderRole::Interface)
        .map_err(|_| ApiError::forbidden("interface provider is not configured"))?;
    if !provider.enabled {
        return Err(ApiError::forbidden("interface provider is disabled"));
    }
    authorize(headers, &provider.token_hash)?;
    Ok(id)
}

enum ModelAccess {
    Operator,
    Interface(String),
}

impl ModelAccess {
    fn interface_id(
        &self,
        config: &LoadedConfig,
        requested: Option<&str>,
    ) -> Result<String, ApiError> {
        match self {
            Self::Operator => requested
                .filter(|value| !value.is_empty())
                .or_else(|| config.value.sole_enabled_interface())
                .map(str::to_owned)
                .ok_or_else(|| {
                    ApiError::from(anyhow!(
                        "current conversation scope needs an explicit interface provider when configuration does not contain exactly one enabled interface"
                    ))
                }),
            Self::Interface(id) => {
                if requested.is_some_and(|requested| requested != id) {
                    return Err(ApiError::forbidden(
                        "interface provider cannot select another interface conversation",
                    ));
                }
                Ok(id.clone())
            }
        }
    }
}

fn authorize_model_access(
    headers: &HeaderMap,
    config: &LoadedConfig,
) -> Result<ModelAccess, ApiError> {
    let candidate = bearer_token(headers)?;
    if token_matches(&config.value.core.operator_token_hash, candidate) {
        return Ok(ModelAccess::Operator);
    }
    authorize_interface(headers, config).map(ModelAccess::Interface)
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::forbidden("provider authentication required"))
}

fn profile_runtime<'a>(
    database: &Database,
    config: &'a LoadedConfig,
    access: &ModelAccess,
    interface_id: Option<&str>,
    audience_id: Option<&str>,
) -> Result<&'a Provider, ApiError> {
    match (interface_id, audience_id) {
        (None, None) => agent_runtime(config).map_err(Into::into),
        (Some(interface_id), Some(audience_id)) if !audience_id.is_empty() => {
            let interface_id = access.interface_id(config, Some(interface_id))?;
            config.provider(&interface_id, ProviderRole::Interface)?;
            let continuation =
                database.continuation(&interface_id, &config.value.agent.id, audience_id)?;
            let runtime_id = continuation.as_ref().map_or(
                config.value.agent.runtime_provider.as_str(),
                |continuation| continuation.runtime.as_str(),
            );
            config
                .provider(runtime_id, ProviderRole::Runtime)
                .map_err(Into::into)
        }
        _ => Err(ApiError::from(anyhow!(
            "conversation-scoped execution profiles need both interface and audience"
        ))),
    }
}

fn admit(event: &ExternalEvent) -> Result<(), ApiError> {
    if event.event_id.is_empty()
        || event.agent_id.is_empty()
        || event.user_id.is_empty()
        || event.audience_id.is_empty()
        || event.delivery_target_id.is_empty()
        || event.feedback_target_id.is_empty()
        || event.content.trim().is_empty()
    {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "event, agent, audience, target, user, and content are required".to_owned(),
        });
    }
    Ok(())
}

fn receipt(event_id: &str, accepted: AcceptedTurn) -> EventReceipt {
    EventReceipt {
        event_id: event_id.to_owned(),
        turn_id: accepted.turn_id,
        state: accepted.state,
        duplicate: true,
    }
}

fn fresh_receipt(event_id: String, turn_id: String, state: &str) -> EventReceipt {
    EventReceipt {
        event_id,
        turn_id,
        state: state.to_owned(),
        duplicate: false,
    }
}

fn command_arguments(content: &str) -> Option<&str> {
    content.strip_prefix("!carillon").map(str::trim)
}

fn provider_snapshot(
    config: &LoadedConfig,
    interface: &Provider,
    runtime: &Provider,
) -> Result<Value> {
    let mut selected = vec![
        serde_json::to_value(selected_provider(interface)).unwrap_or(Value::Null),
        serde_json::to_value(selected_provider(runtime)).unwrap_or(Value::Null),
    ];
    if let Some(provider) = config
        .value
        .interface_extension(&interface.id)?
        .filter(|provider| provider.enabled)
    {
        selected.push(serde_json::to_value(selected_provider(provider)).unwrap_or(Value::Null));
    }
    if let Some(provider) =
        config
            .value
            .agent
            .capability_provider
            .as_deref()
            .and_then(|provider_id| {
                config
                    .value
                    .providers
                    .iter()
                    .find(|provider| provider.id == provider_id && provider.enabled)
            })
    {
        selected.push(serde_json::to_value(selected_provider(provider)).unwrap_or(Value::Null));
    }
    Ok(Value::Array(selected))
}

fn selected_provider(provider: &Provider) -> SelectedProvider {
    SelectedProvider {
        id: provider.id.clone(),
        role: provider.role.as_str().to_owned(),
        version: provider.version.clone(),
        url: provider.url.clone(),
    }
}

fn agent_runtime(config: &LoadedConfig) -> Result<&Provider> {
    config.provider(&config.value.agent.runtime_provider, ProviderRole::Runtime)
}

fn materialize_resources(source: &Path, store: &Path) -> Result<ResourceSnapshot> {
    let files = selected_resource_files(source)?;

    let system_sha256 = hash_bytes(files.get("SYSTEM.md").map_or(&[], Vec::as_slice));
    let context_sha256 = hash_bytes(files.get("AGENTS.md").map_or(&[], Vec::as_slice));
    let settings_sha256 = hash_bytes(files.get("settings.json").map_or(&[], Vec::as_slice));
    let extensions_sha256 = hash_entries(&files, "extensions/");
    let skills_sha256 = hash_entries(&files, "skills/");
    let fingerprint = hash_bytes(
        format!(
            "{system_sha256}{context_sha256}{settings_sha256}{extensions_sha256}{skills_sha256}"
        )
        .as_bytes(),
    );
    let destination = store.join(fingerprint);
    if destination.exists() {
        if selected_resource_files(&destination)? != files {
            bail!(
                "resource snapshot {} does not match its content address",
                destination.display()
            );
        }
    } else {
        fs::create_dir_all(store)?;
        let temporary = store.join(format!(".{}", uuid::Uuid::now_v7()));
        fs::create_dir(&temporary)?;
        for (relative, bytes) in &files {
            let path = temporary.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, bytes)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o440))?;
        }
        set_snapshot_directory_permissions(&temporary)?;
        fs::rename(temporary, &destination)?;
    }
    Ok(ResourceSnapshot {
        root: destination.display().to_string(),
        system_sha256,
        context_sha256,
        settings_sha256,
        extensions_sha256,
        skills_sha256,
    })
}

fn selected_resource_files(source: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for name in ["SYSTEM.md", "AGENTS.md", "settings.json"] {
        let path = source.join(name);
        if path.exists() {
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_file() {
                bail!("agent resource is not a regular file: {}", path.display());
            }
            files.insert(name.to_owned(), fs::read(&path)?);
        }
    }
    collect_resource_tree(source, &source.join("extensions"), "extensions", &mut files)?;
    collect_resource_tree(source, &source.join("skills"), "skills", &mut files)?;
    Ok(files)
}

fn collect_resource_tree(
    source_root: &Path,
    directory: &Path,
    prefix: &str,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)
        .with_context(|| format!("read resource directory {}", directory.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!(
                "resource symlinks are not supported: {}",
                entry.path().display()
            );
        }
        if file_type.is_dir() {
            collect_resource_tree(source_root, &entry.path(), prefix, files)?;
        } else if file_type.is_file() {
            let relative = entry
                .path()
                .strip_prefix(source_root)?
                .to_string_lossy()
                .into_owned();
            if !relative.starts_with(prefix) {
                bail!("resource escaped selected directory: {relative}");
            }
            files.insert(relative, fs::read(entry.path())?);
        }
    }
    Ok(())
}

fn hash_entries(files: &BTreeMap<String, Vec<u8>>, prefix: &str) -> String {
    let mut digest = Sha256::new();
    for (path, bytes) in files.range(prefix.to_owned()..) {
        if !path.starts_with(prefix) {
            break;
        }
        digest.update(path.strip_prefix(prefix).unwrap_or(path).as_bytes());
        digest.update([0]);
        digest.update(bytes);
        digest.update([0]);
    }
    hex::encode(digest.finalize())
}

fn hash_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn set_snapshot_directory_permissions(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            set_snapshot_directory_permissions(&entry.path())?;
        }
    }
    fs::set_permissions(directory, fs::Permissions::from_mode(0o750))?;
    Ok(())
}

pub async fn fetch_status(config: &LoadedConfig) -> Result<StatusReport> {
    Client::new()
        .get(format!("http://{}/v1/status", config.value.core.listen))
        .bearer_auth(config.credential("operator")?)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .map_err(Into::into)
}

pub async fn fetch_execution_profiles(
    config: &LoadedConfig,
    refresh: bool,
    interface_id: Option<&str>,
    audience_id: Option<&str>,
) -> Result<RuntimeExecutionProfiles> {
    let path = if refresh {
        "/v1/execution-profiles/refresh"
    } else {
        "/v1/execution-profiles"
    };
    let client = Client::new();
    let mut url = reqwest::Url::parse(&format!("http://{}{path}", config.value.core.listen))?;
    if let (Some(interface_id), Some(audience_id)) = (interface_id, audience_id) {
        url.query_pairs_mut()
            .append_pair("interface_id", interface_id)
            .append_pair("audience_id", audience_id);
    }
    let request = if refresh {
        client.post(url)
    } else {
        client.get(url)
    };
    request
        .bearer_auth(config.credential("operator")?)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .map_err(Into::into)
}

pub async fn select_execution_profile(
    config: &LoadedConfig,
    selection: &ExecutionProfileSelection,
) -> Result<ExecutionProfileApplication> {
    Client::new()
        .put(format!(
            "http://{}/v1/execution-profile",
            config.value.core.listen
        ))
        .bearer_auth(config.credential("operator")?)
        .json(selection)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .map_err(Into::into)
}

pub async fn fetch_inspection(config: &LoadedConfig, turn_id: &str) -> Result<TurnInspection> {
    Client::new()
        .get(format!(
            "http://{}/v1/turns/{turn_id}",
            config.value.core.listen
        ))
        .bearer_auth(config.credential("operator")?)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .map_err(Into::into)
}

pub async fn fetch_recent(
    config: &LoadedConfig,
    event_id: Option<&str>,
    limit: u32,
) -> Result<Vec<TurnSummary>> {
    let client = Client::new();
    let mut url = reqwest::Url::parse(&format!("http://{}/v1/turns", config.value.core.listen))?;
    url.query_pairs_mut()
        .append_pair("limit", &limit.to_string());
    if let Some(event_id) = event_id {
        url.query_pairs_mut().append_pair("event_id", event_id);
    }
    let request = client.get(url).bearer_auth(config.credential("operator")?);
    request
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .map_err(Into::into)
}

async fn shutdown() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        error!(%error, "failed to install shutdown signal handler");
    }
}

pub fn serialize_pretty<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string_pretty(value).map_err(|error| anyhow!(error))
}

fn provider_instance(headers: &HeaderMap) -> Result<&str, ApiError> {
    identity_header(
        headers,
        "x-carillon-provider",
        "interface provider identity",
    )
}

fn adapter_instance(headers: &HeaderMap) -> Result<&str, ApiError> {
    identity_header(headers, "x-carillon-instance", "interface process identity")
}

fn identity_header<'a>(
    headers: &'a HeaderMap,
    name: &str,
    description: &str,
) -> Result<&'a str, ApiError> {
    let value = headers
        .get(name)
        .and_then(|header| header.to_str().ok())
        .ok_or_else(|| ApiError::forbidden(format!("{description} required")))?;
    validate_provider_identity(value)
        .map_err(|_| ApiError::forbidden(format!("invalid {description}")))?;
    Ok(value)
}
#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use anyhow::{Context, Result};
    use axum::{
        Json, Router,
        extract::{Path as AxumPath, Query, State},
        http::{HeaderMap, HeaderValue, StatusCode},
        response::{IntoResponse as _, Response},
        routing::{get, post, put},
    };
    use serde_json::json;
    use tempfile::{TempDir, tempdir};
    use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

    use super::{
        AppState, ExecutionProfilesQuery, RecentQuery, apply_execution_profile,
        authorize_interface, execution_profiles, materialize_resources, process_event,
        provider_snapshot, recent_turns, reconcile_pending_turns, refresh_execution_profiles,
    };
    use crate::{
        config::{
            AgentConfig, Config, CoreConfig, DiscordActivation, DiscordConfig, DiscordConversation,
            DiscordRoute, InterfaceExtensionBinding, LoadedConfig, Provider, ProviderRole,
            hash_token,
        },
        db::Database,
        model::{
            ExecutionProfile, ExecutionProfileApplication, ExecutionProfileScope,
            ExecutionProfileSelection, ExtensionRequest, ExtensionResponse, ExternalEvent,
            RuntimeEvidence, RuntimeExecutionProfileModel, RuntimeExecutionProfiles,
            RuntimeTurnRequest, RuntimeTurnResponse, ServiceTier,
        },
    };

    #[derive(Clone)]
    struct TestRuntime {
        id: String,
        requests: Arc<Mutex<Vec<RuntimeTurnRequest>>>,
        evidence: Arc<Mutex<BTreeMap<String, RuntimeEvidence>>>,
        profile_selections: Arc<Mutex<Vec<ExecutionProfileSelection>>>,
        corrupt_response: Arc<AtomicBool>,
    }

    #[derive(Clone)]
    struct TestExtension {
        requests: Arc<Mutex<Vec<ExtensionRequest>>>,
    }

    async fn invoke_test_extension(
        State(state): State<TestExtension>,
        Json(request): Json<ExtensionRequest>,
    ) -> Json<ExtensionResponse> {
        state.requests.lock().await.push(request);
        Json(ExtensionResponse {
            content: "extension reply".to_owned(),
            native: json!({ "extension": true }),
        })
    }

    async fn start_test_extension(
        token: &str,
    ) -> Result<(
        Provider,
        TestExtension,
        JoinHandle<Result<(), std::io::Error>>,
    )> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let state = TestExtension {
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let router = Router::new()
            .route("/v1/invoke", post(invoke_test_extension))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        Ok((
            Provider {
                id: "extension-a".to_owned(),
                role: ProviderRole::InterfaceExtension,
                version: "test".to_owned(),
                url: format!("http://{address}"),
                token_hash: hash_token(token),
                enabled: true,
            },
            state,
            server,
        ))
    }

    async fn execute_test_turn(
        State(state): State<TestRuntime>,
        Json(request): Json<RuntimeTurnRequest>,
    ) -> Response {
        state.requests.lock().await.push(request.clone());
        let response = RuntimeTurnResponse {
            correlation_id: request.correlation_id.clone(),
            output: format!("{}:{}", state.id, request.input),
            continuation_ref: format!(
                "{}:{}:{}",
                state.id, request.interface_id, request.audience_id
            ),
            native: json!({ "owner": state.id }),
            capability_activity: Vec::new(),
        };
        state.evidence.lock().await.insert(
            request.correlation_id.clone(),
            RuntimeEvidence {
                correlation_id: request.correlation_id,
                state: "completed".to_owned(),
                error: None,
                result: Some(response.clone()),
            },
        );
        if state.corrupt_response.load(Ordering::SeqCst) {
            return (StatusCode::OK, "not JSON").into_response();
        }
        Json(response).into_response()
    }

    async fn test_evidence(
        State(state): State<TestRuntime>,
        AxumPath(id): AxumPath<String>,
    ) -> Result<Json<RuntimeEvidence>, StatusCode> {
        state
            .evidence
            .lock()
            .await
            .get(&id)
            .cloned()
            .map(Json)
            .ok_or(StatusCode::NOT_FOUND)
    }

    async fn test_execution_profiles(
        State(state): State<TestRuntime>,
    ) -> Json<RuntimeExecutionProfiles> {
        Json(RuntimeExecutionProfiles {
            models: vec![RuntimeExecutionProfileModel {
                key: format!("{}-profile", state.id),
                provider: state.id.clone(),
                provider_name: state.id.clone(),
                id: "model".to_owned(),
                name: format!("{} model", state.id),
                reasoning: Vec::new(),
                service_tiers: vec![ServiceTier::Standard],
            }],
            selected: None,
        })
    }

    async fn apply_test_execution_profile(
        State(state): State<TestRuntime>,
        Json(selection): Json<ExecutionProfileSelection>,
    ) -> Result<Json<ExecutionProfileApplication>, StatusCode> {
        if selection.key != format!("{}-profile", state.id) {
            return Err(StatusCode::BAD_REQUEST);
        }
        state
            .profile_selections
            .lock()
            .await
            .push(selection.clone());
        let profile = ExecutionProfile {
            model: format!("{}/model", state.id),
            reasoning: selection.reasoning,
            service_tier: selection.service_tier,
        };
        Ok(Json(ExecutionProfileApplication {
            scope: selection.scope,
            requested: profile.clone(),
            effective: profile,
            continuation_ref: matches!(selection.scope, ExecutionProfileScope::CurrentConversation)
                .then(|| {
                    format!(
                        "{}:{}:profile",
                        state.id,
                        selection.audience_id.as_deref().unwrap_or_default()
                    )
                }),
        }))
    }

    async fn start_test_runtime(
        id: &str,
        token: &str,
    ) -> Result<(
        Provider,
        TestRuntime,
        JoinHandle<Result<(), std::io::Error>>,
    )> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let state = TestRuntime {
            id: id.to_owned(),
            requests: Arc::new(Mutex::new(Vec::new())),
            evidence: Arc::new(Mutex::new(BTreeMap::new())),
            profile_selections: Arc::new(Mutex::new(Vec::new())),
            corrupt_response: Arc::new(AtomicBool::new(false)),
        };
        let router = Router::new()
            .route("/v1/turns", post(execute_test_turn))
            .route("/v1/evidence/{id}", get(test_evidence))
            .route("/v1/execution-profiles", get(test_execution_profiles))
            .route(
                "/v1/execution-profiles/refresh",
                post(test_execution_profiles),
            )
            .route("/v1/execution-profile", put(apply_test_execution_profile))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        Ok((
            Provider {
                id: id.to_owned(),
                role: ProviderRole::Runtime,
                version: "test".to_owned(),
                url: format!("http://{address}"),
                token_hash: hash_token(token),
                enabled: true,
            },
            state,
            server,
        ))
    }

    fn interface_provider(id: &str, token: &str) -> Provider {
        Provider {
            id: id.to_owned(),
            role: ProviderRole::Interface,
            version: "test".to_owned(),
            url: "http://127.0.0.1:9".to_owned(),
            token_hash: hash_token(token),
            enabled: true,
        }
    }

    fn event(id: &str, audience: &str, input: &str) -> ExternalEvent {
        ExternalEvent {
            event_id: id.to_owned(),
            agent_id: "default".to_owned(),
            user_id: "user".to_owned(),
            audience_id: audience.to_owned(),
            delivery_target_id: format!("target-{id}"),
            feedback_target_id: format!("feedback-{id}"),
            content: input.to_owned(),
            native: json!({ "test": true }),
        }
    }

    fn interface_headers(id: &'static str, token: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::AUTHORIZATION, HeaderValue::from_static(token));
        headers.insert("x-carillon-provider", HeaderValue::from_static(id));
        headers
    }

    fn operator_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer operator-token"),
        );
        headers
    }

    #[test]
    fn materialized_resources_do_not_change_with_the_source() -> Result<()> {
        let source = tempdir()?;
        let store = tempdir()?;
        fs::write(source.path().join("SYSTEM.md"), "first")?;
        fs::create_dir(source.path().join("skills"))?;
        let first = materialize_resources(source.path(), store.path())?;
        fs::write(source.path().join("SYSTEM.md"), "second")?;
        let second = materialize_resources(source.path(), store.path())?;
        assert_ne!(first.system_sha256, second.system_sha256);
        assert_eq!(first.skills_sha256, second.skills_sha256);
        assert_ne!(first.root, second.root);
        assert_eq!(
            fs::read_to_string(std::path::Path::new(&first.root).join("SYSTEM.md"))?,
            "first"
        );
        Ok(())
    }

    fn test_configuration(
        root: &Path,
        primary_runtime: &Provider,
        alternate_runtime: &Provider,
        first_interface: &Provider,
        second_interface: &Provider,
    ) -> Config {
        Config {
            release: "test".to_owned(),
            data_dir: root.join("state"),
            core: CoreConfig {
                listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
                operator_token_hash: hash_token("operator-token"),
            },
            providers: vec![
                first_interface.clone(),
                second_interface.clone(),
                primary_runtime.clone(),
                alternate_runtime.clone(),
            ],
            discord: Some(DiscordConfig {
                provider: first_interface.id.clone(),
                agent: "default".to_owned(),
                guild_id: "100".to_owned(),
                bot_user_id: "101".to_owned(),
                allowed_user_ids: vec!["102".to_owned()],
                routes: vec![DiscordRoute {
                    channel_id: "103".to_owned(),
                    activation: DiscordActivation::Always,
                    conversation: DiscordConversation::Channel,
                }],
                typing: true,
                reactions: true,
                processing_emoji: "👀".to_owned(),
                delivered_emoji: "✅".to_owned(),
                failed_emoji: "❌".to_owned(),
                uncertain_emoji: "⚠️".to_owned(),
            }),
            agent: AgentConfig {
                id: "default".to_owned(),
                name: "Default".to_owned(),
                resource_root: root.join("agent"),
                workspace: root.to_path_buf(),
                runtime_provider: primary_runtime.id.clone(),
                interface_extensions: Vec::new(),
                capability_provider: None,
            },
        }
    }

    struct ProviderHarness {
        _directory: TempDir,
        config: LoadedConfig,
        state: AppState,
        primary_runtime: Provider,
        first_interface: Provider,
        primary_state: TestRuntime,
        alternate_state: TestRuntime,
        servers: [JoinHandle<Result<(), std::io::Error>>; 2],
    }

    impl ProviderHarness {
        async fn new() -> Result<Self> {
            let directory = tempdir()?;
            let root = directory.path();
            fs::create_dir_all(root.join("agent"))?;
            fs::write(root.join("agent/SYSTEM.md"), "test agent")?;
            fs::create_dir_all(root.join("credentials"))?;
            let (primary_runtime, primary_state, primary_server) =
                start_test_runtime("runtime-a", "runtime-a-token").await?;
            let (alternate_runtime, alternate_state, alternate_server) =
                start_test_runtime("runtime-b", "runtime-b-token").await?;
            let first_interface = interface_provider("interface-a", "interface-a-token");
            let second_interface = interface_provider("interface-b", "interface-b-token");
            let value = test_configuration(
                root,
                &primary_runtime,
                &alternate_runtime,
                &first_interface,
                &second_interface,
            );
            value.validate()?;
            let config_path = root.join("carillon.toml");
            fs::write(&config_path, toml::to_string_pretty(&value)?)?;
            for (id, token) in [
                ("runtime-a", "runtime-a-token"),
                ("runtime-b", "runtime-b-token"),
            ] {
                fs::write(root.join("credentials").join(format!("{id}.token")), token)?;
            }
            let config = LoadedConfig {
                path: config_path.clone(),
                root: root.to_path_buf(),
                value,
            };
            let state = AppState {
                config_path,
                database: Database::open(&root.join("state/carillon.sqlite3"))?,
                client: reqwest::Client::new(),
                turns: Arc::new(Mutex::new(())),
            };
            Ok(Self {
                _directory: directory,
                config,
                state,
                primary_runtime,
                first_interface,
                primary_state,
                alternate_state,
                servers: [primary_server, alternate_server],
            })
        }

        async fn send(&self, interface: &str, id: &str, audience: &str, input: &str) -> Result<()> {
            process_event(
                &self.state,
                &self.config,
                interface,
                event(id, audience, input),
            )
            .await?;
            Ok(())
        }

        fn select_alternate_runtime(&mut self) -> Result<()> {
            self.config.value.agent.runtime_provider = "runtime-b".to_owned();
            fs::write(
                &self.config.path,
                toml::to_string_pretty(&self.config.value)?,
            )?;
            Ok(())
        }
    }

    impl Drop for ProviderHarness {
        fn drop(&mut self) {
            for server in &self.servers {
                server.abort();
            }
        }
    }

    #[tokio::test]
    async fn interface_providers_authenticate_and_own_separate_deliveries() -> Result<()> {
        let mut harness = ProviderHarness::new().await?;
        let first_headers = interface_headers("interface-a", "Bearer interface-a-token");
        let second_headers = interface_headers("interface-b", "Bearer interface-b-token");
        assert_eq!(
            authorize_interface(&first_headers, &harness.config)
                .map_err(|failure| anyhow::anyhow!(failure.message))?,
            "interface-a"
        );
        assert_eq!(
            authorize_interface(&second_headers, &harness.config)
                .map_err(|failure| anyhow::anyhow!(failure.message))?,
            "interface-b"
        );
        let wrong_token = interface_headers("interface-b", "Bearer interface-a-token");
        assert!(authorize_interface(&wrong_token, &harness.config).is_err());

        harness
            .send("interface-a", "first-event", "first-audience", "first")
            .await?;
        assert!(
            harness
                .state
                .database
                .claim_delivery("interface-b", "second-process")?
                .is_none()
        );
        let delivery = harness
            .state
            .database
            .claim_delivery("interface-a", "first-process")?
            .context("first interface delivery missing")?;
        assert_eq!(delivery.content.as_deref(), Some("runtime-a:first"));
        harness.state.database.acknowledge_delivery(
            delivery.id,
            "interface-a",
            "first-process",
            "delivered",
            Some("first-native"),
        )?;

        harness.select_alternate_runtime()?;
        harness
            .send("interface-b", "second-event", "second-audience", "second")
            .await?;
        let delivery = harness
            .state
            .database
            .claim_delivery("interface-b", "second-process")?
            .context("second interface delivery missing")?;
        assert_eq!(delivery.content.as_deref(), Some("runtime-b:second"));
        Ok(())
    }

    #[tokio::test]
    async fn runtime_turns_preserve_interface_identity_for_shared_audiences() -> Result<()> {
        let harness = ProviderHarness::new().await?;
        harness
            .send("interface-a", "shared-a", "shared", "from a")
            .await?;
        harness
            .send("interface-b", "shared-b", "shared", "from b")
            .await?;

        let requests = harness.primary_state.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].agent_id, requests[1].agent_id);
        assert_eq!(requests[0].audience_id, requests[1].audience_id);
        assert_eq!(requests[0].interface_id, "interface-a");
        assert_eq!(requests[1].interface_id, "interface-b");
        assert!(
            requests
                .iter()
                .all(|request| request.continuation_ref.is_none())
        );
        drop(requests);

        let first = harness
            .state
            .database
            .continuation("interface-a", "default", "shared")?
            .context("first interface continuation missing")?;
        let second = harness
            .state
            .database
            .continuation("interface-b", "default", "shared")?
            .context("second interface continuation missing")?;
        assert_ne!(first.native_ref, second.native_ref);
        Ok(())
    }

    #[tokio::test]
    async fn non_discord_interface_runs_without_discord_configuration() -> Result<()> {
        let mut harness = ProviderHarness::new().await?;
        harness.config.value.discord = None;
        harness.config.value.validate()?;
        fs::write(
            &harness.config.path,
            toml::to_string_pretty(&harness.config.value)?,
        )?;
        harness.config = LoadedConfig::load(&harness.config.path)?;
        let discord_failure = harness
            .config
            .discord()
            .err()
            .context("missing Discord configuration was accepted")?;
        assert!(discord_failure.to_string().contains("absent"));

        harness
            .send(
                "interface-a",
                "non-discord-event",
                "non-discord-audience",
                "hello",
            )
            .await?;
        let diagnostics = recent_turns(
            State(harness.state.clone()),
            operator_headers(),
            Query(RecentQuery {
                event_id: Some("non-discord-event".to_owned()),
                limit: Some(1),
            }),
        )
        .await
        .map_err(|failure| anyhow::anyhow!(failure.message))?
        .0;
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].state, "completed");
        assert_eq!(
            harness
                .state
                .database
                .continuation("interface-a", "default", "non-discord-audience")?
                .context("non-Discord continuation missing")?
                .runtime,
            "runtime-a"
        );
        Ok(())
    }

    #[tokio::test]
    async fn interface_extensions_are_isolated_to_their_bound_interface() -> Result<()> {
        let mut harness = ProviderHarness::new().await?;
        let (extension, extension_state, extension_server) =
            start_test_extension("extension-token").await?;
        harness.config.value.providers.push(extension.clone());
        harness
            .config
            .value
            .agent
            .interface_extensions
            .push(InterfaceExtensionBinding {
                interface_provider: "interface-a".to_owned(),
                extension_provider: extension.id.clone(),
            });
        harness.config.value.validate()?;
        fs::write(
            harness.config.root.join("credentials/extension-a.token"),
            "extension-token",
        )?;
        fs::write(
            &harness.config.path,
            toml::to_string_pretty(&harness.config.value)?,
        )?;

        harness
            .send(
                "interface-a",
                "extension-a",
                "audience-a",
                "!carillon status",
            )
            .await?;
        harness
            .send(
                "interface-b",
                "extension-b",
                "audience-b",
                "!carillon status",
            )
            .await?;

        assert_eq!(extension_state.requests.lock().await.len(), 1);
        assert_eq!(
            harness
                .primary_state
                .requests
                .lock()
                .await
                .iter()
                .map(|request| request.input.as_str())
                .collect::<Vec<_>>(),
            ["!carillon status"]
        );
        for (event_id, includes_extension) in [("extension-a", true), ("extension-b", false)] {
            let turn = harness
                .state
                .database
                .recent_turns(Some(event_id), 1)?
                .pop()
                .context("extension isolation turn missing")?;
            let inspection = harness
                .state
                .database
                .inspect_turn(&turn.turn_id)?
                .context("extension isolation inspection missing")?;
            assert_eq!(
                inspection.providers.as_array().is_some_and(|providers| {
                    providers
                        .iter()
                        .any(|provider| provider["id"] == extension.id)
                }),
                includes_extension
            );
        }
        extension_server.abort();
        Ok(())
    }
    #[tokio::test]
    async fn conversation_profile_catalog_and_application_use_the_pinned_runtime() -> Result<()> {
        let mut harness = ProviderHarness::new().await?;
        harness
            .send("interface-a", "profile-pin", "profile-audience", "first")
            .await?;
        harness.select_alternate_runtime()?;
        let query = ExecutionProfilesQuery {
            interface_id: Some("interface-a".to_owned()),
            audience_id: Some("profile-audience".to_owned()),
        };

        let catalog = execution_profiles(
            State(harness.state.clone()),
            operator_headers(),
            Query(query.clone()),
        )
        .await
        .map_err(|failure| anyhow::anyhow!(failure.message))?
        .0;
        assert_eq!(catalog.models[0].provider, "runtime-a");
        let refreshed = refresh_execution_profiles(
            State(harness.state.clone()),
            operator_headers(),
            Query(query),
        )
        .await
        .map_err(|failure| anyhow::anyhow!(failure.message))?
        .0;
        assert_eq!(refreshed.models[0].provider, "runtime-a");

        let applied = apply_execution_profile(
            State(harness.state.clone()),
            operator_headers(),
            Json(ExecutionProfileSelection {
                key: catalog.models[0].key.clone(),
                reasoning: None,
                service_tier: Some(ServiceTier::Standard),
                scope: ExecutionProfileScope::CurrentConversation,
                audience_id: Some("profile-audience".to_owned()),
                interface_id: Some("interface-a".to_owned()),
            }),
        )
        .await
        .map_err(|failure| anyhow::anyhow!(failure.message))?
        .0;
        assert_eq!(applied.effective.model, "runtime-a/model");
        assert_eq!(
            harness.primary_state.profile_selections.lock().await.len(),
            1
        );
        assert!(
            harness
                .alternate_state
                .profile_selections
                .lock()
                .await
                .is_empty()
        );

        let cross_interface = execution_profiles(
            State(harness.state.clone()),
            interface_headers("interface-a", "Bearer interface-a-token"),
            Query(ExecutionProfilesQuery {
                interface_id: Some("interface-b".to_owned()),
                audience_id: Some("profile-audience".to_owned()),
            }),
        )
        .await
        .err()
        .context("interface queried another interface conversation")?;
        assert_eq!(cross_interface.status, StatusCode::FORBIDDEN);
        Ok(())
    }

    #[tokio::test]
    async fn conversations_remain_pinned_when_new_work_selects_another_runtime() -> Result<()> {
        let mut harness = ProviderHarness::new().await?;
        harness
            .send("interface-a", "a-1", "shared", "first")
            .await?;
        harness.select_alternate_runtime()?;
        harness
            .send("interface-a", "a-2", "shared", "continued")
            .await?;
        harness
            .send("interface-a", "a-3", "new", "new conversation")
            .await?;
        harness
            .send("interface-b", "b-1", "shared", "other interface")
            .await?;

        let first_pin = harness
            .state
            .database
            .continuation("interface-a", "default", "shared")?
            .context("first interface continuation missing")?;
        let second_pin = harness
            .state
            .database
            .continuation("interface-b", "default", "shared")?
            .context("second interface continuation missing")?;
        assert_eq!(first_pin.runtime, "runtime-a");
        assert_eq!(second_pin.runtime, "runtime-b");
        let primary_inputs = harness
            .primary_state
            .requests
            .lock()
            .await
            .iter()
            .map(|request| request.input.clone())
            .collect::<Vec<_>>();
        let alternate_inputs = harness
            .alternate_state
            .requests
            .lock()
            .await
            .iter()
            .map(|request| request.input.clone())
            .collect::<Vec<_>>();
        assert_eq!(primary_inputs, ["first", "continued"]);
        assert_eq!(alternate_inputs, ["new conversation", "other interface"]);
        Ok(())
    }

    #[tokio::test]
    async fn runtime_connect_failure_interrupts_immediately() -> Result<()> {
        let harness = ProviderHarness::new().await?;
        harness.servers[0].abort();
        tokio::task::yield_now().await;

        let receipt = process_event(
            &harness.state,
            &harness.config,
            "interface-a",
            event("offline", "conversation", "never submitted"),
        )
        .await?;
        assert_eq!(receipt.state, "interrupted");
        assert!(harness.state.database.pending_turns()?.is_empty());
        let delivery = harness
            .state
            .database
            .claim_delivery("interface-a", "adapter")?
            .context("terminal feedback delivery missing")?;
        assert!(delivery.content.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn runtime_transport_failure_reconciles_completed_evidence() -> Result<()> {
        let harness = ProviderHarness::new().await?;
        harness
            .primary_state
            .corrupt_response
            .store(true, Ordering::SeqCst);

        let receipt = process_event(
            &harness.state,
            &harness.config,
            "interface-a",
            event("ambiguous", "conversation", "completed remotely"),
        )
        .await?;
        assert_eq!(receipt.state, "processing");

        assert_eq!(reconcile_pending_turns(&harness.state).await?, 0);
        let delivery = harness
            .state
            .database
            .claim_delivery("interface-a", "adapter")?
            .context("reconciled delivery missing")?;
        assert_eq!(
            delivery.content.as_deref(),
            Some("runtime-a:completed remotely")
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_reconciliation_publishes_interrupted_feedback() -> Result<()> {
        let harness = ProviderHarness::new().await?;
        harness
            .primary_state
            .corrupt_response
            .store(true, Ordering::SeqCst);

        let receipt = process_event(
            &harness.state,
            &harness.config,
            "interface-a",
            event("interrupted", "conversation", "lost remotely"),
        )
        .await?;
        assert_eq!(receipt.state, "processing");
        harness.primary_state.evidence.lock().await.insert(
            receipt.turn_id.clone(),
            RuntimeEvidence {
                correlation_id: receipt.turn_id.clone(),
                state: "interrupted".to_owned(),
                error: Some("runtime interrupted".to_owned()),
                result: None,
            },
        );

        assert_eq!(reconcile_pending_turns(&harness.state).await?, 0);
        let delivery = harness
            .state
            .database
            .claim_delivery("interface-a", "adapter")?
            .context("terminal feedback delivery missing")?;
        assert!(delivery.content.is_none());
        let inspection = harness
            .state
            .database
            .inspect_turn(&receipt.turn_id)?
            .context("reconciled turn missing")?;
        assert_eq!(inspection.state, "interrupted");
        Ok(())
    }

    #[tokio::test]
    async fn recovery_uses_the_runtime_recorded_on_the_pending_turn() -> Result<()> {
        let mut harness = ProviderHarness::new().await?;
        harness.select_alternate_runtime()?;
        let pending_event = event("recover-a", "restart", "recover");
        let resources = materialize_resources(
            &harness.config.value.agent.resource_root,
            &harness.config.value.data_dir.join("resource-snapshots"),
        )?;
        let accepted = harness.state.database.accept_event(
            "interface-a",
            "default",
            "runtime-a",
            &pending_event,
            &provider_snapshot(
                &harness.config,
                &harness.first_interface,
                &harness.primary_runtime,
            )?,
            &resources,
        )?;
        harness.state.database.mark_processing(&accepted.turn_id)?;
        let recovered_response = RuntimeTurnResponse {
            correlation_id: accepted.turn_id.clone(),
            output: "runtime-a:recovered".to_owned(),
            continuation_ref: "runtime-a:restart".to_owned(),
            native: json!({ "owner": "runtime-a", "recovered": true }),
            capability_activity: Vec::new(),
        };
        harness.primary_state.evidence.lock().await.insert(
            accepted.turn_id.clone(),
            RuntimeEvidence {
                correlation_id: accepted.turn_id.clone(),
                state: "completed".to_owned(),
                error: None,
                result: Some(recovered_response),
            },
        );

        assert_eq!(reconcile_pending_turns(&harness.state).await?, 0);
        let inspection = harness
            .state
            .database
            .inspect_turn(&accepted.turn_id)?
            .context("reconciled turn missing")?;
        assert_eq!(inspection.state, "completed");
        assert_eq!(inspection.output.as_deref(), Some("runtime-a:recovered"));
        Ok(())
    }
}
