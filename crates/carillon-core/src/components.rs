use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
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
use reqwest::Client;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex, time::sleep};
use tracing::{error, info, warn};
use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use twilight_http::Client as DiscordHttp;
use twilight_model::id::{Id, marker::ChannelMarker};
use uuid::Uuid;

use crate::{
    config::{LoadedConfig, read_token_file, token_matches},
    model::{
        CapabilityRequest, CapabilityResponse, Delivery, DeliveryAck, DeliveryOutcome,
        EventReceipt, ExtensionRequest, ExtensionResponse, ExternalEvent, Health, SimulateRequest,
        SimulateResponse,
    },
};

#[derive(Debug)]
struct ServiceError {
    status: StatusCode,
    message: String,
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

#[derive(Clone)]
struct ExtensionState {
    token_hash: String,
    id: String,
    version: String,
}

pub async fn serve_extension(config: LoadedConfig, token_file: &Path) -> Result<()> {
    let component = config.value.interface_extension;
    let token = read_token_file(token_file)?;
    if !token_matches(&component.token_hash, &token) {
        bail!("interface extension token does not match configuration");
    }
    let state = ExtensionState {
        token_hash: component.token_hash,
        id: component.id,
        version: component.version,
    };
    let router = Router::new()
        .route("/health", get(extension_health))
        .route("/v1/invoke", post(invoke_extension))
        .with_state(state);
    serve_component(component.url, router, "interface extension").await
}

async fn extension_health(State(state): State<ExtensionState>) -> Json<Health> {
    Json(Health {
        id: state.id,
        role: "interface-extension".to_owned(),
        version: state.version,
        status: "ready".to_owned(),
    })
}

async fn invoke_extension(
    State(state): State<ExtensionState>,
    headers: HeaderMap,
    Json(request): Json<ExtensionRequest>,
) -> Result<Json<ExtensionResponse>, ServiceError> {
    authorize(&headers, &state.token_hash)?;
    if request.command != "carillon" {
        return Err(ServiceError {
            status: StatusCode::NOT_FOUND,
            message: "unknown command".to_owned(),
        });
    }
    let detail = if request.arguments.is_empty() {
        "ready"
    } else {
        request.arguments.as_str()
    };
    Ok(Json(ExtensionResponse {
        content: format!(
            "Carillon {} ({detail}); guild {}; operator {}",
            state.version, request.guild_id, request.user_id
        ),
        native: json!({ "command": request.command, "arguments": request.arguments, "extension": state.id }),
    }))
}

#[derive(Clone)]
struct CapabilityState {
    token_hash: String,
    id: String,
    version: String,
    path: PathBuf,
    evidence: Arc<Mutex<BTreeMap<String, CapabilityResponse>>>,
}

pub async fn serve_capability(config: LoadedConfig, token_file: &Path) -> Result<()> {
    let component = config.value.capability;
    let token = read_token_file(token_file)?;
    if !token_matches(&component.token_hash, &token) {
        bail!("capability token does not match configuration");
    }
    let state_dir = config.value.data_dir.join("components").join(&component.id);
    fs::create_dir_all(&state_dir)?;
    let path = state_dir.join("evidence.json");
    let evidence = if path.exists() {
        serde_json::from_slice(&fs::read(&path)?)?
    } else {
        BTreeMap::new()
    };
    let state = CapabilityState {
        token_hash: component.token_hash,
        id: component.id,
        version: component.version,
        path,
        evidence: Arc::new(Mutex::new(evidence)),
    };
    let router = Router::new()
        .route("/health", get(capability_health))
        .route("/v1/execute", post(execute_capability))
        .with_state(state);
    serve_component(component.url, router, "capability").await
}

async fn capability_health(State(state): State<CapabilityState>) -> Json<Health> {
    Json(Health {
        id: state.id,
        role: "capability".to_owned(),
        version: state.version,
        status: "ready".to_owned(),
    })
}

async fn execute_capability(
    State(state): State<CapabilityState>,
    headers: HeaderMap,
    Json(request): Json<CapabilityRequest>,
) -> Result<Json<CapabilityResponse>, ServiceError> {
    authorize(&headers, &state.token_hash)?;
    if request.operation != "echo" {
        return Err(ServiceError {
            status: StatusCode::BAD_REQUEST,
            message: "unsupported capability operation".to_owned(),
        });
    }
    let mut evidence = state.evidence.lock().await;
    if let Some(existing) = evidence.get(&request.correlation_id) {
        return Ok(Json(existing.clone()));
    }
    let response = CapabilityResponse {
        output: format!("external echo: {}", request.input),
        native: json!({ "component": state.id, "operation": request.operation, "sequence": evidence.len() + 1 }),
    };
    evidence.insert(request.correlation_id, response.clone());
    persist_json(&state.path, &*evidence)?;
    Ok(Json(response))
}

#[derive(Clone)]
struct DiscordState {
    config_path: PathBuf,
    core_url: String,
    token: String,
    instance_id: String,
    client: Client,
}

pub async fn serve_discord(
    config: LoadedConfig,
    token_file: &Path,
    discord_token_file: Option<&Path>,
    deterministic: bool,
) -> Result<()> {
    let token = read_token_file(token_file)?;
    if !token_matches(&config.value.discord.token_hash, &token) {
        bail!("Discord adapter token does not match configuration");
    }
    let state = DiscordState {
        config_path: config.path.clone(),
        core_url: format!("http://{}", config.value.core.listen),
        token,
        instance_id: Uuid::now_v7().to_string(),
        client: Client::new(),
    };
    loop {
        match reconcile_core_deliveries(&state).await {
            Ok(()) => break,
            Err(failure) => {
                warn!(error = %failure.message, "waiting to reconcile Discord delivery claims");
                sleep(Duration::from_millis(250)).await;
            }
        }
    }
    let router = Router::new()
        .route("/health", get(discord_health))
        .route("/v1/simulate", post(simulate))
        .with_state(state.clone());
    if !deterministic {
        let discord_token_path = discord_token_file
            .context("--discord-token-file is required outside deterministic mode")?;
        let discord_token = read_token_file(discord_token_path)?;
        let gateway_state = state.clone();
        let delivery_state = state.clone();
        let http = Arc::new(DiscordHttp::new(discord_token.clone()));
        tokio::spawn(async move {
            if let Err(failure) = gateway_loop(gateway_state, discord_token).await {
                error!(error = %failure, "Discord gateway stopped");
            }
        });
        tokio::spawn(async move {
            delivery_loop(delivery_state, http).await;
        });
    }
    let listener = TcpListener::bind(config.value.discord.listen).await?;
    info!(listen = %config.value.discord.listen, deterministic, "Discord adapter ready");
    axum::serve(listener, router).await?;
    Ok(())
}

async fn discord_health(State(state): State<DiscordState>) -> Result<Json<Health>, ServiceError> {
    let config = LoadedConfig::load(&state.config_path)?;
    Ok(Json(Health {
        id: config.value.discord.id,
        role: "interface-adapter".to_owned(),
        version: config.value.discord.version,
        status: "ready".to_owned(),
    }))
}

async fn simulate(
    State(state): State<DiscordState>,
    Json(request): Json<SimulateRequest>,
) -> Result<Json<SimulateResponse>, ServiceError> {
    let config = LoadedConfig::load(&state.config_path)?;
    adapter_admit(&config, request.guild_id.as_deref(), &request.user_id)?;
    let event = ExternalEvent {
        event_id: request.event_id,
        guild_id: request.guild_id,
        user_id: request.user_id,
        channel_id: request.channel_id,
        content: request.content,
        native: json!({ "transport": "deterministic-local-discord" }),
    };
    let receipt = submit_event(&state, &event).await?;
    let delivery = if receipt.state == "completed" && !receipt.duplicate {
        let delivery = claim_core_delivery(&state).await?;
        if let Some(value) = &delivery {
            acknowledge_core_delivery(
                &state,
                value.id,
                DeliveryOutcome::Delivered,
                Some(format!("local:{}", event.event_id)),
            )
            .await?;
        }
        delivery
    } else {
        None
    };
    Ok(Json(SimulateResponse { receipt, delivery }))
}

async fn gateway_loop(state: DiscordState, token: String) -> Result<()> {
    let intents = Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT;
    let mut shard = Shard::new(ShardId::ONE, token, intents);
    while let Some(event) = shard.next_event(EventTypeFlags::MESSAGE_CREATE).await {
        let Event::MessageCreate(message) = event.context("receive Discord gateway event")? else {
            continue;
        };
        if message.author.bot {
            continue;
        }
        let config = LoadedConfig::load(&state.config_path)?;
        let guild = message.guild_id.map(|id| id.to_string());
        if adapter_admit(&config, guild.as_deref(), &message.author.id.to_string()).is_err() {
            continue;
        }
        let event = ExternalEvent {
            event_id: message.id.to_string(),
            guild_id: guild,
            user_id: message.author.id.to_string(),
            channel_id: message.channel_id.to_string(),
            content: message.content.clone(),
            native: serde_json::to_value(&*message).unwrap_or(Value::Null),
        };
        if let Err(failure) = submit_event(&state, &event).await {
            warn!(error = %failure.message, "Discord event submission failed");
        }
    }
    Ok(())
}

async fn delivery_loop(state: DiscordState, http: Arc<DiscordHttp>) {
    loop {
        match claim_core_delivery(&state).await {
            Ok(Some(delivery)) => {
                let channel = delivery.channel_id.parse::<Id<ChannelMarker>>();
                match channel {
                    Ok(channel_id) => match http
                        .create_message(channel_id)
                        .content(&delivery.content)
                        .await
                    {
                        Ok(message) => {
                            let native_ref =
                                message.model().await.map(|value| value.id.to_string()).ok();
                            if let Err(failure) = acknowledge_core_delivery(
                                &state,
                                delivery.id,
                                DeliveryOutcome::Delivered,
                                native_ref,
                            )
                            .await
                            {
                                warn!(error = %failure.message, "delivery acknowledgement failed");
                            }
                        }
                        Err(failure) => {
                            warn!(error = %failure, delivery_id = delivery.id, "Discord delivery outcome uncertain");
                            let _ = acknowledge_core_delivery(
                                &state,
                                delivery.id,
                                DeliveryOutcome::Uncertain,
                                None,
                            )
                            .await;
                        }
                    },
                    Err(failure) => {
                        warn!(error = %failure, "invalid Discord channel ID in delivery");
                        let _ = acknowledge_core_delivery(
                            &state,
                            delivery.id,
                            DeliveryOutcome::Uncertain,
                            None,
                        )
                        .await;
                    }
                }
            }
            Ok(None) => sleep(Duration::from_millis(500)).await,
            Err(failure) => {
                warn!(error = %failure.message, "delivery poll failed");
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn submit_event(
    state: &DiscordState,
    event: &ExternalEvent,
) -> Result<EventReceipt, ServiceError> {
    Ok(state
        .client
        .post(format!("{}/v1/events", state.core_url))
        .bearer_auth(&state.token)
        .json(event)
        .send()
        .await
        .context("submit event to core")?
        .error_for_status()
        .context("core rejected event")?
        .json()
        .await
        .context("decode event receipt")?)
}

async fn claim_core_delivery(state: &DiscordState) -> Result<Option<Delivery>, ServiceError> {
    Ok(state
        .client
        .get(format!("{}/v1/deliveries", state.core_url))
        .bearer_auth(&state.token)
        .header("x-carillon-instance", &state.instance_id)
        .send()
        .await
        .context("poll core delivery")?
        .error_for_status()
        .context("core rejected delivery poll")?
        .json()
        .await
        .context("decode core delivery")?)
}

async fn acknowledge_core_delivery(
    state: &DiscordState,
    id: i64,
    outcome: DeliveryOutcome,
    native_ref: Option<String>,
) -> Result<(), ServiceError> {
    state
        .client
        .post(format!("{}/v1/deliveries/{id}/ack", state.core_url))
        .bearer_auth(&state.token)
        .header("x-carillon-instance", &state.instance_id)
        .json(&DeliveryAck {
            native_ref,
            outcome,
        })
        .send()
        .await
        .context("acknowledge core delivery")?
        .error_for_status()
        .context("core rejected delivery acknowledgement")?;
    Ok(())
}

async fn reconcile_core_deliveries(state: &DiscordState) -> Result<(), ServiceError> {
    state
        .client
        .post(format!("{}/v1/deliveries/reconcile", state.core_url))
        .bearer_auth(&state.token)
        .header("x-carillon-instance", &state.instance_id)
        .send()
        .await
        .context("reconcile abandoned delivery claims")?
        .error_for_status()
        .context("core rejected delivery reconciliation")?;
    Ok(())
}

fn adapter_admit(
    config: &LoadedConfig,
    guild_id: Option<&str>,
    user_id: &str,
) -> Result<(), ServiceError> {
    let guild = guild_id.ok_or_else(|| ServiceError {
        status: StatusCode::FORBIDDEN,
        message: "direct messages are not accepted".to_owned(),
    })?;
    if guild != config.value.discord.guild_id {
        return Err(ServiceError {
            status: StatusCode::FORBIDDEN,
            message: "guild is not configured".to_owned(),
        });
    }
    if !config
        .value
        .discord
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

fn authorize(headers: &HeaderMap, expected_hash: &str) -> Result<(), ServiceError> {
    let token = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| ServiceError {
            status: StatusCode::FORBIDDEN,
            message: "component authentication required".to_owned(),
        })?;
    if !token_matches(expected_hash, token) {
        return Err(ServiceError {
            status: StatusCode::FORBIDDEN,
            message: "component authentication failed".to_owned(),
        });
    }
    Ok(())
}

async fn serve_component(url: String, router: Router, name: &str) -> Result<()> {
    let address = url
        .strip_prefix("http://")
        .context("component URL must use http://")?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("bind {name} to {address}"))?;
    info!(%address, component = name, "component ready");
    axum::serve(listener, router).await?;
    Ok(())
}

fn persist_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}
