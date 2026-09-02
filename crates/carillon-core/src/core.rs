use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};

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
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::{
    config::{LoadedConfig, Provider, ProviderRole, RELEASE_VERSION, token_matches},
    db::{AcceptedTurn, Database, QueuedTurn},
    model::{
        Delivery, DeliveryAck, DeliveryOutcome, DeliveryReconciliation, EventReceipt,
        ExecutionProfileApplication, ExecutionProfileScope, ExecutionProfileSelection,
        ExternalEvent, Health, ResourceSnapshot, RuntimeExecutionProfileSelection,
        RuntimeExecutionProfiles, RuntimeTurnRequest, RuntimeTurnResponse, SelectedProvider,
        StatusReport, TurnInspection, TurnSummary,
    },
};

#[derive(Clone)]
struct AppState {
    config: LoadedConfig,
    database: Database,
    client: Client,
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
    let database = Database::open(&config.value.data_dir.join("carillon.sqlite3"))?;
    let recovered_deliveries = database.recover_deliveries()?;
    let recovered_turns = database.recover_running_turns()?;
    if recovered_deliveries > 0 {
        warn!(
            recovered_deliveries,
            "delivery outcomes became uncertain after restart"
        );
    }
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
    let runtime_id = config.value.agent.runtime_provider.clone();
    let worker = state.clone();
    tokio::spawn(async move { runtime_worker(worker, runtime_id).await });

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
    let interface = interface_provider(&headers, &state.config)?.clone();
    admit(&event, &state.config)?;
    let runtime = selected_runtime(&state.config)?.clone();
    let resources = materialize_resources(
        &state.config.value.agent.resource_root,
        &state.config.value.data_dir.join("resource-snapshots"),
    )?;
    let providers =
        serde_json::to_value([selected_provider(&interface), selected_provider(&runtime)])
            .map_err(anyhow::Error::from)?;
    let accepted =
        state
            .database
            .accept_event(&interface.id, &runtime.id, &event, &providers, &resources)?;
    Ok(Json(receipt(&event.event_id, accepted)))
}

