use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
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
    config::{Component, LoadedConfig, RELEASE_VERSION, token_matches},
    db::{AcceptedTurn, Database},
    model::{
        CapabilitySelection, Delivery, DeliveryAck, DeliveryOutcome, EventReceipt,
        ExtensionRequest, ExtensionResponse, ExternalEvent, Health, ResourceSnapshot,
        RuntimeEvidence, RuntimeModelSelection, RuntimeModels, RuntimeTurnRequest,
        RuntimeTurnResponse, SelectedComponent, StatusReport, TurnInspection, TurnSummary,
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
    recover(&state).await?;
    if !state.database.pending_turns()?.is_empty() {
        let reconciliation = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                match reconcile_pending_turns(&reconciliation).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(failure) => warn!(error = %failure, "runtime reconciliation retry failed"),
                }
            }
        });
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
        .route("/v1/models", get(models))
        .route("/v1/models/refresh", post(refresh_models))
        .route("/v1/model", put(select_model))
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
    authorize(&headers, &config.value.discord.token_hash)?;
    admit(&config, &event)?;
    let _guard = state.turns.lock().await;
    Ok(Json(process_event(&state, &config, event).await?))
}

async fn process_event(
    state: &AppState,
    config: &LoadedConfig,
    event: ExternalEvent,
) -> Result<EventReceipt> {
    let resources = materialize_resources(
        &config.value.agent.resource_root,
        &config.value.data_dir.join("resource-snapshots"),
    )?;
    let components = component_snapshot(config);
    let accepted = state.database.accept_event(
        &config.value.discord.id,
        &config.value.agent.id,
        &config.value.runtime.id,
        &event,
        &components,
        &resources,
    )?;
    if accepted.duplicate {
        return Ok(receipt(&event.event_id, accepted));
    }
    state.database.mark_processing(&accepted.turn_id)?;
    let result = if let Some(arguments) = command_arguments(&event.content) {
        run_extension(state, config, &accepted.turn_id, &event, arguments).await
    } else {
        run_runtime(state, config, &accepted.turn_id, &event, resources).await
    };
    match result {
        Ok(response) => {
            state.database.complete_turn(
                &accepted.turn_id,
                &config.value.discord.id,
                &event.channel_id,
                &response,
            )?;
            Ok(EventReceipt {
                event_id: event.event_id,
                turn_id: accepted.turn_id,
                state: "completed".to_owned(),
                duplicate: false,
            })
        }
        Err(failure) => {
            let message = failure.to_string();
            state
                .database
                .fail_turn(&accepted.turn_id, "interrupted", &message)?;
            warn!(turn_id = %accepted.turn_id, error = %message, "turn interrupted");
            Ok(EventReceipt {
                event_id: event.event_id,
                turn_id: accepted.turn_id,
                state: "interrupted".to_owned(),
                duplicate: false,
            })
        }
    }
}

async fn run_runtime(
    state: &AppState,
    config: &LoadedConfig,
    turn_id: &str,
    event: &ExternalEvent,
    resources: ResourceSnapshot,
) -> Result<RuntimeTurnResponse> {
    let runtime = &config.value.runtime;
    if !runtime.enabled {
        bail!("selected runtime adapter is disabled");
    }
    let audience = event
        .guild_id
        .as_deref()
        .context("event audience missing")?;
    let continuation =
        state
            .database
            .continuation(&config.value.agent.id, audience, &runtime.id)?;
    let capability = if config.value.capability.enabled {
        Some(CapabilitySelection {
            component: selected_component(&config.value.capability),
            token: config.credential(&config.value.capability.id)?,
        })
    } else {
        None
    };
    let request = RuntimeTurnRequest {
        correlation_id: turn_id.to_owned(),
        agent_id: config.value.agent.id.clone(),
        audience_id: audience.to_owned(),
        input: event.content.clone(),
        continuation_ref: continuation,
        resources,
        capability,
    };
    let token = config.credential(&runtime.id)?;
    let response = state
        .client
        .post(format!("{}/v1/turns", runtime.url))
        .bearer_auth(token)
        .json(&request)
        .send()
        .await
        .context("contact runtime adapter")?
        .error_for_status()
        .context("runtime adapter rejected turn")?
        .json::<RuntimeTurnResponse>()
        .await
        .context("decode runtime response")?;
    if response.correlation_id != turn_id {
        bail!("runtime returned a mismatched correlation ID");
    }
    state.database.save_continuation(
        &config.value.agent.id,
        audience,
        &runtime.id,
        &response.continuation_ref,
    )?;
    Ok(response)
}

