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
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::message::{
        Component, MessageFlags,
        component::{ActionRow, Button, ButtonStyle, SelectMenu, SelectMenuOption, SelectMenuType},
    },
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{
        Id,
        marker::{ChannelMarker, GuildMarker},
    },
};
use uuid::Uuid;

use crate::{
    config::{LoadedConfig, read_token_file, token_matches},
    model::{
        CapabilityRequest, CapabilityResponse, Delivery, DeliveryAck, DeliveryOutcome,
        EventReceipt, ExtensionRequest, ExtensionResponse, ExternalEvent, Health,
        RuntimeModelSelection, RuntimeModels, SimulateRequest, SimulateResponse,
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
        let http = Arc::new(DiscordHttp::new(discord_token.clone()));
        let application_id = http.current_user_application().await?.model().await?.id;
        let guild_id = config
            .value
            .discord
            .guild_id
            .parse::<Id<GuildMarker>>()
            .context("configured Discord guild ID is invalid")?;
        http.interaction(application_id)
            .create_guild_command(guild_id)
            .chat_input("model", "Select the model for subsequent agent turns")
            .await?;
        let gateway_state = state.clone();
        let gateway_http = http.clone();
        let delivery_state = state.clone();
        tokio::spawn(async move {
            if let Err(failure) =
                Box::pin(gateway_loop(gateway_state, discord_token, gateway_http)).await
            {
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

async fn gateway_loop(state: DiscordState, token: String, http: Arc<DiscordHttp>) -> Result<()> {
    let intents = Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT;
    let mut shard = Shard::new(ShardId::ONE, token, intents);
    let event_types = EventTypeFlags::MESSAGE_CREATE | EventTypeFlags::INTERACTION_CREATE;
    while let Some(event) = shard.next_event(event_types).await {
        match event.context("receive Discord gateway event")? {
            Event::MessageCreate(message) => {
                if message.author.bot {
                    continue;
                }
                let config = LoadedConfig::load(&state.config_path)?;
                let guild = message.guild_id.map(|id| id.to_string());
                if adapter_admit(&config, guild.as_deref(), &message.author.id.to_string()).is_err()
                {
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
            Event::InteractionCreate(interaction) => {
                if let Err(failure) = handle_model_interaction(&state, &http, interaction.0).await {
                    warn!(error = %failure, "Discord model interaction failed");
                }
            }
            _ => {}
        }
    }
    Ok(())
}

const MODEL_SELECT_ID: &str = "carillon:model:select";
const MODEL_PAGE_PREFIX: &str = "carillon:model:page:";
const MODEL_PAGE_SIZE: usize = 25;

async fn handle_model_interaction(
    state: &DiscordState,
    http: &DiscordHttp,
    interaction: Interaction,
) -> Result<()> {
    let Some(data) = interaction.data.as_ref() else {
        return Ok(());
    };
    let relevant = match data {
        InteractionData::ApplicationCommand(command) => command.name == "model",
        InteractionData::MessageComponent(component) => {
            component.custom_id == MODEL_SELECT_ID
                || component.custom_id.starts_with(MODEL_PAGE_PREFIX)
        }
        _ => false,
    };
    if !relevant {
        return Ok(());
    }
    let component = matches!(data, InteractionData::MessageComponent(_));
    let config = LoadedConfig::load(&state.config_path)?;
    let admitted = interaction.author_id().is_some_and(|user| {
        adapter_admit(
            &config,
            interaction
                .guild_id
                .map(|guild| guild.to_string())
                .as_deref(),
            &user.to_string(),
        )
        .is_ok()
    });
    if !admitted {
        let response = interaction_response(
            if component {
                InteractionResponseType::UpdateMessage
            } else {
                InteractionResponseType::ChannelMessageWithSource
            },
            "This interaction is not authorized.",
            !component,
        );
        http.interaction(interaction.application_id)
            .create_response(interaction.id, &interaction.token, &response)
            .await
            .context("respond to unauthorized Discord model interaction")?;
        return Ok(());
    }

    let deferred = deferred_model_response(component);
    http.interaction(interaction.application_id)
        .create_response(interaction.id, &interaction.token, &deferred)
        .await
        .context("defer Discord model interaction")?;

    let response = match model_interaction_response(state, data).await {
        Ok(response) => response,
        Err(failure) => {
            warn!(error = %failure, "model interaction operation failed");
            interaction_data("Models are currently unavailable.", Vec::new())
        }
    };
    http.interaction(interaction.application_id)
        .update_response(&interaction.token)
        .content(response.content.as_deref())
        .components(response.components.as_deref())
        .await
        .context("update Discord model interaction response")?;
    Ok(())
}

async fn model_interaction_response(
    state: &DiscordState,
    data: &InteractionData,
) -> Result<InteractionResponseData> {
    match data {
        InteractionData::ApplicationCommand(_) => {
            let models = fetch_runtime_models(state).await?;
            Ok(model_picker(&models, 0))
        }
        InteractionData::MessageComponent(component) if component.custom_id == MODEL_SELECT_ID => {
            let key = component
                .values
                .first()
                .context("model selection is empty")?;
            let models = select_runtime_model(state, key).await?;
            let selected = models
                .models
                .iter()
                .find(|model| model.key == models.selected.as_deref().unwrap_or_default())
                .context("runtime did not return the selected model")?;
            Ok(interaction_data(
                &format!(
                    "Selected **{}/{}** for subsequent turns.",
                    selected.provider, selected.id
                ),
                Vec::new(),
            ))
        }
        InteractionData::MessageComponent(component) => {
            let page = component
                .custom_id
                .strip_prefix(MODEL_PAGE_PREFIX)
                .context("unknown model component")?
                .parse::<usize>()
                .context("invalid model page")?;
            let models = fetch_runtime_models(state).await?;
            Ok(model_picker(&models, page))
        }
        _ => bail!("unsupported model interaction"),
    }
}

fn model_picker(models: &RuntimeModels, requested_page: usize) -> InteractionResponseData {
    if models.models.is_empty() {
        return interaction_data(
            "Pi has no available models. Log in through Pi first.",
            Vec::new(),
        );
    }
    let pages = models.models.len().div_ceil(MODEL_PAGE_SIZE);
    let page = requested_page.min(pages - 1);
    let start = page * MODEL_PAGE_SIZE;
    let options = models.models[start..models.models.len().min(start + MODEL_PAGE_SIZE)]
        .iter()
        .map(|model| SelectMenuOption {
            default: models.selected.as_deref() == Some(model.key.as_str()),
            description: Some(limit_discord_text(
                &format!("{}/{}", model.provider, model.id),
                100,
            )),
            emoji: None,
            label: limit_discord_text(&model.name, 100),
            value: model.key.clone(),
        })
        .collect();
    let mut components = vec![Component::ActionRow(ActionRow {
        id: None,
        components: vec![Component::SelectMenu(SelectMenu {
            id: None,
            channel_types: None,
            custom_id: MODEL_SELECT_ID.to_owned(),
            default_values: None,
            disabled: false,
            kind: SelectMenuType::Text,
            max_values: Some(1),
            min_values: Some(1),
            options: Some(options),
            placeholder: Some("Choose a model".to_owned()),
            required: None,
        })],
    })];
    if pages > 1 {
        components.push(Component::ActionRow(ActionRow {
            id: None,
            components: vec![
                model_page_button("Previous", page.saturating_sub(1), page == 0),
                model_page_button("Next", page + 1, page + 1 == pages),
            ],
        }));
    }
    let selected = match models.selected.as_deref() {
        None => "No model is selected.".to_owned(),
        Some(key) => models
            .models
            .iter()
            .find(|model| model.key == key)
            .map_or_else(
                || "The selected model is unavailable.".to_owned(),
                |model| format!("Current model: **{}/{}**.", model.provider, model.id),
            ),
    };
    interaction_data(
        &format!("{selected} Choose a model for subsequent turns."),
        components,
    )
}

fn model_page_button(label: &str, page: usize, disabled: bool) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(format!("{MODEL_PAGE_PREFIX}{page}")),
        disabled,
        emoji: None,
        label: Some(label.to_owned()),
        style: ButtonStyle::Secondary,
        url: None,
        sku_id: None,
    })
}

fn deferred_model_response(component: bool) -> InteractionResponse {
    InteractionResponse {
        kind: if component {
            InteractionResponseType::DeferredUpdateMessage
        } else {
            InteractionResponseType::DeferredChannelMessageWithSource
        },
        data: (!component).then(|| InteractionResponseData {
            flags: Some(MessageFlags::EPHEMERAL),
            ..InteractionResponseData::default()
        }),
    }
}

fn interaction_response(
    kind: InteractionResponseType,
    content: &str,
    ephemeral: bool,
) -> InteractionResponse {
    InteractionResponse {
        kind,
        data: Some(InteractionResponseData {
            flags: ephemeral.then_some(MessageFlags::EPHEMERAL),
            ..interaction_data(content, Vec::new())
        }),
    }
}

fn interaction_data(content: &str, components: Vec<Component>) -> InteractionResponseData {
    InteractionResponseData {
        content: Some(content.to_owned()),
        components: Some(components),
        ..InteractionResponseData::default()
    }
}

fn limit_discord_text(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

async fn fetch_runtime_models(state: &DiscordState) -> Result<RuntimeModels> {
    state
        .client
        .get(format!("{}/v1/models", state.core_url))
        .bearer_auth(&state.token)
        .send()
        .await
        .context("fetch available runtime models")?
        .error_for_status()
        .context("core rejected runtime model listing")?
        .json()
        .await
        .context("decode runtime models")
}

async fn select_runtime_model(state: &DiscordState, key: &str) -> Result<RuntimeModels> {
    state
        .client
        .put(format!("{}/v1/model", state.core_url))
        .bearer_auth(&state.token)
        .json(&RuntimeModelSelection {
            key: key.to_owned(),
        })
        .send()
        .await
        .context("select runtime model")?
        .error_for_status()
        .context("core rejected runtime model selection")?
        .json()
        .await
        .context("decode selected runtime model")
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

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{deferred_model_response, model_picker};
    use crate::model::{RuntimeModel, RuntimeModels};

    #[test]
    fn discord_model_deferral_payloads_match_interaction_kinds() -> serde_json::Result<()> {
        let command = serde_json::to_value(deferred_model_response(false))?;
        assert_eq!(command["type"], 5);
        assert_eq!(command["data"]["flags"], 64);

        let component = serde_json::to_value(deferred_model_response(true))?;
        assert_eq!(component["type"], 6);
        assert_eq!(component["data"], Value::Null);
        Ok(())
    }

    #[test]
    fn discord_model_picker_pages_every_available_model() -> serde_json::Result<()> {
        let models = (0..26)
            .map(|index| RuntimeModel {
                key: format!("key-{index}"),
                provider: "provider".to_owned(),
                id: format!("model-{index}"),
                name: format!("Model {index}"),
            })
            .collect::<Vec<_>>();
        let first_models = RuntimeModels {
            models: models.clone(),
            selected: Some("key-25".to_owned()),
        };
        let first = serde_json::to_value(model_picker(&first_models, 0))?;
        assert_eq!(
            first["components"][0]["components"][0]["options"]
                .as_array()
                .map(Vec::len),
            Some(25)
        );
        assert_eq!(
            first["components"][1]["components"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );

        let second_models = RuntimeModels {
            models,
            selected: Some("key-25".to_owned()),
        };
        let second = serde_json::to_value(model_picker(&second_models, 1))?;
        assert_eq!(
            second["components"][0]["components"][0]["options"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(
            second["components"][0]["components"][0]["options"][0]["default"],
            Value::Bool(true)
        );
        Ok(())
    }
}