async fn runtime_worker(state: AppState, runtime_id: String) {
    loop {
        if let Err(failure) = run_next_turn(&state, &runtime_id).await {
            warn!(runtime_id, error = %failure, "runtime worker failed");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn run_next_turn(state: &AppState, runtime_id: &str) -> Result<()> {
    let runtime = state.config.provider(runtime_id, ProviderRole::Runtime)?;
    if !runtime.enabled {
        tokio::time::sleep(Duration::from_secs(1)).await;
        return Ok(());
    }
    let token = state.config.credential(runtime_id)?;
    let Some(turn) = state.database.claim_turn(runtime_id)? else {
        tokio::time::sleep(Duration::from_millis(100)).await;
        return Ok(());
    };
    if let Err(failure) = execute_turn(state, runtime, &token, &turn).await {
        let message = failure.to_string();
        while let Err(persistence_failure) = state.database.mark_uncertain(&turn, &message) {
            warn!(
                turn_id = %turn.turn_id,
                error = %persistence_failure,
                "retrying uncertain turn persistence"
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        warn!(turn_id = %turn.turn_id, error = %message, "runtime outcome is uncertain");
    }
    Ok(())
}

async fn execute_turn(
    state: &AppState,
    runtime: &Provider,
    token: &str,
    turn: &QueuedTurn,
) -> Result<()> {
    let continuation_ref = state.database.continuation(
        &turn.interface_id,
        &turn.agent_id,
        &turn.audience_id,
        &runtime.id,
    )?;
    let request = RuntimeTurnRequest {
        correlation_id: turn.turn_id.clone(),
        interface_id: turn.interface_id.clone(),
        agent_id: turn.agent_id.clone(),
        audience_id: turn.audience_id.clone(),
        input: turn.input.clone(),
        continuation_ref,
        resources: turn.resources.clone(),
    };
    let response = state
        .client
        .post(format!("{}/v1/turns", runtime.url))
        .bearer_auth(token)
        .timeout(Duration::from_mins(30))
        .json(&request)
        .send()
        .await
        .context("contact runtime provider")?
        .error_for_status()
        .context("runtime provider rejected turn")?
        .json::<RuntimeTurnResponse>()
        .await
        .context("decode runtime response")?;
    if response.correlation_id != turn.turn_id {
        bail!("runtime returned a mismatched correlation ID");
    }
    if response.continuation_ref.is_empty() {
        bail!("runtime returned an empty continuation reference");
    }
    state.database.complete_turn(turn, &runtime.id, &response)
}

async fn claim_delivery(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Option<Delivery>>, ApiError> {
    let interface = interface_provider(&headers, &state.config)?;
    let owner = adapter_instance(&headers)?;
    Ok(Json(state.database.claim_delivery(&interface.id, owner)?))
}

async fn reconcile_delivery_claims(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DeliveryReconciliation>, ApiError> {
    let interface = interface_provider(&headers, &state.config)?;
    let owner = adapter_instance(&headers)?;
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
    interface_provider(&headers, &state.config)?;
    let owner = adapter_instance(&headers)?;
    let outcome = match ack.outcome {
        DeliveryOutcome::Delivered => "delivered",
        DeliveryOutcome::Uncertain => "uncertain",
    };
    state
        .database
        .acknowledge_delivery(id, owner, outcome, ack.native_ref.as_deref())?;
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
    let execution_profile = runtime_execution_profiles_request(&state.client, &state.config, false)
        .await
        .ok()
        .and_then(|profiles| profiles.selected);
    Ok(Json(StatusReport {
        release: state.config.value.release.clone(),
        agent_id: state.config.value.agent.id.clone(),
        providers,
        turns: state.database.state_counts("turns")?,
        deliveries: state.database.state_counts("deliveries")?,
        latest_backup: state.database.latest_backup()?,
        recent_turns: state.database.recent_turns(None, 10)?,
        execution_profile,
    }))
}

#[derive(Default, Deserialize)]
struct ProfileQuery {
    interface_id: Option<String>,
    audience_id: Option<String>,
}

async fn execution_profiles(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ProfileQuery>,
) -> Result<Json<RuntimeExecutionProfiles>, ApiError> {
    authorize_profile_access(&headers, &state.config, query.interface_id.as_deref())?;
    if query.interface_id.is_some() != query.audience_id.is_some() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "interface and audience must be supplied together",
        ));
    }
    Ok(Json(
        runtime_execution_profiles_request(&state.client, &state.config, false).await?,
    ))
}

async fn refresh_execution_profiles(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<RuntimeExecutionProfiles>, ApiError> {
    authorize(&headers, &state.config.value.core.operator_token_hash)?;
    Ok(Json(
        runtime_execution_profiles_request(&state.client, &state.config, true).await?,
    ))
}

async fn apply_execution_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(selection): Json<ExecutionProfileSelection>,
) -> Result<Json<ExecutionProfileApplication>, ApiError> {
    authorize_profile_access(&headers, &state.config, selection.interface_id.as_deref())?;
    let runtime = selected_runtime(&state.config)?;
    let current = matches!(selection.scope, ExecutionProfileScope::CurrentConversation);
    let (interface_id, audience_id, continuation_ref, resources) = if current {
        let interface_id = selection
            .interface_id
            .as_deref()
            .or_else(|| state.config.value.sole_enabled_interface())
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "current conversation scope needs an interface",
                )
            })?;
        state
            .config
            .provider(interface_id, ProviderRole::Interface)?;
        let audience_id = selection
            .audience_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "current conversation scope needs an audience",
                )
            })?;
        if state
            .database
            .audience_busy(interface_id, &state.config.value.agent.id, audience_id)?
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "conversation has unfinished work",
            ));
        }
        let continuation = state
            .database
            .continuation(
                interface_id,
                &state.config.value.agent.id,
                audience_id,
                &runtime.id,
            )?
            .ok_or_else(|| {
                ApiError::not_found("current conversation does not have a runtime continuation")
            })?;
        let resources = materialize_resources(
            &state.config.value.agent.resource_root,
            &state.config.value.data_dir.join("resource-snapshots"),
        )?;
        (
            Some(interface_id.to_owned()),
            Some(audience_id.to_owned()),
            Some(continuation),
            Some(resources),
        )
    } else {
        (None, None, None, None)
    };
    let request = RuntimeExecutionProfileSelection {
        key: selection.key,
        reasoning: selection.reasoning,
        service_tier: selection.service_tier,
        scope: selection.scope,
        continuation_ref,
        interface_id,
        agent_id: state.config.value.agent.id.clone(),
        audience_id,
        resources,
    };
    Ok(Json(
        runtime_execution_profile_selection(&state.client, &state.config, &request).await?,
    ))
}

async fn runtime_execution_profiles_request(
    client: &Client,
    config: &LoadedConfig,
    refresh: bool,
) -> Result<RuntimeExecutionProfiles> {
    let runtime = selected_runtime(config)?;
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
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("contact runtime execution-profile service")?
        .error_for_status()
        .context("runtime provider rejected execution-profile operation")?
        .json()
        .await
        .context("decode runtime execution profiles")
}