async fn run_extension(
    state: &AppState,
    config: &LoadedConfig,
    turn_id: &str,
    event: &ExternalEvent,
    arguments: &str,
) -> Result<RuntimeTurnResponse> {
    let extension = &config.value.interface_extension;
    if !extension.enabled {
        bail!("Carillon command extension is not active");
    }
    let request = ExtensionRequest {
        correlation_id: turn_id.to_owned(),
        command: "carillon".to_owned(),
        arguments: arguments.to_owned(),
        guild_id: event.guild_id.clone().context("command audience missing")?,
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
    authorize(&headers, &config.value.discord.token_hash)?;
    let owner = adapter_instance(&headers)?;
    Ok(Json(
        state
            .database
            .claim_delivery(&config.value.discord.id, owner)?,
    ))
}

async fn reconcile_delivery_claims(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let config = LoadedConfig::load(&state.config_path)?;
    authorize(&headers, &config.value.discord.token_hash)?;
    let owner = adapter_instance(&headers)?;
    let uncertain = state
        .database
        .reconcile_delivery_claims(&config.value.discord.id, owner)?;
    Ok(Json(json!({ "uncertain": uncertain })))
}

async fn acknowledge_delivery(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<i64>,
    Json(ack): Json<DeliveryAck>,
) -> Result<StatusCode, ApiError> {
    let config = LoadedConfig::load(&state.config_path)?;
    authorize(&headers, &config.value.discord.token_hash)?;
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
    let components = component_health(&state.client, &config).await;
    Ok(Json(StatusReport {
        release: config.value.release.clone(),
        agent_id: config.value.agent.id,
        components,
        turns: state.database.state_counts("turns")?,
        deliveries: state.database.state_counts("deliveries")?,
        latest_backup: state.database.latest_backup()?,
        recent_turns: state.database.recent_turns(None, 10)?,
    }))
}

async fn models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<RuntimeModels>, ApiError> {
    let config = LoadedConfig::load(&state.config_path)?;
    authorize_model_access(&headers, &config)?;
    Ok(Json(
        runtime_models_request(&state.client, &config, reqwest::Method::GET, None).await?,
    ))
}

async fn refresh_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<RuntimeModels>, ApiError> {
    let config = LoadedConfig::load(&state.config_path)?;
    authorize(&headers, &config.value.core.operator_token_hash)?;
    Ok(Json(
        runtime_models_request(&state.client, &config, reqwest::Method::POST, None).await?,
    ))
}

async fn select_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(selection): Json<RuntimeModelSelection>,
) -> Result<Json<RuntimeModels>, ApiError> {
    let config = LoadedConfig::load(&state.config_path)?;
    authorize_model_access(&headers, &config)?;
    Ok(Json(
        runtime_models_request(
            &state.client,
            &config,
            reqwest::Method::PUT,
            Some(&selection),
        )
        .await?,
    ))
}

async fn runtime_models_request(
    client: &Client,
    config: &LoadedConfig,
    method: reqwest::Method,
    selection: Option<&RuntimeModelSelection>,
) -> Result<RuntimeModels> {
    let runtime = &config.value.runtime;
    if !runtime.enabled {
        bail!("selected runtime adapter is disabled");
    }
    let endpoint = if method == reqwest::Method::POST {
        "models/refresh"
    } else if method == reqwest::Method::PUT {
        "model"
    } else {
        "models"
    };
    let mut request = client
        .request(method, format!("{}/v1/{endpoint}", runtime.url))
        .bearer_auth(config.credential(&runtime.id)?);
    if let Some(value) = selection {
        request = request.json(value);
    }
    request
        .send()
        .await
        .context("contact runtime model service")?
        .error_for_status()
        .context("runtime adapter rejected model operation")?
        .json()
        .await
        .context("decode runtime models")
}

