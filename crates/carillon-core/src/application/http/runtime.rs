//! What durable runtimes call core for, with their own credential: settling
//! the turns they hold and opening their own. And the dispatcher that hands
//! each of them its queued turns.

use axum::{
    Json,
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
};
use carillon_installation::config::{LoadedConfig, Provider, ProviderRole};
use carillon_protocol::{
    model::EventReceipt,
    turns::{TurnOpening, TurnOutcome},
};
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use super::{ApiError, AppState, authorize, provider_identity, runtime_token};
use crate::{RuntimeClient, WorkerStep, db::Settled};

/// The enabled durable runtime a call names, authenticated by the same
/// credential core presents to it.
fn runtime_provider<'a>(
    headers: &HeaderMap,
    config: &'a LoadedConfig,
) -> Result<&'a Provider, ApiError> {
    let id = provider_identity(headers)?;
    let provider = config
        .provider(id, ProviderRole::Runtime)
        .map_err(|_| ApiError::forbidden("unknown runtime provider"))?;
    if !provider.enabled {
        return Err(ApiError::forbidden("runtime provider is disabled"));
    }
    if !provider.durable {
        return Err(ApiError::forbidden(
            "only a durable runtime settles or opens turns",
        ));
    }
    authorize(headers, &provider.token_hash)?;
    Ok(provider)
}

/// A body read after authentication, so a caller that is refused learns
/// nothing of what core would accept.
fn body<T: DeserializeOwned>(bytes: &Bytes, what: &str) -> Result<T, ApiError> {
    serde_json::from_slice(bytes)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, format!("invalid {what}: {error}")))
}

/// Settles a turn the calling runtime holds: 204 once recorded, now or as
/// the same outcome before; 409 when the turn ended another way; 404 when
/// the runtime holds no such turn.
pub(super) async fn outcome(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(turn_id): AxumPath<String>,
    bytes: Bytes,
) -> Result<StatusCode, ApiError> {
    let runtime = runtime_provider(&headers, &state.config())?.id.clone();
    let outcome: TurnOutcome = body(&bytes, "turn outcome")?;
    let settled = state
        .core
        .blocking(move |core| core.settle(&runtime, &turn_id, outcome))
        .await?;
    match settled {
        Settled::Recorded => Ok(StatusCode::NO_CONTENT),
        Settled::Superseded => Err(ApiError::new(
            StatusCode::CONFLICT,
            "the turn already ended another way",
        )),
        Settled::Unknown => Err(ApiError::not_found("this runtime holds no such turn")),
    }
}

/// Opens a running turn in one of the calling runtime's conversations, for
/// it to settle later, or returns the receipt of the one its key opened.
pub(super) async fn open(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<EventReceipt>, ApiError> {
    let runtime = runtime_provider(&headers, &state.config())?.id.clone();
    let opening: TurnOpening = body(&bytes, "turn opening")?;
    Ok(Json(
        state
            .core
            .blocking(move |core| core.open_turn(&runtime, &opening))
            .await?,
    ))
}

/// One step of a durable runtime's dispatcher: its oldest queued turn,
/// handed over.
pub(super) async fn dispatch_next(
    state: &AppState,
    runtime_id: &str,
    stopping: &CancellationToken,
) -> anyhow::Result<WorkerStep> {
    let config = state.config();
    let runtime = config.provider(runtime_id, ProviderRole::Runtime)?;
    if !runtime.enabled {
        return Ok(WorkerStep::Backoff);
    }
    let client = RuntimeClient::new(
        state.client.clone(),
        runtime.url.clone(),
        runtime_token(&config, runtime)?,
    );
    state
        .core
        .dispatch_next(runtime_id, stopping, async |request| {
            client.submit(&request).await
        })
        .await
}