async fn runtime_execution_profile_selection(
    client: &Client,
    config: &LoadedConfig,
    selection: &RuntimeExecutionProfileSelection,
) -> Result<ExecutionProfileApplication> {
    let runtime = selected_runtime(config)?;
    client
        .put(format!("{}/v1/execution-profile", runtime.url))
        .bearer_auth(config.credential(&runtime.id)?)
        .timeout(Duration::from_secs(5))
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
        if !provider.enabled {
            reports.push(Health {
                id: provider.id.clone(),
                role: provider.role.as_str().to_owned(),
                version: provider.version.clone(),
                status: "disabled".to_owned(),
            });
            continue;
        }
        let token = config.credential(&provider.id).ok();
        reports.push(
            fetch_health(
                client,
                &format!("{}/health", provider.url),
                token.as_deref(),
                provider,
            )
            .await,
        );
    }
    reports
}

async fn fetch_health(
    client: &Client,
    url: &str,
    token: Option<&str>,
    provider: &Provider,
) -> Health {
    let mut request = client.get(url).timeout(Duration::from_secs(2));
    if let Some(value) = token {
        request = request.bearer_auth(value);
    }
    match request.send().await {
        Ok(response) => match response.error_for_status() {
            Ok(response) => response
                .json::<Health>()
                .await
                .unwrap_or_else(|_| unavailable(&provider.id, provider.role, &provider.version)),
            Err(_) => unavailable(&provider.id, provider.role, &provider.version),
        },
        Err(_) => unavailable(&provider.id, provider.role, &provider.version),
    }
}

fn unavailable(id: &str, role: ProviderRole, version: &str) -> Health {
    Health {
        id: id.to_owned(),
        role: role.as_str().to_owned(),
        version: version.to_owned(),
        status: "unavailable".to_owned(),
    }
}

fn selected_runtime(config: &LoadedConfig) -> Result<&Provider> {
    let runtime = config.provider(&config.value.agent.runtime_provider, ProviderRole::Runtime)?;
    if !runtime.enabled {
        bail!("selected runtime provider is disabled");
    }
    Ok(runtime)
}

fn interface_provider<'a>(
    headers: &HeaderMap,
    config: &'a LoadedConfig,
) -> Result<&'a Provider, ApiError> {
    let id = provider_identity(headers)?;
    let provider = config.provider(id, ProviderRole::Interface)?;
    if !provider.enabled {
        return Err(ApiError::forbidden("interface provider is disabled"));
    }
    authorize(headers, &provider.token_hash)?;
    Ok(provider)
}

fn authorize_profile_access(
    headers: &HeaderMap,
    config: &LoadedConfig,
    selected_interface: Option<&str>,
) -> Result<(), ApiError> {
    if bearer(headers)
        .is_some_and(|token| token_matches(&config.value.core.operator_token_hash, token))
    {
        return Ok(());
    }
    let interface = interface_provider(headers, config)?;
    if selected_interface.is_some_and(|selected| selected != interface.id) {
        return Err(ApiError::forbidden(
            "interface provider cannot mutate another interface conversation",
        ));
    }
    Ok(())
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
        .get("x-carillon-provider")
        .and_then(|header| header.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::forbidden("provider identity required"))
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

fn receipt(event_id: &str, accepted: AcceptedTurn) -> EventReceipt {
    EventReceipt {
        event_id: event_id.to_owned(),
        turn_id: accepted.turn_id,
        state: accepted.state,
        duplicate: accepted.duplicate,
    }
}

fn selected_provider(provider: &Provider) -> SelectedProvider {
    SelectedProvider {
        id: provider.id.clone(),
        role: provider.role.as_str().to_owned(),
        version: provider.version.clone(),
        url: provider.url.clone(),
    }
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
    if let (Some(interface), Some(audience)) = (interface_id, audience_id) {
        url.query_pairs_mut()
            .append_pair("interface_id", interface)
            .append_pair("audience_id", audience);
    }
    let mut request = if refresh {
        client.post(url)
    } else {
        client.get(url)
    };
    request = request.bearer_auth(config.credential("operator")?);
    request
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
    client
        .get(url)
        .bearer_auth(config.credential("operator")?)
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

fn adapter_instance(headers: &HeaderMap) -> Result<&str, ApiError> {
    let value = headers
        .get("x-carillon-instance")
        .and_then(|header| header.to_str().ok())
        .ok_or_else(|| ApiError::forbidden("adapter instance identity required"))?;
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ApiError::forbidden("invalid adapter instance identity"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::materialize_resources;

    #[test]
    fn materialized_resources_do_not_change_with_the_source() -> anyhow::Result<()> {
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
}