async fn component_health(client: &Client, config: &LoadedConfig) -> Vec<Health> {
    let mut reports = vec![Health {
        id: "core".to_owned(),
        role: "core".to_owned(),
        version: RELEASE_VERSION.to_owned(),
        status: "ready".to_owned(),
    }];
    let discord_url = format!("http://{}/health", config.value.discord.listen);
    reports.push(
        fetch_health(
            client,
            &discord_url,
            None,
            &config.value.discord.id,
            "interface-adapter",
            &config.value.discord.version,
        )
        .await,
    );
    for component in [
        &config.value.runtime,
        &config.value.interface_extension,
        &config.value.capability,
    ] {
        if component.enabled {
            let token = config.credential(&component.id).ok();
            reports.push(
                fetch_health(
                    client,
                    &format!("{}/health", component.url),
                    token.as_deref(),
                    &component.id,
                    role_name(component),
                    &component.version,
                )
                .await,
            );
        } else {
            reports.push(Health {
                id: component.id.clone(),
                role: role_name(component).to_owned(),
                version: component.version.clone(),
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

async fn recover(state: &AppState) -> Result<()> {
    let uncertain = state.database.recover_deliveries()?;
    if uncertain > 0 {
        warn!(
            uncertain,
            "delivery outcomes became uncertain after restart"
        );
    }
    reconcile_pending_turns(state).await?;
    Ok(())
}

async fn reconcile_pending_turns(state: &AppState) -> Result<usize> {
    let config = LoadedConfig::load(&state.config_path)?;
    let runtime = &config.value.runtime;
    let pending_turns = state.database.pending_turns()?;
    if !runtime.enabled {
        for pending in pending_turns {
            state.database.fail_turn(
                &pending.turn,
                "interrupted",
                "runtime disabled during recovery",
            )?;
        }
        return Ok(0);
    }
    let token = config.credential(&runtime.id)?;
    for pending in pending_turns {
        let evidence = state
            .client
            .get(format!("{}/v1/evidence/{}", runtime.url, pending.turn))
            .bearer_auth(&token)
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
                    &pending.agent,
                    &pending.audience,
                    &pending.runtime,
                    &result.continuation_ref,
                )?;
                state.database.complete_turn(
                    &pending.turn,
                    &pending.interface,
                    &pending.channel,
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
    let candidate = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::forbidden("component authentication required"))?;
    if !token_matches(expected_hash, candidate) {
        return Err(ApiError::forbidden("component authentication failed"));
    }
    Ok(())
}

fn authorize_model_access(headers: &HeaderMap, config: &LoadedConfig) -> Result<(), ApiError> {
    let candidate = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::forbidden("component authentication required"))?;
    if token_matches(&config.value.core.operator_token_hash, candidate)
        || token_matches(&config.value.discord.token_hash, candidate)
    {
        return Ok(());
    }
    Err(ApiError::forbidden("component authentication failed"))
}

fn admit(config: &LoadedConfig, event: &ExternalEvent) -> Result<(), ApiError> {
    let guild = event
        .guild_id
        .as_deref()
        .ok_or_else(|| ApiError::forbidden("direct messages are not accepted"))?;
    if guild != config.value.discord.guild_id {
        return Err(ApiError::forbidden(
            "event is outside the configured private guild",
        ));
    }
    if !config
        .value
        .discord
        .allowed_user_ids
        .iter()
        .any(|user| user == &event.user_id)
    {
        return Err(ApiError::forbidden("user is not allowlisted"));
    }
    if event.event_id.is_empty() || event.channel_id.is_empty() || event.content.trim().is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "event ID, channel ID, and content are required".to_owned(),
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

fn command_arguments(content: &str) -> Option<&str> {
    content.strip_prefix("!carillon").map(str::trim)
}

fn component_snapshot(config: &LoadedConfig) -> Value {
    let mut selected = vec![
        json!({ "id": config.value.discord.id, "role": "interface-adapter", "version": config.value.discord.version }),
        serde_json::to_value(selected_component(&config.value.runtime)).unwrap_or(Value::Null),
    ];
    for component in [&config.value.interface_extension, &config.value.capability] {
        if component.enabled {
            selected
                .push(serde_json::to_value(selected_component(component)).unwrap_or(Value::Null));
        }
    }
    Value::Array(selected)
}

fn selected_component(component: &Component) -> SelectedComponent {
    SelectedComponent {
        id: component.id.clone(),
        role: role_name(component).to_owned(),
        version: component.version.clone(),
        url: component.url.clone(),
    }
}

fn role_name(component: &Component) -> &'static str {
    match component.role {
        crate::config::ComponentRole::RuntimeAdapter => "runtime-adapter",
        crate::config::ComponentRole::InterfaceExtension => "interface-extension",
        crate::config::ComponentRole::Capability => "capability",
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

pub async fn fetch_models(config: &LoadedConfig, refresh: bool) -> Result<RuntimeModels> {
    let path = if refresh {
        "/v1/models/refresh"
    } else {
        "/v1/models"
    };
    let client = Client::new();
    let mut request = if refresh {
        client.post(format!("http://{}{path}", config.value.core.listen))
    } else {
        client.get(format!("http://{}{path}", config.value.core.listen))
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

pub async fn select_runtime_model(
    config: &LoadedConfig,
    selection: &RuntimeModelSelection,
) -> Result<RuntimeModels> {
    Client::new()
        .put(format!("http://{}/v1/model", config.value.core.listen))
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
