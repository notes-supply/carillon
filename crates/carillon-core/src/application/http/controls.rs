//! A person's control of a turn a durable runtime holds, relayed from the
//! interface that submitted it. Core keeps nothing of it: the runtime
//! decides, and says how the turn ended when it settles it.

use axum::{
    Json,
    extract::{Path as AxumPath, State},
    http::HeaderMap,
    response::Response,
};
use carillon_installation::config::ProviderRole;
use carillon_protocol::turns::{RuntimeTurnControl, TurnControl};
use reqwest::Url;

use super::{ApiError, AppState, interface_provider, relay_model_call, runtime_token};

/// Relays a control of the running turn the interface's event became to its
/// runtime, answering as the runtime does: 404 for an event the interface
/// never submitted, 409 for a turn no runtime can control now.
pub(super) async fn control(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(event_id): AxumPath<String>,
    Json(control): Json<TurnControl>,
) -> Result<Response, ApiError> {
    let config = state.config();
    let interface = interface_provider(&headers, &config)?;
    let Some((runtime, turn_id)) = state.core.controlled(&interface.id, &event_id)? else {
        return Err(ApiError::not_found("no such event"));
    };
    let provider = config.provider(&runtime.id, ProviderRole::Runtime)?;
    let token = runtime_token(&config, provider)?;
    let mut url = Url::parse(&provider.url).map_err(anyhow::Error::from)?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("runtime provider URL cannot have a path"))?
        .extend(["v1", "turns", &turn_id, "control"]);
    let relayed = RuntimeTurnControl {
        interface_id: interface.id.clone(),
        action: control.action,
    };
    relay_model_call(state.client.post(url).bearer_auth(token).json(&relayed)).await
}
